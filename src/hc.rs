use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Child, Command, Stdio};

const OWNER_LABEL: &str = "hc-cron-owner";
const OCCURRENCE_LABEL: &str = "hc-cron-occurrence";
const READ_HISTORY_LINES: &str = "1000";
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const TRUNCATED_MARKER: &str = "[earlier terminal output omitted]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub address: String,
    pub occurrence: String,
    pub finished: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub finished_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Hc {
    binary: OsString,
}

impl Hc {
    pub fn new(binary: OsString) -> Self {
        Self { binary }
    }

    pub fn list(&self, owner: &str) -> Result<Vec<Terminal>> {
        let output = self
            .command()
            .arg("ls")
            .arg("--all")
            .arg("--label")
            .arg(format!("{OWNER_LABEL}={owner}"))
            .arg("--json")
            .output()
            .context("failed to run hc ls")?;
        let listing: ListResponse = decode_output(output, "hc ls")?;

        listing
            .terminals
            .into_iter()
            .filter(|brief| local_address(&brief.address))
            .map(|brief| {
                let output = self
                    .command()
                    .arg("get")
                    .arg("--json")
                    .arg(&brief.address)
                    .output()
                    .with_context(|| format!("failed to get terminal {}", brief.address))?;
                let wire: TerminalWire = decode_output(output, "hc get")?;
                anyhow::ensure!(
                    wire.address == brief.address
                        && wire.labels.get(OWNER_LABEL).map(String::as_str) == Some(owner),
                    "terminal {} no longer belongs to this scheduler",
                    brief.address
                );
                wire.try_into()
            })
            .collect()
    }

    pub fn launch(
        &self,
        owner: &str,
        occurrence: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<String> {
        let output = self
            .command()
            .arg("run")
            .arg("--bg")
            .arg("--json")
            .arg("--no-notify")
            .arg("--outlive-parent")
            .arg("--no-parent")
            .arg("--cwd")
            .arg(cwd.as_os_str())
            .arg("--label")
            .arg(format!("{OWNER_LABEL}={owner}"))
            .arg("--label")
            .arg(format!("{OCCURRENCE_LABEL}={occurrence}"))
            .arg("/bin/sh")
            .arg("-c")
            .arg(command)
            .output()
            .context("failed to run hc run")?;
        let launched: LaunchResponse = decode_output(output, "hc run")?;
        anyhow::ensure!(
            local_address(&launched.address),
            "unexpected terminal address from hc run"
        );
        Ok(launched.address)
    }

    pub fn read(&self, address: &str) -> Result<String> {
        let output = self
            .command()
            .arg("read")
            .arg("--json")
            .arg("--lines")
            .arg(READ_HISTORY_LINES)
            .arg(address)
            .output()
            .with_context(|| format!("failed to read terminal {address}"))?;
        let read: ReadResponse = decode_output(output, "hc read")?;
        Ok(read.tail(MAX_OUTPUT_BYTES))
    }

    pub fn remove(&self, address: &str) -> Result<()> {
        let output = self
            .command()
            .arg("rm")
            .arg(address)
            .output()
            .with_context(|| format!("failed to remove terminal {address}"))?;
        check_status(output, "hc rm")
    }

    pub fn events(&self, owner: &str) -> Result<Child> {
        self.command()
            .arg("events")
            .arg("--json")
            .arg("--label")
            .arg(format!("{OWNER_LABEL}={owner}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start hc events")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.stdin(Stdio::null());
        command
    }
}

fn local_address(address: &str) -> bool {
    address.strip_prefix('t').is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    })
}

#[derive(Deserialize)]
struct ListResponse {
    terminals: Vec<TerminalBrief>,
}

#[derive(Deserialize)]
struct TerminalBrief {
    address: String,
}

#[derive(Deserialize)]
struct LaunchResponse {
    address: String,
}

#[derive(Deserialize)]
struct ReadResponse {
    #[serde(default = "default_complete")]
    complete: bool,
    text: String,
}

