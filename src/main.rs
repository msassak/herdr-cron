mod hc;
mod ledger;
mod schedule;
mod supervisor;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(version, about = "Schedule commands in Herdr Core terminals")]
struct Cli {
    /// Run history directory (default: $XDG_STATE_HOME/herdr-cron or ~/.local/state/herdr-cron).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// IANA timezone, e.g. America/New_York (default: TZ or system timezone).
    #[arg(long, global = true)]
    timezone: Option<Tz>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a crontab and show its upcoming runs.
    Check {
        crontab: PathBuf,
        /// Upcoming times per job.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=20))]
        next: u32,
    },
    /// Run the scheduler in the foreground.
    Serve {
        crontab: PathBuf,
        /// Working directory of jobs (default: your home directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Show recent runs, including their output tails.
    History {
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1_000))]
        limit: u32,
    },
}

fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME must name an absolute path")
}

fn state_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    let base = match env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(path) => path,
        None => home()?.join(".local/state"),
    };
    Ok(base.join("herdr-cron"))
}

fn timezone(explicit: Option<Tz>) -> Result<Tz> {
    if let Some(zone) = explicit {
        return Ok(zone);
    }
    let name = match env::var("TZ") {
        Ok(name) if !name.is_empty() => name,
        _ => iana_time_zone::get_timezone().context("detect system timezone; pass --timezone")?,
    };
    name.parse()
        .map_err(|_| anyhow::anyhow!("unknown IANA timezone {name:?}; pass --timezone"))
}

fn load(path: &Path) -> Result<Vec<schedule::Job>> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    schedule::parse(&contents).with_context(|| format!("parse {}", path.display()))
}

fn print_check(jobs: &[schedule::Job], zone: Tz, count: u32, now: DateTime<Utc>) {
    println!("{} job(s) · {zone}", jobs.len());
    for job in jobs {
        println!("line {}: {}", job.line, job.command);
        let mut cursor = now;
        for _ in 0..count {
            let Some(next) = job.next_after(cursor, zone) else {
                println!("  no upcoming occurrence");
                break;
            };
            println!("  {}", next.with_timezone(&zone).to_rfc3339());
            cursor = next;
        }
    }
}

fn stamp(time: i64) -> String {
    DateTime::from_timestamp(time, 0).map_or_else(|| time.to_string(), |time| time.to_rfc3339())
}

fn print_history(dir: &Path, limit: u32) -> Result<()> {
    if !dir.join(ledger::DATABASE_NAME).try_exists()? {
        println!("No runs recorded.");
        return Ok(());
    }
    let runs = ledger::Ledger::open_history(dir)?.recent(limit)?;
    if runs.is_empty() {
        println!("No runs recorded.");
    }
    for run in runs {
        println!(
            "{}  {}  {}",
            stamp(run.key.scheduled),
            run.state.as_str(),
            run.command
        );
        if let Some(terminal) = run.terminal {
            println!("  terminal: {terminal}");
        }
        if let Some(started) = run.started {
            println!("  started: {}", stamp(started));
        }
        if let Some(finished) = run.finished {
            println!("  finished: {}", stamp(finished));
        }
        if let Some(code) = run.exit_code {
            println!("  exit: {code}");
        }
        if let Some(signal) = run.signal {
            println!("  signal: {signal}");
        }
        if let Some(detail) = run.detail {
            println!("  {detail}");
        }
        if let Some(output) = run.output.filter(|s| !s.is_empty()) {
            println!("  terminal output (tail; earlier lines may be omitted):");
            for line in output.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Check { crontab, next } => {
            print_check(&load(&crontab)?, timezone(cli.timezone)?, next, Utc::now())
        }
        Command::Serve { crontab, cwd } => {
            let jobs = load(&crontab)?;
            let work_dir = match cwd {
                Some(dir) => dir,
                None => home()?,
            }
            .canonicalize()
            .context("resolve job working directory")?;
            ensure!(
                work_dir.is_dir(),
                "job working directory is not a directory: {}",
                work_dir.display()
            );
            let core = hc::Hc::new(env::var_os("HERDR_CRON_HC").unwrap_or_else(|| "hc".into()));
            supervisor::serve(
                &core,
                &state_dir(cli.state_dir)?,
                &work_dir,
                &jobs,
                timezone(cli.timezone)?,
            )?;
        }
        Command::History { limit } => print_history(&state_dir(cli.state_dir)?, limit)?,
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-cron: {error:#}");
            ExitCode::FAILURE
        }
    }
}
