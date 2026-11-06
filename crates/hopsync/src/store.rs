//! The daemon's database (Phase 2 step 2): servers, sealed secrets, jobs with their per-file
//! results and events, settings, the admin account and login sessions.
//!
//! - One SQLite file, `hopsync.db`, created 0600 in a 0700 directory, WAL with `synchronous =
//!   NORMAL` like the journal. The resume journal keeps its own file: it writes a row per durable
//!   chunk, and that write rate should not contend with API requests.
//! - The schema version is `PRAGMA user_version`. Each migration runs in one transaction; a
//!   database from a newer Hopsync is refused, never downgraded.
//! - All access goes through one connection behind a mutex. Callers on the async runtime use
//!   `spawn_blocking` for anything that may touch the disk for long.

use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum StoreError {
    Db(rusqlite::Error),
    Io(io::Error),
    /// The database was written by a newer Hopsync.
    TooNew {
        found: i64,
        known: i64,
    },
    NotFound,
    /// A uniqueness rule or reference check failed; the text says which.
    Conflict(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::Io(e) => write!(f, "database file: {e}"),
            Self::TooNew { found, known } => write!(
                f,
                "database schema version {found} is newer than this Hopsync understands ({known}); upgrade Hopsync"
            ),
            Self::NotFound => f.write_str("not found"),
            Self::Conflict(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => Self::NotFound,
            e => Self::Db(e),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Migrations in order; index + 1 is the schema version each one produces.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema.
    "CREATE TABLE secrets (
         id INTEGER PRIMARY KEY,
         purpose TEXT NOT NULL,
         sealed BLOB NOT NULL
     );
     CREATE TABLE servers (
         id INTEGER PRIMARY KEY,
         name TEXT NOT NULL UNIQUE,
         host TEXT NOT NULL,
         port INTEGER NOT NULL,
         user TEXT NOT NULL,
         root TEXT NOT NULL,
         key_file TEXT,
         key_secret INTEGER REFERENCES secrets(id),
         min_free INTEGER NOT NULL DEFAULT 0,
         blocked TEXT,
         created INTEGER NOT NULL
     );
     CREATE TABLE jobs (
         id INTEGER PRIMARY KEY,
         direction TEXT NOT NULL CHECK (direction IN ('push', 'pull')),
         server INTEGER NOT NULL REFERENCES servers(id),
         src TEXT NOT NULL,
         dst TEXT NOT NULL,
         existing TEXT NOT NULL,
         verify INTEGER NOT NULL,
         lanes INTEGER,
         state TEXT NOT NULL,
         position INTEGER NOT NULL,
         created INTEGER NOT NULL,
         started INTEGER,
         finished INTEGER,
         error TEXT,
         files INTEGER NOT NULL DEFAULT 0,
         bytes INTEGER NOT NULL DEFAULT 0,
         problems INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX jobs_state ON jobs(state, position);
     CREATE TABLE job_items (
         job INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
         path TEXT NOT NULL,
         outcome TEXT NOT NULL,
         bytes INTEGER NOT NULL,
         detail TEXT NOT NULL
     );
     CREATE INDEX job_items_job ON job_items(job);
     CREATE TABLE job_events (
         job INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
         at INTEGER NOT NULL,
         text TEXT NOT NULL
     );
     CREATE INDEX job_events_job ON job_events(job);
     CREATE TABLE settings (
         key TEXT PRIMARY KEY,
         value TEXT NOT NULL
     ) WITHOUT ROWID;
     CREATE TABLE admin (
         id INTEGER PRIMARY KEY CHECK (id = 1),
         password_hash TEXT NOT NULL,
         created INTEGER NOT NULL
     );
     CREATE TABLE sessions (
         token_hash BLOB PRIMARY KEY,
         created INTEGER NOT NULL,
         seen INTEGER NOT NULL
     ) WITHOUT ROWID;
     CREATE TABLE audit (
         at INTEGER NOT NULL,
         event TEXT NOT NULL,
         detail TEXT NOT NULL
     );",
    // 2: a stop pressed on a running job, kept until the job reports that it stopped.
    "ALTER TABLE jobs ADD COLUMN stop_wanted TEXT CHECK (stop_wanted IN ('paused', 'cancelled'));",
];

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Absolute remote directory every path of this server stays under.
    pub root: String,
    /// Private key file on this machine, or None when the key is sealed in the vault.
    pub key_file: Option<String>,
    pub key_secret: Option<i64>,
    pub min_free: u64,
    /// Why the server may not be used (a changed host key), until the user re-trusts it.
    pub blocked: Option<String>,
    pub created: i64,
}

/// What the API accepts for a new or edited server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub root: String,
    pub key_file: Option<String>,
    #[serde(default)]
    pub min_free: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Finished one way or another: nothing will run unless the user retries.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Push,
    Pull,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pull => "pull",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "push" => Some(Self::Push),
            "pull" => Some(Self::Pull),
            _ => None,
        }
    }
}