impl ReadResponse {
    fn tail(self, max_bytes: usize) -> String {
        if self.complete && self.text.len() <= max_bytes {
            return self.text;
        }
        let mut start = self
            .text
            .len()
            .saturating_sub(max_bytes.saturating_sub(TRUNCATED_MARKER.len() + 1));
        while !self.text.is_char_boundary(start) {
            start += 1;
        }
        format!("{TRUNCATED_MARKER}\n{}", &self.text[start..])
    }
}

fn default_complete() -> bool {
    true
}

#[derive(Deserialize)]
struct TerminalWire {
    address: String,
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
    state: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    signal: Option<i32>,
    #[serde(default)]
    finished_ms: Option<i64>,
}

impl TryFrom<TerminalWire> for Terminal {
    type Error = anyhow::Error;

    fn try_from(wire: TerminalWire) -> Result<Self> {
        let finished = match wire.state.as_str() {
            "live" => false,
            "finished" => true,
            other => bail!("unknown hc terminal state: {other}"),
        };
        let occurrence =
            wire.labels.get(OCCURRENCE_LABEL).cloned().ok_or_else(|| {
                anyhow!("terminal {} has no {OCCURRENCE_LABEL} label", wire.address)
            })?;
        Ok(Self {
            address: wire.address,
            occurrence,
            finished,
            exit_code: wire.exit_code,
            signal: wire.signal,
            finished_ms: wire.finished_ms,
        })
    }
}

fn decode_output<T: for<'de> Deserialize<'de>>(
    output: std::process::Output,
    command: &str,
) -> Result<T> {
    check_status_ref(&output, command)?;
    serde_json::from_slice(&output.stdout).with_context(|| format!("invalid JSON from {command}"))
}

fn check_status(output: std::process::Output, command: &str) -> Result<()> {
    check_status_ref(&output, command)
}

fn check_status_ref(output: &std::process::Output, command: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        bail!("{command} exited with {}", output.status);
    }
    bail!("{command} failed ({}): {stderr}", output.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_wire_format_maps_state_and_labels() {
        let wire: TerminalWire = serde_json::from_str(
            r#"{
            "address":"t1", "state":"finished", "labels":{"hc-cron-occurrence":"daily-42"},
            "exit_code":7, "signal":null, "finished_ms":1234, "reason":"exited"
        }"#,
        )
        .unwrap();
        let terminal = Terminal::try_from(wire).unwrap();
        assert_eq!(
            terminal,
            Terminal {
                address: "t1".into(),
                occurrence: "daily-42".into(),
                finished: true,
                exit_code: Some(7),
                signal: None,
                finished_ms: Some(1234),
            }
        );
    }

    #[test]
    fn live_terminal_can_have_no_exit_facts() {
        let wire: TerminalWire = serde_json::from_str(
            r#"{
            "address":"t2", "state":"live", "labels":{"hc-cron-occurrence":"daily-43"},
            "exit_code":null, "signal":null, "finished_ms":null
        }"#,
        )
        .unwrap();
        let terminal = Terminal::try_from(wire).unwrap();
        assert!(!terminal.finished);
        assert_eq!(terminal.exit_code, None);
        assert_eq!(terminal.finished_ms, None);
    }

    #[test]
    fn read_adds_a_marker_for_incomplete_history() {
        let read: ReadResponse =
            serde_json::from_str(r#"{"complete":false,"text":"recent output"}"#).unwrap();
        assert_eq!(read.tail(100), format!("{TRUNCATED_MARKER}\nrecent output"));
        let read = ReadResponse {
            complete: true,
            text: "é".repeat(30),
        };
        let result = read.tail(40);
        assert!(result.len() <= 40 && result.ends_with('é'));
        assert!(!local_address("remote/t7"));
    }

    #[test]
    fn list_and_launch_json_shapes_are_supported() {
        let list: ListResponse = serde_json::from_str(r#"{"hidden_finished":0,"terminals":[{"address":"t1","state":"finished","exit_code":0}]}"#).unwrap();
        assert_eq!(list.terminals[0].address, "t1");
        let launch: LaunchResponse =
            serde_json::from_str(r#"{"address":"t2","agent":"sh","state":"live","turn":0}"#)
                .unwrap();
        assert_eq!(launch.address, "t2");
    }
}
