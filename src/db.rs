//! SQLite storage: the baseline, currently open incidents and unsent alerts.
use crate::{
    scan::{Entry, Snapshot},
    Issue,
};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::{path::Path, time::Duration};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS baseline(
    path TEXT PRIMARY KEY, sha256 TEXT NOT NULL, size INTEGER NOT NULL,
    mtime INTEGER NOT NULL, mode INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS incidents(
    key TEXT PRIMARY KEY, kind TEXT NOT NULL, path TEXT NOT NULL,
    detail TEXT NOT NULL, since TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS outbox(
    id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, last_error TEXT);";

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?; // `check` may run next to `watch`
    db.execute_batch(SCHEMA)?;
    Ok(db)
}

/// Read-only access, used by `page` (it never changes the database).
pub fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// Stores the baseline once. It is never overwritten automatically.
pub fn save_baseline(db: &mut Connection, files: &Snapshot) -> Result<()> {
    let count: i64 = db.query_row("SELECT COUNT(*) FROM baseline", [], |r| r.get(0))?;
    ensure!(
        count == 0,
        "baseline already exists; delete the database to create a new one"
    );
    let tx = db.transaction()?;
    for (path, e) in files {
        tx.execute(
            "INSERT INTO baseline VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, e.sha256, e.size, e.mtime, e.mode],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn load_baseline(db: &Connection) -> Result<Snapshot> {
    let mut stmt = db.prepare("SELECT path, sha256, size, mtime, mode FROM baseline")?;
    let rows = stmt.query_map([], |r| {
        let entry = Entry {
            sha256: r.get(1)?,
            size: r.get(2)?,
            mtime: r.get(3)?,
            mode: r.get(4)?,
        };
        Ok((r.get(0)?, entry))
    })?;
    let files = rows.collect::<rusqlite::Result<Snapshot>>()?;
    ensure!(!files.is_empty(), "baseline is empty; run `init` first");
    Ok(files)
}

pub fn baseline_hash(db: &Connection, path: &str) -> Result<Option<String>> {
    let sql = "SELECT sha256 FROM baseline WHERE path = ?1";
    Ok(db.query_row(sql, [path], |r| r.get(0)).optional()?)
}

pub fn open_incidents(db: &Connection) -> Result<Vec<Issue>> {
    let mut stmt = db.prepare("SELECT kind, path, detail FROM incidents ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        Ok(Issue {
            kind: r.get(0)?,
            path: r.get(1)?,
            detail: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn add_incident(db: &Connection, issue: &Issue, since: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO incidents VALUES(?1, ?2, ?3, ?4, ?5)",
        params![issue.key(), issue.kind, issue.path, issue.detail, since],
    )?;
    Ok(())
}

pub fn close_incident(db: &Connection, issue: &Issue) -> Result<()> {
    db.execute("DELETE FROM incidents WHERE key = ?1", [issue.key()])?;
    Ok(())
}

pub fn queue_alert(db: &Connection, text: &str) -> Result<()> {
    db.execute("INSERT INTO outbox(text) VALUES(?1)", [text])?;
    Ok(())
}

/// Unsent alerts, oldest first.
pub fn pending_alerts(db: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = db.prepare("SELECT id, text FROM outbox ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn alert_sent(db: &Connection, id: i64) -> Result<()> {
    db.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
    Ok(())
}

pub fn alert_failed(db: &Connection, id: i64, error: &str) -> Result<()> {
    db.execute(
        "UPDATE outbox SET last_error = ?2 WHERE id = ?1",
        params![id, error],
    )?;
    Ok(())
}
