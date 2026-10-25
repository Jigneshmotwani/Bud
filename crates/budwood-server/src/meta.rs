//! The changeset log and derived indexes, stored in SQLite.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use budwood_core::{Changeset, ObjectId, RepoPath};
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
    name       TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS changesets (
    repo      TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    id        TEXT NOT NULL,
    root_tree TEXT NOT NULL,
    body      TEXT NOT NULL,
    PRIMARY KEY (repo, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS changesets_by_id ON changesets (repo, id);
-- Derived data: which changesets touched each path. Answers both
-- "history of a path" and "what changed between two revisions".
CREATE TABLE IF NOT EXISTS path_history (
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    seq  INTEGER NOT NULL,
    PRIMARY KEY (repo, path, seq)
);
CREATE INDEX IF NOT EXISTS path_history_by_seq ON path_history (repo, seq);
-- Memoized derived data (see derived.rs), keyed by (repo, kind, key). Values
-- are pure functions of history, so rows are never updated, only added.
CREATE TABLE IF NOT EXISTS derived (
    repo  TEXT NOT NULL,
    kind  TEXT NOT NULL,
    key   TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (repo, kind, key)
);
"#;

/// The fields of a changeset needed to attribute something to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetSummary {
    pub id: ObjectId,
    pub author: String,
    pub timestamp: i64,
}

pub struct MetaStore {
    conn: Mutex<Connection>,
}

/// Returned by [`MetaStore::append`] when another changeset took the slot.
#[derive(Debug, thiserror::Error)]
#[error("head moved")]
pub struct HeadMoved;

impl MetaStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA).context("creating schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns false if the repository already existed.
    pub fn create_repo(&self, name: &str, now: i64) -> Result<bool> {
        let n = self
            .conn()
            .execute("INSERT OR IGNORE INTO repos (name, created_at) VALUES (?1, ?2)", params![name, now])?;
        Ok(n == 1)
    }

    pub fn repo_exists(&self, name: &str) -> Result<bool> {
        Ok(self.conn().query_row("SELECT 1 FROM repos WHERE name = ?1", [name], |_| Ok(())).optional()?.is_some())
    }

    pub fn list_repos(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name FROM repos ORDER BY name")?;
        let names = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(names)
    }

    /// The newest changeset, or `None` if the repository is empty.
    pub fn head(&self, repo: &str) -> Result<Option<Changeset>> {
        self.one("SELECT body FROM changesets WHERE repo = ?1 ORDER BY seq DESC LIMIT 1", params![repo])
    }

    pub fn by_seq(&self, repo: &str, seq: u64) -> Result<Option<Changeset>> {
        self.one("SELECT body FROM changesets WHERE repo = ?1 AND seq = ?2", params![repo, seq as i64])
    }

    pub fn by_id(&self, repo: &str, id: &ObjectId) -> Result<Option<Changeset>> {
        self.one("SELECT body FROM changesets WHERE repo = ?1 AND id = ?2", params![repo, id.to_hex()])
    }

    /// Changesets older than `before` that touched `path` (or anything under
    /// it), newest first.
    pub fn log(&self, repo: &str, path: &RepoPath, before: Option<u64>, limit: usize) -> Result<Vec<Changeset>> {
        let before = before.map_or(i64::MAX, |b| b as i64);
        let limit = limit as i64;
        if path.is_root() {
            return self.many(
                "SELECT body FROM changesets WHERE repo = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
                params![repo, before, limit],
            );
        }
        // `p/` <= x < `p0` selects every path under `p/`, since '0' follows '/'.
        let (lo, hi) = (format!("{}/", path.as_str()), format!("{}0", path.as_str()));
        self.many(
            "SELECT body FROM changesets WHERE repo = ?1 AND seq < ?2 AND seq IN (
                 SELECT seq FROM path_history
                 WHERE repo = ?1 AND (path = ?3 OR (path >= ?4 AND path < ?5))
             ) ORDER BY seq DESC LIMIT ?6",
            params![repo, before, path.as_str(), lo, hi, limit],
        )
    }

    /// The root tree of changeset `seq`, without decoding the changeset body.
    pub fn root_tree_at(&self, repo: &str, seq: u64) -> Result<Option<ObjectId>> {
        let hex: Option<String> = self
            .conn()
            .query_row(
                "SELECT root_tree FROM changesets WHERE repo = ?1 AND seq = ?2",
                params![repo, seq as i64],
                |row| row.get(0),
            )
            .optional()?;
        hex.map(|h| ObjectId::from_hex(&h).context("decoding root tree id")).transpose()
    }

    /// Id, author and timestamp of changeset `seq`. SQLite extracts the two
    /// fields from the stored JSON, so a huge `changes` list isn't decoded.
    pub fn summary(&self, repo: &str, seq: u64) -> Result<Option<ChangesetSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id, json_extract(body, '$.author'), json_extract(body, '$.timestamp')
             FROM changesets WHERE repo = ?1 AND seq = ?2",
        )?;
        let row = stmt
            .query_row(params![repo, seq as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
            })
            .optional()?;
        row.map(|(id, author, timestamp)| {
            Ok(ChangesetSummary { id: ObjectId::from_hex(&id).context("decoding changeset id")?, author, timestamp })
        })
        .transpose()
    }

    /// The newest changeset at or before `upto` that changed exactly `path`.
    pub fn last_touch(&self, repo: &str, path: &RepoPath, upto: u64) -> Result<Option<u64>> {
        let seq: Option<i64> = self.conn().query_row(
            "SELECT MAX(seq) FROM path_history WHERE repo = ?1 AND path = ?2 AND seq <= ?3",
            params![repo, path.as_str(), upto.min(i64::MAX as u64) as i64],
            |row| row.get(0),
        )?;
        Ok(seq.map(|s| s as u64))
    }

    pub fn derived_get(&self, repo: &str, kind: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT value FROM derived WHERE repo = ?1 AND kind = ?2 AND key = ?3")?;
        Ok(stmt.query_row(params![repo, kind, key], |row| row.get(0)).optional()?)
    }

    /// Stores a derived value. Derivations are deterministic, so when two
    /// requests race to compute the same key the first one wins.
    pub fn derived_put(&self, repo: &str, kind: &str, key: &str, value: &str) -> Result<()> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("INSERT OR IGNORE INTO derived (repo, kind, key, value) VALUES (?1, ?2, ?3, ?4)")?;
        stmt.execute(params![repo, kind, key, value])?;
        Ok(())
    }

    /// Number of memoized values of `kind` in `repo`.
    pub fn derived_count(&self, repo: &str, kind: &str) -> Result<u64> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM derived WHERE repo = ?1 AND kind = ?2",
            params![repo, kind],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// Every path changed by changesets in `(after, upto]`.
    pub fn touched_paths(&self, repo: &str, after: u64, upto: u64) -> Result<Vec<RepoPath>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT DISTINCT path FROM path_history WHERE repo = ?1 AND seq > ?2 AND seq <= ?3")?;
        let rows = stmt.query_map(params![repo, after as i64, upto as i64], |row| row.get::<_, String>(0))?;
        rows.map(|r| Ok(RepoPath::new(r?)?)).collect()
    }

    /// Appends `cs` if it directly follows the current head; the compare-and-
    /// swap that makes commits safe without a global lock.
    pub fn append(&self, repo: &str, cs: &Changeset) -> Result<Result<(), HeadMoved>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let head: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM changesets WHERE repo = ?1", [repo], |row| row.get(0))?;
        if head as u64 + 1 != cs.seq {
            return Ok(Err(HeadMoved));
        }
        tx.execute(
            "INSERT INTO changesets (repo, seq, id, root_tree, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo, cs.seq as i64, cs.id.to_hex(), cs.root_tree.to_hex(), serde_json::to_string(cs)?],
        )?;
        {
            let mut insert = tx.prepare("INSERT INTO path_history (repo, path, seq) VALUES (?1, ?2, ?3)")?;
            for change in &cs.changes {
                insert.execute(params![repo, change.path.as_str(), cs.seq as i64])?;
            }
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    fn one(&self, sql: &str, params: impl rusqlite::Params) -> Result<Option<Changeset>> {
        let body: Option<String> = self.conn().query_row(sql, params, |row| row.get(0)).optional()?;
        body.map(|b| serde_json::from_str(&b).context("decoding changeset")).transpose()
    }

    fn many(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Changeset>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(sql)?;
        let bodies = stmt.query_map(params, |row| row.get::<_, String>(0))?;
        bodies.map(|b| serde_json::from_str(&b?).context("decoding changeset")).collect()
    }
}
