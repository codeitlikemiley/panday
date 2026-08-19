//! An append-only file event store — the on-disk form of the log (M21.3).
//!
//! `panday replay` is specified as taking a `<session_id>` and looking the log
//! up in the store (docs/21 §The replay tool). Postgres is M3.5 and SQLite is
//! M18.1, so in Phase 2 there is nowhere to look a session up *from*. Rather
//! than ship a replay tool with no reachable input, this is the smallest store
//! that makes the fold observable: one JSON envelope per line, in `seq` order,
//! which is already the shape of the golden fixtures.
//!
//! It is deliberately not a general database — no indexes, no compaction, one
//! session per file. `read_after` scans. That is fine for a debugging artifact
//! and honest about what it is.

use crate::{EventStore, StoreError};
use panday_types::event::Envelope;
use panday_types::SessionId;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct JsonlStore {
    path: PathBuf,
    // Serializes writers, which is what keeps `seq` gapless. Two processes
    // appending to one file would break that, and no file lock can make
    // concurrent agents agree on the next seq — so the invariant is "one
    // writer per file", enforced here for one process and documented for the
    // rest.
    file: Mutex<File>,
}

impl JsonlStore {
    /// Open (creating if absent) a log file for appending.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError::Io(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read a log file back. Unparseable lines are an error rather than a skip:
/// a replay that silently dropped events would show a session that never
/// happened, which is worse than no replay at all. (Events from a *newer*
/// version still parse — `Event::Unknown` — so this only rejects real
/// corruption.)
pub fn read_log(path: impl AsRef<Path>) -> Result<Vec<Envelope>, StoreError> {
    let path = path.as_ref();
    let f = File::open(path).map_err(|e| StoreError::Io(format!("{}: {e}", path.display())))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line.map_err(|e| StoreError::Io(format!("{}: {e}", path.display())))?;
        if line.trim().is_empty() {
            continue;
        }
        let env: Envelope = serde_json::from_str(&line).map_err(|e| {
            StoreError::Io(format!("{}:{}: not an event: {e}", path.display(), i + 1))
        })?;
        out.push(env);
    }
    // A gap means the log is not a log — say so here rather than letting a
    // fold quietly produce a state no session ever held (ADR-002).
    for w in out.windows(2) {
        if w[1].seq != w[0].seq + 1 {
            return Err(StoreError::SeqConflict(w[1].seq));
        }
    }
    Ok(out)
}

#[async_trait::async_trait]
impl EventStore for JsonlStore {
    async fn append(&self, e: Envelope) -> Result<(), StoreError> {
        let line = serde_json::to_string(&e)
            .map_err(|err| StoreError::Io(format!("serialize event: {err}")))?;
        let mut f = self.file.lock().unwrap();
        f.write_all(line.as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .map_err(|err| StoreError::Io(format!("{}: {err}", self.path.display())))?;
        // docs/13 §persist-before-proceed: `append` is fsync-durable before it
        // returns. Without this the harness would resume from a log that lost
        // its last turn to the page cache — exactly the crash it is designed
        // to survive.
        f.sync_data()
            .map_err(|err| StoreError::Io(format!("{}: fsync: {err}", self.path.display())))?;
        Ok(())
    }

    async fn read_after(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> Result<Vec<Envelope>, StoreError> {
        Ok(read_log(&self.path)?
            .into_iter()
            .filter(|e| e.session_id == session && e.seq > after_seq)
            .collect())
    }

    async fn next_seq(&self, session: SessionId) -> Result<u64, StoreError> {
        Ok(read_log(&self.path)?
            .iter()
            .filter(|e| e.session_id == session)
            .map(|e| e.seq)
            .max()
            .unwrap_or(0)
            + 1)
    }
}
