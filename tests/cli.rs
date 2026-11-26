use std::{fs, process::Command};

#[test]
fn check_lists_upcoming_runs_in_selected_zone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let crontab = dir.path().join("crontab");
    fs::write(&crontab, "# sample\n*/15 * * * MON-FRI echo hello\n")?;
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-cron"))
        .args(["check", "--timezone", "UTC", "--next", "1"])
        .arg(crontab)
        .output()?;
    assert!(result.status.success(), "{:?}", result);
    let output = String::from_utf8(result.stdout)?;
    assert!(output.contains("line 2: echo hello"));
    let instant = chrono::DateTime::parse_from_rfc3339(output.lines().last().unwrap().trim())?;
    assert_eq!(instant.timestamp() % 900, 0);
    assert_eq!(instant.offset().local_minus_utc(), 0);
    Ok(())
}

#[test]
fn check_reports_line_and_field_without_creating_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let crontab = dir.path().join("crontab");
    let state_dir = dir.path().join("state");
    fs::write(&crontab, "# sample\n61 * * * * echo hello\n")?;
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-cron"))
        .args(["check", "--state-dir"])
        .arg(&state_dir)
        .arg(crontab)
        .output()?;
    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr)?;
    assert!(error.contains("line 2: minute field"), "{error}");
    assert!(!state_dir.exists());
    Ok(())
}

#[test]
fn history_without_state_does_not_create_a_database() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state_dir = dir.path().join("state");
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-cron"))
        .args(["history", "--state-dir"])
        .arg(&state_dir)
        .output()?;
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(result.stdout, b"No runs recorded.\n");
    assert!(!state_dir.exists());
    Ok(())
}
