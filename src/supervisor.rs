use crate::{
    hc::{Hc, Terminal},
    ledger::{self, Completion, Key, Ledger, State},
    schedule::Job,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
    path::Path,
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};

const MINUTE: i64 = 60;
const RECHECK: Duration = Duration::from_secs(10);
const RECONNECT: Duration = Duration::from_secs(5);

trait Core {
    fn list(&self, owner: &str) -> Result<Vec<Terminal>>;
    fn launch(&self, owner: &str, occurrence: &str, command: &str, cwd: &Path) -> Result<String>;
    fn read(&self, address: &str) -> Result<String>;
    fn remove(&self, address: &str) -> Result<()>;
}

impl Core for Hc {
    fn list(&self, owner: &str) -> Result<Vec<Terminal>> {
        self.list(owner)
    }
    fn launch(&self, owner: &str, occurrence: &str, command: &str, cwd: &Path) -> Result<String> {
        self.launch(owner, occurrence, command, cwd)
    }
    fn read(&self, address: &str) -> Result<String> {
        self.read(address)
    }
    fn remove(&self, address: &str) -> Result<()> {
        self.remove(address)
    }
}

fn reconcile(db: &Ledger, core: &impl Core, owner: &str, now: i64) -> Result<HashSet<String>> {
    let terminals = core.list(owner)?;
    let mut seen = HashSet::new();
    for terminal in &terminals {
        ensure!(
            seen.insert(terminal.occurrence.as_str()),
            "multiple terminals claim occurrence {}",
            terminal.occurrence
        );
    }
    let mut busy = HashSet::new();
    for terminal in &terminals {
        let Some(key) = Key::from_label(&terminal.occurrence) else {
            continue;
        };
        let Some(run) = db.get(&key)? else {
            continue; // A label alone does not authorize removal.
        };
        ensure!(
            run.terminal
                .as_deref()
                .is_none_or(|s| s == terminal.address),
            "terminal address changed for occurrence {}",
            key.label()
        );
        if !terminal.finished {
            busy.insert(key.job.clone());
            if matches!(run.state, State::Launching | State::Unknown) && run.output.is_none() {
                db.running(&key, &terminal.address)?;
            }
        } else if run.state != State::Skipped {
            if run.output.is_none() {
                let output = match core.read(&terminal.address) {
                    Ok(output) => output,
                    Err(error) => {
                        eprintln!("collect {}: {error:#}", terminal.address);
                        continue;
                    }
                };
                let (state, detail) = match (terminal.exit_code, terminal.signal) {
                    (Some(0), None) => (State::Succeeded, None),
                    (Some(_), _) | (None, Some(_)) => (State::Failed, None),
                    (None, None) => (State::Unknown, Some("core reported no exit status")),
                };
                db.collect(
                    &key,
                    Completion {
                        state,
                        terminal: &terminal.address,
                        finished: terminal.finished_ms.map_or(now, |ms| ms.div_euclid(1000)),
                        exit_code: terminal.exit_code,
                        signal: terminal.signal,
                        output: &output,
                        detail,
                    },
                )?;
                eprintln!("{} {} ({})", state.as_str(), key.label(), terminal.address);
            }
            // If the process died after persisting output, this is safe to retry.
            if let Err(error) = core.remove(&terminal.address) {
                eprintln!("remove {}: {error:#}", terminal.address);
            }
        }
    }
    for run in db.active()? {
        if !seen.contains(run.key.label().as_str()) {
            db.unknown(&run.key, now, "no matching terminal in core; not retried")?;
        }
    }
    Ok(busy)
}

struct Dispatch<'a> {
    db: &'a Ledger,
    cwd: &'a Path,
    owner: &'a str,
    jobs: &'a [Job],
    zone: Tz,
}

