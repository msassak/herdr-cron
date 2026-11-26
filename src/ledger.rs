use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};

pub const DATABASE_NAME: &str = "runs.sqlite3";

pub struct Ledger(Connection);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Launching,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }

    fn from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Self> {
        match row.get_ref(index)?.as_str()? {
            "launching" => Ok(Self::Launching),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            "unknown" => Ok(Self::Unknown),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Key {
    pub job: String,
    pub scheduled: i64,
}

impl Key {
    pub fn label(&self) -> String {
        format!("{}:{}", self.job, self.scheduled)
    }

    pub fn from_label(label: &str) -> Option<Self> {
        let (job, scheduled) = label.split_once(':')?;
        let key = Self {
            job: job.to_owned(),
            scheduled: scheduled.parse().ok()?,
        };
        (key.label() == label).then_some(key)
    }
}

#[derive(Debug)]
pub struct Run {
    pub key: Key,
    pub command: String,
    pub state: State,
    pub terminal: Option<String>,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub output: Option<String>,
    pub detail: Option<String>,
}

pub struct Completion<'a> {
    pub state: State,
    pub terminal: &'a str,
    pub finished: i64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub output: &'a str,
    pub detail: Option<&'a str>,
}

const COLUMNS: &str = "job_id, scheduled, command, state, terminal, started, finished,\n    exit_code, signal, output, detail";

fn run(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        key: Key {
            job: row.get(0)?,
            scheduled: row.get(1)?,
        },
        command: row.get(2)?,
        state: State::from_row(row, 3)?,
        terminal: row.get(4)?,
        started: row.get(5)?,
        finished: row.get(6)?,
        exit_code: row.get(7)?,
        signal: row.get(8)?,
        output: row.get(9)?,
        detail: row.get(10)?,
    })
}

fn prepare_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create state directory {}", dir.display()))
}

// An OS lock releases on crash. Keep the file: unlinking it could create two
// lockable inodes while an old instance still holds one.
pub fn serve_lock(dir: &Path) -> Result<File> {
    prepare_dir(dir)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("serve.lock"))
        .context("open scheduler lock")?;
    file.try_lock()
        .with_context(|| format!("lock {}; another scheduler may be running", dir.display()))?;
    Ok(file)
}

