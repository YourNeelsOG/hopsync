//! Resume journal (plan.md §6.4, Phase 1 step 11).
//!
//! One SQLite database, one row per partly transferred file and one per durable chunk. The engine
//! records a chunk only after its data is durable on the destination (remote `fsync@`, local
//! `fdatasync`), so the journal can lag behind the data but never run ahead of it: losing a journal
//! write costs a redone chunk, never a hole.
//!
//! A file is resumed only if its source identity (size, mtime) matches the row; otherwise the row is
//! reset and the file starts over. Stitching old and new bytes would produce a file that never
//! existed. The row keeps the chunk size it was planned with, so a different lane count on the next
//! run still lines up with the recorded chunks.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug)]
pub struct JournalError(String);

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resume journal: {}", self.0)
    }
}

impl std::error::Error for JournalError {}

impl From<rusqlite::Error> for JournalError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        Self(e.to_string())
    }
}

/// What the journal knows about one file at the start of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub id: i64,
    /// Chunk size to plan with: the recorded one when resuming, else the one passed in.
    pub chunk_size: u64,
    /// Chunk indexes already durable at the destination.
    pub done: HashSet<u64>,
}

pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    /// Opens or creates the journal. The directory is created 0700 and the file 0600: it names
    /// hosts and paths.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
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
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 job TEXT NOT NULL,
                 dst TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 mtime INTEGER,
                 chunk_size INTEGER NOT NULL,
                 UNIQUE (job, dst)
             );
             CREATE TABLE IF NOT EXISTS chunks (
                 file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 idx INTEGER NOT NULL,
                 PRIMARY KEY (file, idx)
             ) WITHOUT ROWID;",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The default location: `$XDG_STATE_HOME/hopsync/journal.sqlite`, else
    /// `~/.local/state/hopsync/journal.sqlite`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(base.join("hopsync").join("journal.sqlite"))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, JournalError> {
        self.conn
            .lock()
            .map_err(|_| JournalError("lock poisoned".into()))
    }

    /// Starts or resumes `dst` within `job`. A row with a different source identity is replaced.
    pub fn begin(
        &self,
        job: &str,
        dst: &str,
        size: u64,
        mtime: Option<i64>,
        chunk_size: u64,
    ) -> Result<Resume, JournalError> {
        let conn = self.lock()?;
        let row: Option<(i64, i64, Option<i64>, i64)> = conn
            .query_row(
                "SELECT id, size, mtime, chunk_size FROM files WHERE job = ?1 AND dst = ?2",
                params![job, dst],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((id, rsize, rmtime, rchunk)) = row {
            let same = u64::try_from(rsize).ok() == Some(size) && rmtime == mtime;
            if same && let Ok(cs) = u64::try_from(rchunk) {
                let mut stmt = conn.prepare("SELECT idx FROM chunks WHERE file = ?1")?;
                let done = stmt
                    .query_map([id], |r| r.get::<_, i64>(0))?
                    .filter_map(|r| r.ok().and_then(|i| u64::try_from(i).ok()))
                    .collect();
                return Ok(Resume {
                    id,
                    chunk_size: cs,
                    done,
                });
            }
            conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        }
        conn.execute(
            "INSERT INTO files (job, dst, size, mtime, chunk_size) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![job, dst, to_i64(size)?, mtime, to_i64(chunk_size)?],
        )?;
        Ok(Resume {
            id: conn.last_insert_rowid(),
            chunk_size,
            done: HashSet::new(),
        })
    }

    /// Records chunk `idx` as durable at the destination.
    pub fn mark_done(&self, id: i64, idx: u64) -> Result<(), JournalError> {
        self.lock()?.execute(
            "INSERT OR IGNORE INTO chunks (file, idx) VALUES (?1, ?2)",
            params![id, to_i64(idx)?],
        )?;
        Ok(())
    }

    /// Records several durable chunks in one transaction.
    pub fn mark_done_many(&self, id: i64, idx: &[u64]) -> Result<(), JournalError> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        {
            let mut st =
                tx.prepare_cached("INSERT OR IGNORE INTO chunks (file, idx) VALUES (?1, ?2)")?;
            for &i in idx {
                st.execute(params![id, to_i64(i)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the recorded chunks of `id` but keeps the row (the file restarts under the same id).
    pub fn reset(&self, id: i64) -> Result<(), JournalError> {
        self.lock()?
            .execute("DELETE FROM chunks WHERE file = ?1", [id])?;
        Ok(())
    }

    /// Removes the file's row, after it is complete or abandoned.
    pub fn forget(&self, id: i64) -> Result<(), JournalError> {
        self.lock()?
            .execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn to_i64(v: u64) -> Result<i64, JournalError> {
    i64::try_from(v).map_err(|_| JournalError(format!("{v} does not fit the journal")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn journal() -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().expect("tempdir");
        let j = Journal::open(&dir.path().join("state/journal.sqlite")).expect("open");
        (dir, j)
    }

    #[test]
    fn records_survive_reopen_and_resume_with_matching_identity() {
        let (dir, j) = journal();
        let r = j.begin("job", "a/f", 100, Some(7), 10).expect("begin");
        assert!(r.done.is_empty());
        j.mark_done(r.id, 0).expect("mark");
        j.mark_done(r.id, 3).expect("mark");
        j.mark_done(r.id, 3).expect("mark twice is fine");
        drop(j);
        let j = Journal::open(&dir.path().join("state/journal.sqlite")).expect("reopen");
        let again = j.begin("job", "a/f", 100, Some(7), 10).expect("begin");
        assert_eq!(again.id, r.id);
        assert_eq!(again.done, HashSet::from([0, 3]));
    }

    #[test]
    fn changed_source_starts_over() {
        let (_d, j) = journal();
        let r = j.begin("job", "f", 100, Some(7), 10).expect("begin");
        j.mark_done(r.id, 1).expect("mark");
        for (size, mtime) in [(101, Some(7)), (100, Some(8)), (100, None)] {
            let n = j.begin("job", "f", size, mtime, 10).expect("begin");
            assert!(
                n.done.is_empty(),
                "size {size} mtime {mtime:?} must not resume"
            );
            j.mark_done(n.id, 1).expect("mark");
        }
    }

    #[test]
    fn recorded_chunk_size_wins_over_a_new_plan() {
        let (_d, j) = journal();
        let r = j.begin("job", "f", 1000, None, 64).expect("begin");
        j.mark_done(r.id, 2).expect("mark");
        let again = j.begin("job", "f", 1000, None, 8).expect("begin");
        assert_eq!((again.chunk_size, again.done.len()), (64, 1));
    }

    #[test]
    fn jobs_are_separate_and_forget_and_reset_clear() {
        let (_d, j) = journal();
        let a = j.begin("push|h|/x", "f", 10, None, 1).expect("a");
        let b = j.begin("pull|h|/x", "f", 10, None, 1).expect("b");
        assert_ne!(a.id, b.id);
        j.mark_done(a.id, 0).expect("mark");
        j.mark_done(b.id, 0).expect("mark");
        j.reset(a.id).expect("reset");
        assert!(
            j.begin("push|h|/x", "f", 10, None, 1)
                .expect("a")
                .done
                .is_empty()
        );
        j.forget(b.id).expect("forget");
        let b2 = j.begin("pull|h|/x", "f", 10, None, 1).expect("b2");
        assert!(b2.done.is_empty());
    }

    #[test]
    fn journal_file_and_directory_are_private() {
        let (dir, _j) = journal();
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("state")), 0o700);
        assert_eq!(mode(&dir.path().join("state/journal.sqlite")), 0o600);
    }
}