impl Dispatch<'_> {
    fn minute(
        &self,
        core: &impl Core,
        scheduled: i64,
        busy: &HashSet<String>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let instant =
            DateTime::from_timestamp(scheduled, 0).context("invalid scheduled instant")?;
        for job in self
            .jobs
            .iter()
            .filter(|job| job.matches(instant, self.zone))
        {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let key = Key {
                job: job.id.clone(),
                scheduled,
            };
            let overlap = busy.contains(&job.id);
            if self
                .db
                .claim(&key, &job.command, Utc::now().timestamp(), overlap)?
            {
                match core.launch(self.owner, &key.label(), &job.command, self.cwd) {
                    Ok(address) => {
                        self.db.running(&key, &address)?;
                        eprintln!("started {} ({address})", key.label());
                    }
                    Err(error) => {
                        // The request may have reached core. Keep the claim and reconcile.
                        eprintln!("launch {}: {error:#}", key.label());
                    }
                }
            }
        }
        Ok(())
    }
}

// The event contents are not authoritative; every wake triggers a fresh list.
struct Events {
    child: Child,
    closed: Arc<AtomicBool>,
}

impl Events {
    fn start(core: &Hc, owner: &str, wake: SyncSender<()>) -> Result<Self> {
        let mut child = core.events(owner)?;
        let stdout = child.stdout.take().expect("event stdout is piped");
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = Arc::clone(&closed);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                let _ = wake.try_send(());
                line.clear();
            }
            reader_closed.store(true, Ordering::Release);
            let _ = wake.try_send(());
        });
        Ok(Self { child, closed })
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn minute_at(now: i64) -> i64 {
    now.div_euclid(MINUTE) * MINUTE
}

fn report_gap(previous: i64, current: i64) {
    if current - previous > MINUTE {
        eprintln!(
            "skipped {} elapsed schedule minute(s); no catch-up",
            (current - previous) / MINUTE - 1
        );
    }
}