impl Ledger {
    pub fn open(dir: &Path) -> Result<Self> {
        prepare_dir(dir)?;
        let path = dir.join(DATABASE_NAME);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS runs (
                 job_id TEXT NOT NULL,
                 scheduled INTEGER NOT NULL,
                 command TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN
                     ('launching', 'running', 'succeeded', 'failed', 'skipped', 'unknown')),
                 terminal TEXT,
                 started INTEGER,
                 finished INTEGER,
                 exit_code INTEGER,
                 signal INTEGER,
                 output TEXT,
                 detail TEXT,
                 PRIMARY KEY (job_id, scheduled)
             );
             CREATE INDEX IF NOT EXISTS runs_by_time ON runs (scheduled DESC, job_id);
             CREATE INDEX IF NOT EXISTS active_runs ON runs (state)
                 WHERE state IN ('launching', 'running');
             INSERT INTO meta (key, value) VALUES ('owner', lower(hex(randomblob(16))))
                 ON CONFLICT (key) DO NOTHING;",
        )?;
        Ok(Self(db))
    }

    pub fn open_history(dir: &Path) -> Result<Self> {
        let db =
            Connection::open_with_flags(dir.join(DATABASE_NAME), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self(db))
    }

    pub fn owner(&self) -> Result<String> {
        Ok(self
            .0
            .query_row("SELECT value FROM meta WHERE key='owner'", [], |row| {
                row.get(0)
            })?)
    }

    pub fn last_minute(&self) -> Result<Option<i64>> {
        let value: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM meta WHERE key='last_minute'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.map(|value| value.parse()).transpose()?)
    }

    pub fn remember_minute(&self, instant: i64) -> Result<()> {
        self.0.execute(
            "INSERT INTO meta (key, value) VALUES ('last_minute', ?1)
                ON CONFLICT (key) DO UPDATE SET value=excluded.value",
            [instant.to_string()],
        )?;
        Ok(())
    }

    pub fn claim(&self, key: &Key, command: &str, now: i64, overlap: bool) -> Result<bool> {
        let (state, started, finished, detail) = if overlap {
            (
                State::Skipped,
                None,
                Some(now),
                Some("previous run still active"),
            )
        } else {
            (State::Launching, Some(now), None, None)
        };
        let inserted = self.0.execute(
            "INSERT INTO runs (job_id, scheduled, command, state, started, finished, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (job_id, scheduled) DO NOTHING",
            params![
                key.job,
                key.scheduled,
                command,
                state.as_str(),
                started,
                finished,
                detail
            ],
        )?;
        Ok(inserted == 1 && !overlap)
    }

    pub fn get(&self, key: &Key) -> Result<Option<Run>> {
        let sql = format!("SELECT {COLUMNS} FROM runs WHERE job_id=?1 AND scheduled=?2");
        Ok(self
            .0
            .query_row(&sql, params![key.job, key.scheduled], run)
            .optional()?)
    }

    pub fn active(&self) -> Result<Vec<Run>> {
        let sql = format!("SELECT {COLUMNS} FROM runs WHERE state IN ('launching', 'running')");
        let mut query = self.0.prepare(&sql)?;
        Ok(query.query_map([], run)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recent(&self, limit: u32) -> Result<Vec<Run>> {
        let sql = format!("SELECT {COLUMNS} FROM runs ORDER BY scheduled DESC, job_id LIMIT ?1");
        let mut query = self.0.prepare(&sql)?;
        Ok(query
            .query_map([limit], run)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn running(&self, key: &Key, terminal: &str) -> Result<()> {
        self.0.execute(
            "UPDATE runs SET state='running', terminal=?3, finished=NULL, detail=NULL
             WHERE job_id=?1 AND scheduled=?2 AND output IS NULL AND state!='skipped'",
            params![key.job, key.scheduled, terminal],
        )?;
        Ok(())
    }

    pub fn unknown(&self, key: &Key, now: i64, detail: &str) -> Result<()> {
        self.0.execute(
            "UPDATE runs SET state='unknown', finished=?3, detail=?4
             WHERE job_id=?1 AND scheduled=?2 AND state IN ('launching', 'running')",
            params![key.job, key.scheduled, now, detail],
        )?;
        Ok(())
    }

    pub fn collect(&self, key: &Key, result: Completion<'_>) -> Result<()> {
        self.0.execute(
            "UPDATE runs SET state=?3, terminal=?4, finished=?5, exit_code=?6, signal=?7,
                             output=?8, detail=?9
             WHERE job_id=?1 AND scheduled=?2 AND output IS NULL AND state!='skipped'",
            params![
                key.job,
                key.scheduled,
                result.state.as_str(),
                result.terminal,
                result.finished,
                result.exit_code,
                result.signal,
                result.output,
                result.detail
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scheduled: i64) -> Key {
        Key {
            job: "some-job".into(),
            scheduled,
        }
    }

    #[test]
    fn claims_are_durable_and_do_not_relaunch_after_ambiguity() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Ledger::open(dir.path())?;
        let owner = db.owner()?;
        assert!(db.claim(&key(60), "example", 61, false)?);
        db.unknown(&key(60), 62, "response lost")?;
        db.remember_minute(60)?;
        drop(db);

        let db = Ledger::open(dir.path())?;
        assert_eq!(db.owner()?, owner);
        assert_eq!(db.last_minute()?, Some(60));
        assert!(!db.claim(&key(60), "example", 63, false)?);
        assert_eq!(db.get(&key(60))?.unwrap().state, State::Unknown);
        db.running(&key(60), "t1")?;
        assert_eq!(db.active()?[0].terminal.as_deref(), Some("t1"));
        Ok(())
    }

    #[test]
    fn records_skips_and_results_in_scheduled_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Ledger::open(dir.path())?;
        assert!(db.claim(&key(60), "example", 61, false)?);
        assert!(!db.claim(&key(120), "example", 120, true)?);
        db.collect(
            &key(60),
            Completion {
                state: State::Failed,
                terminal: "t1",
                finished: 130,
                exit_code: Some(7),
                signal: None,
                output: "some output",
                detail: None,
            },
        )?;
        let runs = db.recent(10)?;
        assert_eq!(runs[0].state, State::Skipped);
        assert_eq!(runs[1].state, State::Failed);
        assert_eq!(runs[1].exit_code, Some(7));
        assert_eq!(runs[1].output.as_deref(), Some("some output"));
        assert!(db.active()?.is_empty());
        Ok(())
    }

    #[test]
    fn releases_single_scheduler_lock_on_close() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let lock = serve_lock(dir.path())?;
        assert!(serve_lock(dir.path()).is_err());
        drop(lock);
        assert!(serve_lock(dir.path()).is_ok());
        Ok(())
    }
}