/// What the API accepts for a new job. Paths are relative: `src` and `dst` are under the local
/// root and the server root, swapped for a pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub direction: Direction,
    pub server: i64,
    pub src: String,
    pub dst: String,
    /// `skip`, `update`, `overwrite` or `rename` (validated by the API).
    pub existing: String,
    pub verify: bool,
    /// Fixed lane count; None lets auto-tune choose.
    pub lanes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Job {
    pub id: i64,
    #[serde(flatten)]
    pub spec: JobSpec,
    pub state: JobState,
    pub position: i64,
    pub created: i64,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub error: Option<String>,
    pub files: u64,
    pub bytes: u64,
    pub problems: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobItem {
    pub path: String,
    /// `sent`, `unchanged`, `skipped` or `failed`.
    pub outcome: String,
    pub bytes: u64,
    /// Destination for sent files; the reason for skipped and failed ones.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobEvent {
    pub at: i64,
    pub text: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Opens or creates the database and brings its schema up to date.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        // Create with 0600 before SQLite opens it, so it never exists with wider permissions.
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite half-written (transactions roll
        // back), so the connection stays usable.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.lock();
        let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
        let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > known {
            return Err(StoreError::TooNew { found, known });
        }
        for (i, sql) in MIGRATIONS
            .iter()
            .enumerate()
            .skip(usize::try_from(found).unwrap_or(0))
        {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(i + 1).unwrap_or(i64::MAX),
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // Secrets (sealed by the vault; the store never sees plaintext).

    pub fn add_secret(&self, purpose: &str, sealed: &[u8]) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO secrets (purpose, sealed) VALUES (?1, ?2)",
            params![purpose, sealed],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Replaces a secret's sealed bytes (sealing needs the row id, so a new secret is added
    /// empty, then sealed and set).
    pub fn set_secret(&self, id: i64, sealed: &[u8]) -> Result<()> {
        let n = self.lock().execute(
            "UPDATE secrets SET sealed = ?2 WHERE id = ?1",
            params![id, sealed],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// The purpose and sealed bytes of a secret.
    pub fn secret(&self, id: i64) -> Result<(String, Vec<u8>)> {
        Ok(self.lock().query_row(
            "SELECT purpose, sealed FROM secrets WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn secret_count(&self) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("SELECT count(*) FROM secrets", [], |r| r.get(0))?)
    }

    pub fn delete_secret(&self, id: i64) -> Result<()> {
        self.lock()
            .execute("DELETE FROM secrets WHERE id = ?1", [id])?;
        Ok(())
    }

    // Servers.

    fn server_row(r: &Row<'_>) -> rusqlite::Result<Server> {
        Ok(Server {
            id: r.get(0)?,
            name: r.get(1)?,
            host: r.get(2)?,
            port: r.get(3)?,
            user: r.get(4)?,
            root: r.get(5)?,
            key_file: r.get(6)?,
            key_secret: r.get(7)?,
            min_free: to_u64(r.get(8)?),
            blocked: r.get(9)?,
            created: r.get(10)?,
        })
    }

    const SERVER_COLS: &str =
        "id, name, host, port, user, root, key_file, key_secret, min_free, blocked, created";

    pub fn add_server(&self, s: &ServerSpec) -> Result<Server> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO servers (name, host, port, user, root, key_file, min_free, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.name,
                s.host,
                s.port,
                s.user,
                s.root,
                s.key_file,
                to_i64(s.min_free),
                now()
            ],
        )
        .map_err(|e| unique_conflict(e, "a server with this name exists"))?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.server(id)
    }

    pub fn update_server(&self, id: i64, s: &ServerSpec) -> Result<Server> {
        let n = self
            .lock()
            .execute(
                "UPDATE servers SET name = ?2, host = ?3, port = ?4, user = ?5, root = ?6,
                 key_file = ?7, min_free = ?8 WHERE id = ?1",
                params![
                    id,
                    s.name,
                    s.host,
                    s.port,
                    s.user,
                    s.root,
                    s.key_file,
                    to_i64(s.min_free)
                ],
            )
            .map_err(|e| unique_conflict(e, "a server with this name exists"))?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        self.server(id)
    }

    pub fn server(&self, id: i64) -> Result<Server> {
        Ok(self.lock().query_row(
            &format!("SELECT {} FROM servers WHERE id = ?1", Self::SERVER_COLS),
            [id],
            Self::server_row,
        )?)
    }

    pub fn servers(&self) -> Result<Vec<Server>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {} FROM servers ORDER BY name",
            Self::SERVER_COLS
        ))?;
        let rows = st.query_map([], Self::server_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_server_key_secret(&self, id: i64, secret: Option<i64>) -> Result<()> {
        self.lock().execute(
            "UPDATE servers SET key_secret = ?2 WHERE id = ?1",
            params![id, secret],
        )?;
        Ok(())
    }

    pub fn set_server_blocked(&self, id: i64, why: Option<&str>) -> Result<()> {
        self.lock().execute(
            "UPDATE servers SET blocked = ?2 WHERE id = ?1",
            params![id, why],
        )?;
        Ok(())
    }

    /// Removes a server that no unfinished job uses. Finished jobs that used it are removed
    /// with it, as is its sealed key.
    pub fn delete_server(&self, id: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let busy: i64 = tx.query_row(
            "SELECT count(*) FROM jobs WHERE server = ?1 AND state IN ('queued', 'running', 'paused')",
            [id],
            |r| r.get(0),
        )?;
        if busy > 0 {
            return Err(StoreError::Conflict(
                "the server has unfinished jobs; cancel or finish them first".into(),
            ));
        }
        let secret: Option<i64> = tx
            .query_row("SELECT key_secret FROM servers WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or(StoreError::NotFound)?;
        tx.execute("DELETE FROM jobs WHERE server = ?1", [id])?;
        tx.execute("DELETE FROM servers WHERE id = ?1", [id])?;
        if let Some(s) = secret {
            tx.execute("DELETE FROM secrets WHERE id = ?1", [s])?;
        }
        tx.commit()?;
        Ok(())
    }

    // Jobs.

    const JOB_COLS: &str = "id, direction, server, src, dst, existing, verify, lanes, state, \
         position, created, started, finished, error, files, bytes, problems";

    fn job_row(r: &Row<'_>) -> rusqlite::Result<Job> {
        let dir: String = r.get(1)?;
        let state: String = r.get(8)?;
        let bad = |i, what: &str| {
            rusqlite::Error::FromSqlConversionFailure(
                i,
                rusqlite::types::Type::Text,
                format!("unknown {what}").into(),
            )
        };
        Ok(Job {
            id: r.get(0)?,
            spec: JobSpec {
                direction: Direction::parse(&dir).ok_or_else(|| bad(1, "direction"))?,
                server: r.get(2)?,
                src: r.get(3)?,
                dst: r.get(4)?,
                existing: r.get(5)?,
                verify: r.get(6)?,
                lanes: r.get(7)?,
            },
            state: JobState::parse(&state).ok_or_else(|| bad(8, "state"))?,
            position: r.get(9)?,
            created: r.get(10)?,
            started: r.get(11)?,
            finished: r.get(12)?,
            error: r.get(13)?,
            files: to_u64(r.get(14)?),
            bytes: to_u64(r.get(15)?),
            problems: to_u64(r.get(16)?),
        })
    }

    /// Queues a job at the end. Refused when an unfinished job already writes the same
    /// destination: two jobs would race on the same part files.
    pub fn add_job(&self, spec: &JobSpec) -> Result<Job> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let clash: i64 = tx.query_row(
            "SELECT count(*) FROM jobs WHERE direction = ?1 AND server = ?2 AND dst = ?3
             AND state IN ('queued', 'running', 'paused')",
            params![spec.direction.as_str(), spec.server, spec.dst],
            |r| r.get(0),
        )?;
        if clash > 0 {
            return Err(StoreError::Conflict(
                "another unfinished job writes the same destination".into(),
            ));
        }
        let position: i64 =
            tx.query_row("SELECT coalesce(max(position), 0) + 1 FROM jobs", [], |r| {
                r.get(0)
            })?;
        tx.execute(
            "INSERT INTO jobs (direction, server, src, dst, existing, verify, lanes, state,
             position, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9)",
            params![
                spec.direction.as_str(),
                spec.server,
                spec.src,
                spec.dst,
                spec.existing,
                spec.verify,
                spec.lanes,
                position,
                now()
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                StoreError::Conflict("no such server".into())
            }
            e => e.into(),
        })?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        drop(conn);
        self.job(id)
    }

    pub fn job(&self, id: i64) -> Result<Job> {
        Ok(self.lock().query_row(
            &format!("SELECT {} FROM jobs WHERE id = ?1", Self::JOB_COLS),
            [id],
            Self::job_row,
        )?)
    }

    /// Jobs in queue order, optionally only those in `state`.
    pub fn jobs(&self, state: Option<JobState>) -> Result<Vec<Job>> {
        let conn = self.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {} FROM jobs WHERE ?1 IS NULL OR state = ?1 ORDER BY position",
            Self::JOB_COLS
        ))?;
        let rows = st.query_map([state.map(JobState::as_str)], Self::job_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The first queued job, in queue order.
    pub fn next_queued(&self) -> Result<Option<Job>> {
        Ok(self
            .lock()
            .query_row(
                &format!(
                    "SELECT {} FROM jobs WHERE state = 'queued' ORDER BY position LIMIT 1",
                    Self::JOB_COLS
                ),
                [],
                Self::job_row,
            )
            .optional()?)
    }

    pub fn set_job_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<()> {
        let t = now();
        let n = self.lock().execute(
            "UPDATE jobs SET state = ?2, error = ?3, stop_wanted = NULL,
               started = CASE WHEN ?2 = 'running' THEN ?4 ELSE started END,
               finished = CASE WHEN ?2 IN ('done', 'failed', 'cancelled') THEN ?4 ELSE NULL END
             WHERE id = ?1",
            params![id, state.as_str(), error, t],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn set_job_totals(&self, id: i64, files: u64, bytes: u64, problems: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET files = ?2, bytes = ?3, problems = ?4 WHERE id = ?1",
            params![id, to_i64(files), to_i64(bytes), to_i64(problems)],
        )?;
        Ok(())
    }

    /// Moves a job to just before `before` in the queue (None: to the end).
    pub fn move_job(&self, id: i64, before: Option<i64>) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut ids: Vec<i64> = {
            let mut st = tx.prepare("SELECT id FROM jobs ORDER BY position")?;
            st.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let from = ids
            .iter()
            .position(|&j| j == id)
            .ok_or(StoreError::NotFound)?;
        ids.remove(from);
        let to = match before {
            Some(b) => ids
                .iter()
                .position(|&j| j == b)
                .ok_or(StoreError::NotFound)?,
            None => ids.len(),
        };
        ids.insert(to, id);
        for (pos, j) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE jobs SET position = ?2 WHERE id = ?1",
                params![j, i64::try_from(pos).unwrap_or(i64::MAX) + 1],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes a finished job and its items and events.
    pub fn delete_job(&self, id: i64) -> Result<()> {
        let state = self.job(id)?.state;
        if !state.is_final() {
            return Err(StoreError::Conflict(
                "the job is not finished; cancel it first".into(),
            ));
        }
        self.lock()
            .execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Records that the user asked a running job to stop, so a daemon that dies before the
    /// transfer has stopped still ends the job that way. `set_job_state` clears it.
    pub fn want_job_stop(&self, id: i64, state: JobState) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET stop_wanted = ?2 WHERE id = ?1 AND state = 'running'",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// Jobs left running by a daemon that stopped. One the user asked to stop ends as asked; the
    /// others go back to the queue, to resume from the journal. Returns how many were requeued.
    pub fn requeue_running(&self) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let t = now();
        tx.execute(
            "INSERT INTO job_events (job, at, text)
             SELECT id, ?1, stop_wanted FROM jobs
             WHERE state = 'running' AND stop_wanted IS NOT NULL",
            [t],
        )?;
        tx.execute(
            "UPDATE jobs SET state = stop_wanted, stop_wanted = NULL,
               finished = CASE WHEN stop_wanted = 'cancelled' THEN ?1 ELSE NULL END
             WHERE state = 'running' AND stop_wanted IS NOT NULL",
            [t],
        )?;
        let requeued = tx.execute(
            "UPDATE jobs SET state = 'queued', started = NULL WHERE state = 'running'",
            [],
        )?;
        tx.commit()?;
        Ok(requeued)
    }

    /// Starts a job's run: forgets the items of any earlier run.
    pub fn clear_job_items(&self, id: i64) -> Result<()> {
        self.lock()
            .execute("DELETE FROM job_items WHERE job = ?1", [id])?;
        Ok(())
    }

    pub fn add_job_items(&self, job: i64, items: &[JobItem]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO job_items (job, path, outcome, bytes, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for i in items {
                st.execute(params![job, i.path, i.outcome, to_i64(i.bytes), i.detail])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One page of a job's items, in the order they finished.
    pub fn job_items(&self, job: i64, offset: u64, limit: u64) -> Result<Vec<JobItem>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT path, outcome, bytes, detail FROM job_items WHERE job = ?1
             ORDER BY rowid LIMIT ?2 OFFSET ?3",
        )?;
        let rows = st.query_map(params![job, to_i64(limit), to_i64(offset)], |r| {
            Ok(JobItem {
                path: r.get(0)?,
                outcome: r.get(1)?,
                bytes: to_u64(r.get(2)?),
                detail: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_job_event(&self, job: i64, text: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO job_events (job, at, text) VALUES (?1, ?2, ?3)",
            params![job, now(), text],
        )?;
        Ok(())
    }

    pub fn job_events(&self, job: i64) -> Result<Vec<JobEvent>> {
        let conn = self.lock();
        let mut st =
            conn.prepare("SELECT at, text FROM job_events WHERE job = ?1 ORDER BY rowid")?;
        let rows = st.query_map([job], |r| {
            Ok(JobEvent {
                at: r.get(0)?,
                text: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // Settings: one JSON value per key, validated by the API.

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // Admin account and sessions.

    pub fn admin_hash(&self) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT password_hash FROM admin WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Creates the admin account; refused when one exists (setup runs once).
    pub fn create_admin(&self, hash: &str) -> Result<()> {
        self.lock()
            .execute(
                "INSERT INTO admin (id, password_hash, created) VALUES (1, ?1, ?2)",
                params![hash, now()],
            )
            .map_err(|e| unique_conflict(e, "setup is already done"))?;
        Ok(())
    }

    /// Replaces the password hash and ends every session.
    pub fn set_admin_hash(&self, hash: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("UPDATE admin SET password_hash = ?1 WHERE id = 1", [hash])?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute("DELETE FROM sessions", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_session(&self, token_hash: &[u8]) -> Result<()> {
        let t = now();
        self.lock().execute(
            "INSERT INTO sessions (token_hash, created, seen) VALUES (?1, ?2, ?2)",
            params![token_hash, t],
        )?;
        Ok(())
    }

    /// The session's creation and last-seen times.
    pub fn session(&self, token_hash: &[u8]) -> Result<Option<(i64, i64)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT created, seen FROM sessions WHERE token_hash = ?1",
                [token_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn touch_session(&self, token_hash: &[u8], at: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET seen = ?2 WHERE token_hash = ?1",
            params![token_hash, at],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        self.lock()
            .execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    /// Drops sessions created before `created_before` or idle since before `seen_before`.
    pub fn prune_sessions(&self, created_before: i64, seen_before: i64) -> Result<usize> {
        Ok(self.lock().execute(
            "DELETE FROM sessions WHERE created < ?1 OR seen < ?2",
            params![created_before, seen_before],
        )?)
    }

    // Audit log: append only through this API.

    pub fn audit(&self, event: &str, detail: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO audit (at, event, detail) VALUES (?1, ?2, ?3)",
            params![now(), event, detail],
        )?;
        Ok(())
    }

    /// The newest `limit` audit entries, newest first.
    pub fn audit_log(&self, limit: u64) -> Result<Vec<(i64, String, String)>> {
        let conn = self.lock();
        let mut st =
            conn.prepare("SELECT at, event, detail FROM audit ORDER BY rowid DESC LIMIT ?1")?;
        let rows = st.query_map([to_i64(limit)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn unique_conflict(e: rusqlite::Error, why: &str) -> StoreError {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Conflict(why.into())
        }
        e => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("data/hopsync.db")).expect("open");
        (dir, store)
    }

    fn spec(name: &str) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            host: "seedbox.example.com".into(),
            port: 22,
            user: "me".into(),
            root: "/home/me".into(),
            key_file: Some("/home/me/.ssh/id_ed25519".into()),
            min_free: 10 << 30,
        }
    }

    fn job(server: i64, dst: &str) -> JobSpec {
        JobSpec {
            direction: Direction::Push,
            server,
            src: "Movies".into(),
            dst: dst.into(),
            existing: "skip".into(),
            verify: false,
            lanes: None,
        }
    }

    #[test]
    fn a_new_database_is_private_and_at_the_latest_schema() {
        let (dir, store) = open();
        assert_eq!(store.schema_version().expect("version"), 2);
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("data")), 0o700);
        assert_eq!(mode(&dir.path().join("data/hopsync.db")), 0o600);
        drop(store);
        let again = Store::open(&dir.path().join("data/hopsync.db")).expect("reopen");
        assert_eq!(again.schema_version().expect("version"), 2);
    }

    #[test]
    fn a_database_from_a_newer_hopsync_is_refused() {
        let (dir, store) = open();
        store
            .lock()
            .pragma_update(None, "user_version", 99)
            .expect("bump");
        drop(store);
        let err = Store::open(&dir.path().join("data/hopsync.db"))
            .err()
            .expect("refused");
        assert!(
            matches!(
                err,
                StoreError::TooNew {
                    found: 99,
                    known: 2
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn servers_round_trip_and_names_are_unique() {
        let (_d, store) = open();
        let s = store.add_server(&spec("box")).expect("add");
        assert_eq!(store.server(s.id).expect("get").name, "box");
        assert!(matches!(
            store.add_server(&spec("box")),
            Err(StoreError::Conflict(_))
        ));
        let mut changed = spec("box2");
        changed.port = 2222;
        let u = store.update_server(s.id, &changed).expect("update");
        assert_eq!((u.name.as_str(), u.port), ("box2", 2222));
        store
            .set_server_blocked(s.id, Some("host key changed"))
            .expect("block");
        assert_eq!(
            store.servers().expect("list")[0].blocked.as_deref(),
            Some("host key changed")
        );
    }

    #[test]
    fn a_server_with_unfinished_jobs_cannot_be_deleted() {
        let (_d, store) = open();
        let s = store.add_server(&spec("box")).expect("add");
        let secret = store.add_secret("key", b"sealed").expect("secret");
        store
            .set_server_key_secret(s.id, Some(secret))
            .expect("link");
        let j = store.add_job(&job(s.id, "in")).expect("job");
        assert!(matches!(
            store.delete_server(s.id),
            Err(StoreError::Conflict(_))
        ));
        store
            .set_job_state(j.id, JobState::Done, None)
            .expect("done");
        store.delete_server(s.id).expect("delete");
        assert!(matches!(store.server(s.id), Err(StoreError::NotFound)));
        assert!(matches!(store.job(j.id), Err(StoreError::NotFound)));
        assert!(matches!(store.secret(secret), Err(StoreError::NotFound)));
    }

    #[test]
    fn jobs_queue_in_order_and_refuse_a_shared_destination() {
        let (_d, store) = open();
        let s = store.add_server(&spec("box")).expect("add");
        let a = store.add_job(&job(s.id, "a")).expect("a");
        let b = store.add_job(&job(s.id, "b")).expect("b");
        assert!(matches!(
            store.add_job(&job(s.id, "a")),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.add_job(&job(s.id + 1, "c")),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(store.next_queued().expect("next").map(|j| j.id), Some(a.id));
        store.move_job(b.id, Some(a.id)).expect("move");
        assert_eq!(store.next_queued().expect("next").map(|j| j.id), Some(b.id));

        store
            .set_job_state(b.id, JobState::Running, None)
            .expect("run");
        let running = store.job(b.id).expect("job");
        assert!(running.started.is_some() && running.finished.is_none());
        assert_eq!(store.requeue_running().expect("requeue"), 1);
        assert_eq!(store.job(b.id).expect("job").state, JobState::Queued);

        store
            .set_job_state(a.id, JobState::Failed, Some("boom"))
            .expect("fail");
        let failed = store.job(a.id).expect("job");
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(failed.finished.is_some());
        // A failed job no longer holds its destination.
        store
            .add_job(&job(s.id, "a"))
            .expect("same destination again");
        assert_eq!(store.jobs(Some(JobState::Failed)).expect("list").len(), 1);
        assert!(matches!(
            store.delete_job(b.id),
            Err(StoreError::Conflict(_))
        ));
        store.delete_job(a.id).expect("delete finished");
    }

    #[test]
    fn items_and_events_belong_to_their_job() {
        let (_d, store) = open();
        let s = store.add_server(&spec("box")).expect("add");
        let j = store.add_job(&job(s.id, "a")).expect("job");
        let items: Vec<JobItem> = (0..5)
            .map(|i| JobItem {
                path: format!("f{i}"),
                outcome: "sent".into(),
                bytes: i,
                detail: format!("/r/f{i}"),
            })
            .collect();
        store.add_job_items(j.id, &items).expect("items");
        assert_eq!(store.job_items(j.id, 1, 2).expect("page"), items[1..3]);
        store
            .add_job_event(j.id, "lane 2 connected")
            .expect("event");
        assert_eq!(
            store.job_events(j.id).expect("events")[0].text,
            "lane 2 connected"
        );
        store.clear_job_items(j.id).expect("clear");
        assert!(store.job_items(j.id, 0, 10).expect("page").is_empty());
    }

    #[test]
    fn setup_runs_once_and_a_new_password_ends_sessions() {
        let (_d, store) = open();
        assert_eq!(store.admin_hash().expect("hash"), None);
        store.create_admin("h1").expect("setup");
        assert!(matches!(
            store.create_admin("h2"),
            Err(StoreError::Conflict(_))
        ));
        store.add_session(b"t").expect("session");
        assert!(store.session(b"t").expect("get").is_some());
        store.set_admin_hash("h3").expect("change");
        assert_eq!(store.admin_hash().expect("hash").as_deref(), Some("h3"));
        assert!(store.session(b"t").expect("get").is_none());
    }

    #[test]
    fn old_and_idle_sessions_are_pruned() {
        let (_d, store) = open();
        store.add_session(b"a").expect("a");
        store.add_session(b"b").expect("b");
        store.touch_session(b"b", now() + 100).expect("touch");
        // Sessions seen before now + 50 go: a, not b.
        assert_eq!(store.prune_sessions(0, now() + 50).expect("prune"), 1);
        assert!(store.session(b"a").expect("a").is_none());
        assert!(store.session(b"b").expect("b").is_some());
    }

    #[test]
    fn a_stop_request_outlives_the_daemon_only_while_the_job_runs() {
        let (_dir, store) = open();
        let s = store.add_server(&spec("a")).expect("add").id;
        let stopped = store.add_job(&job(s, "stopped")).expect("job").id;
        let finished = store.add_job(&job(s, "finished")).expect("job").id;
        let waiting = store.add_job(&job(s, "waiting")).expect("job").id;
        let running = store.add_job(&job(s, "running")).expect("job").id;
        for id in [stopped, finished, running] {
            store
                .set_job_state(id, JobState::Running, None)
                .expect("run");
        }
        store
            .want_job_stop(stopped, JobState::Paused)
            .expect("stop");
        store
            .want_job_stop(finished, JobState::Cancelled)
            .expect("stop");
        store
            .want_job_stop(waiting, JobState::Paused)
            .expect("ignored: it is not running");
        // The transfer ended on its own before it noticed the stop.
        store
            .set_job_state(finished, JobState::Done, None)
            .expect("done");

        assert_eq!(store.requeue_running().expect("requeue"), 1);
        let state = |id| store.job(id).expect("job").state;
        assert_eq!(state(stopped), JobState::Paused);
        assert_eq!(state(finished), JobState::Done);
        assert_eq!(state(waiting), JobState::Queued);
        assert_eq!(state(running), JobState::Queued);

        // Used up: a later run of the same job is not stopped by the old request.
        store
            .set_job_state(stopped, JobState::Running, None)
            .expect("run again");
        assert_eq!(store.requeue_running().expect("requeue"), 1);
        assert_eq!(state(stopped), JobState::Queued);
    }

    #[test]
    fn a_database_from_before_stop_requests_keeps_its_jobs() {
        let (dir, store) = open();
        let s = store.add_server(&spec("a")).expect("add").id;
        let id = store.add_job(&job(s, "kept")).expect("job").id;
        store
            .set_job_state(id, JobState::Running, None)
            .expect("run");
        {
            let conn = store.lock();
            conn.execute_batch(
                "ALTER TABLE jobs DROP COLUMN stop_wanted; PRAGMA user_version = 1;",
            )
            .expect("back to schema 1");
        }
        drop(store);

        let store = Store::open(&dir.path().join("data/hopsync.db")).expect("upgrade");
        assert_eq!(store.schema_version().expect("version"), 2);
        assert_eq!(store.requeue_running().expect("requeue"), 1);
        assert_eq!(store.job(id).expect("job").state, JobState::Queued);
    }

    #[test]
    fn settings_upsert() {
        let (_d, store) = open();
        assert_eq!(store.setting("verify").expect("get"), None);
        store.set_setting("verify", "false").expect("set");
        store.set_setting("verify", "true").expect("set");
        assert_eq!(
            store.setting("verify").expect("get").as_deref(),
            Some("true")
        );
    }
}