pub fn serve(core: &Hc, state_dir: &Path, cwd: &Path, jobs: &[Job], zone: Tz) -> Result<()> {
    let _lock = ledger::serve_lock(state_dir)?;
    let db = Ledger::open(state_dir)?;
    let owner = db.owner()?;
    let dispatch = Dispatch {
        db: &db,
        cwd,
        owner: &owner,
        jobs,
        zone,
    };
    let (wake, inbox) = mpsc::sync_channel(1);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let wake_on_stop = wake.clone();
    ctrlc::set_handler(move || {
        stop.store(true, Ordering::Relaxed);
        let _ = wake_on_stop.try_send(());
    })
    .context("install shutdown handler")?;

    // The current minute predates this process: only future minutes are due.
    let mut cursor = minute_at(Utc::now().timestamp());
    if let Some(previous) = db.last_minute()? {
        report_gap(previous, cursor + MINUTE);
        cursor = cursor.max(previous);
    }
    db.remember_minute(cursor)?;
    eprintln!(
        "serving {} job(s) in {zone}; state: {}",
        jobs.len(),
        state_dir.display()
    );

    let mut events: Option<Events> = None;
    let mut retry_at = Instant::now();
    let mut last_core_error = None;
    while !stopping.load(Ordering::Relaxed) {
        if events
            .as_ref()
            .is_some_and(|e| e.closed.load(Ordering::Acquire))
        {
            events = None;
        }
        if events.is_none() && Instant::now() >= retry_at {
            match Events::start(core, &owner, wake.clone()) {
                Ok(listener) => events = Some(listener),
                Err(error) => eprintln!("watch core: {error:#}"),
            }
            retry_at = Instant::now() + RECONNECT;
        }
        let now = Utc::now().timestamp();
        let minute = minute_at(now);
        match reconcile(&db, core, &owner, now) {
            Ok(busy) => {
                last_core_error = None;
                // Dispatch the current minute only, even if reconciliation crossed a boundary.
                let current = minute_at(Utc::now().timestamp());
                if current > cursor {
                    report_gap(cursor, current);
                    dispatch.minute(core, current, &busy, &stopping)?;
                    db.remember_minute(current)?;
                    cursor = current;
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if last_core_error.as_ref() != Some(&message) {
                    eprintln!("reconcile core (dispatch paused): {message}");
                    last_core_error = Some(message);
                }
                // No dispatch without a fresh view of existing runs.
                if minute > cursor {
                    report_gap(cursor, minute);
                    eprintln!("skipped schedule minute {minute}: core state unavailable");
                    db.remember_minute(minute)?;
                    cursor = minute;
                }
            }
        }
        let ms = Utc::now().timestamp_millis();
        let until_minute = Duration::from_millis((60_000 - ms.rem_euclid(60_000)) as u64);
        match inbox.recv_timeout(RECHECK.min(until_minute)) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct Fake {
        terminals: RefCell<Vec<Terminal>>,
        launched: Cell<usize>,
        fail_read: Cell<bool>,
        removed: RefCell<Vec<String>>,
    }

    impl Core for Fake {
        fn list(&self, _: &str) -> Result<Vec<Terminal>> {
            Ok(self.terminals.borrow().clone())
        }
        fn launch(&self, _: &str, _: &str, _: &str, _: &Path) -> Result<String> {
            self.launched.set(self.launched.get() + 1);
            anyhow::bail!("ambiguous reply")
        }
        fn read(&self, _: &str) -> Result<String> {
            ensure!(!self.fail_read.get(), "read failed");
            Ok("transcript".into())
        }
        fn remove(&self, address: &str) -> Result<()> {
            self.removed.borrow_mut().push(address.into());
            self.terminals.borrow_mut().retain(|t| t.address != address);
            Ok(())
        }
    }

    fn terminal(key: &Key, finished: bool) -> Terminal {
        Terminal {
            address: "t7".into(),
            occurrence: key.label(),
            finished,
            exit_code: finished.then_some(7),
            signal: None,
            finished_ms: finished.then_some(65_000),
        }
    }

    #[test]
    fn adopts_then_collects_after_restart_and_keeps_terminal_on_read_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let key = Key {
            job: "job".into(),
            scheduled: 60,
        };
        Ledger::open(dir.path())?.claim(&key, "command", 60, false)?;
        let db = Ledger::open(dir.path())?;
        let core = Fake::default();
        core.terminals.borrow_mut().push(terminal(&key, false));
        assert!(reconcile(&db, &core, "owner", 62)?.contains("job"));
        assert_eq!(db.get(&key)?.unwrap().terminal.as_deref(), Some("t7"));
        core.terminals.replace(vec![terminal(&key, true)]);
        core.fail_read.set(true);
        assert!(reconcile(&db, &core, "owner", 66)?.is_empty());
        assert!(core.removed.borrow().is_empty());
        core.fail_read.set(false);
        reconcile(&db, &core, "owner", 67)?;
        let run = db.get(&key)?.unwrap();
        assert_eq!(run.state, State::Failed);
        assert_eq!(run.finished, Some(65));
        assert_eq!(run.output.as_deref(), Some("transcript"));
        assert_eq!(*core.removed.borrow(), ["t7"]);
        Ok(())
    }

    #[test]
    #[ignore = "requires installed hc; starts a private local core"]
    fn real_core_recovers_and_collects_two_runs() -> Result<()> {
        use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

        struct PrivateCore(PathBuf);
        impl Drop for PrivateCore {
            fn drop(&mut self) {
                let _ = Command::new(&self.0).args(["server", "stop"]).output();
            }
        }

        fn shell_quote(path: &Path) -> String {
            format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
        }

        let dir = tempfile::Builder::new()
            .prefix("cron-hc-")
            .tempdir_in("/tmp")?;
        let wrapper = dir.path().join("private-hc");
        let mut script = String::from("#!/bin/sh\nunset HC_TERMINAL_ID\n");
        for (key, name) in [
            ("HC_SOCKET", "core.sock"),
            ("HC_SESSION", "session"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let value = if key == "HC_SESSION" {
                dir.path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            } else {
                shell_quote(&dir.path().join(name))
            };
            script.push_str(&format!("export {key}={value}\n"));
        }
        script.push_str("exec hc \"$@\"\n");
        fs::write(&wrapper, script)?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
        let private = PrivateCore(wrapper.clone());
        let result = Command::new(&private.0)
            .args(["server", "start"])
            .output()?;
        ensure!(result.status.success(), "private core startup: {result:?}");
        let work = dir.path();
        let state = work.join("scheduler");
        let core = Hc::new(wrapper.into_os_string());
        let long = Key {
            job: "long".into(),
            scheduled: 60,
        };
        let short = Key {
            job: "short".into(),
            scheduled: 60,
        };
        let (owner, long_t, short_t) = {
            let _lock = ledger::serve_lock(&state)?;
            let db = Ledger::open(&state)?;
            let owner = db.owner()?;
            let start = |key: &Key, command: &str| {
                ensure!(db.claim(key, command, 60, false)?, "expected a new claim");
                core.launch(&owner, &key.label(), command, work)
            };
            let long_t = start(&long, "read -r input; echo long-output")?;
            let short_t = start(&short, "echo short-output; exit 7")?;
            // Lose both launch addresses before recording them.
            (owner, long_t, short_t)
        };
        let _lock = ledger::serve_lock(&state)?;
        let db = Ledger::open(&state)?;
        assert!(reconcile(&db, &core, &owner, 61)?.contains("long"));
        assert_eq!(
            db.get(&long)?.unwrap().terminal.as_deref(),
            Some(long_t.as_str())
        );
        assert!(!db.claim(&long, "duplicate", 62, false)?);
        let keys = Command::new(&private.0)
            .args(["keys", &long_t, "enter"])
            .output()?;
        ensure!(keys.status.success(), "release long terminal: {keys:?}");
        if core
            .list(&owner)?
            .iter()
            .any(|terminal| terminal.address == short_t)
        {
            let result = Command::new(&private.0)
                .args(["wait", "--json", "--timeout", "5", &short_t])
                .output()?;
            let value: serde_json::Value = serde_json::from_slice(&result.stdout)?;
            assert_eq!(value["exit_code"], 7, "{result:?}");
        }
        let result = Command::new(&private.0)
            .args(["wait", "--json", "--timeout", "5", &long_t])
            .output()?;
        let value: serde_json::Value = serde_json::from_slice(&result.stdout)?;
        assert_eq!(value["exit_code"], 0, "{result:?}");
        reconcile(&db, &core, &owner, 63)?;
        let long = db.get(&long)?.unwrap();
        let short = db.get(&short)?.unwrap();
        assert_eq!(long.state, State::Succeeded);
        assert!(long.output.unwrap().contains("long-output"));
        assert_eq!(short.exit_code, Some(7));
        assert!(short.output.unwrap().contains("short-output"));
        assert!(core.list(&owner)?.is_empty());
        Ok(())
    }

    #[test]
    fn skips_overlap_and_never_retries_an_ambiguous_occurrence() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Ledger::open(dir.path())?;
        let core = Fake::default();
        let jobs = crate::schedule::parse("* * * * * example")?;
        let dispatch = Dispatch {
            db: &db,
            cwd: dir.path(),
            owner: "owner",
            jobs: &jobs,
            zone: chrono_tz::UTC,
        };
        let stop = AtomicBool::new(false);
        let job = jobs[0].id.clone();
        dispatch.minute(&core, 60, &HashSet::from([job.clone()]), &stop)?;
        assert_eq!(
            db.get(&Key {
                job: job.clone(),
                scheduled: 60
            })?
            .unwrap()
            .state,
            State::Skipped
        );
        let busy = reconcile(&db, &core, "owner", 120)?;
        dispatch.minute(&core, 120, &busy, &stop)?;
        reconcile(&db, &core, "owner", 121)?;
        dispatch.minute(&core, 120, &busy, &stop)?;
        assert_eq!(core.launched.get(), 1);
        assert_eq!(
            db.get(&Key {
                job,
                scheduled: 120
            })?
            .unwrap()
            .state,
            State::Unknown
        );
        Ok(())
    }
}
