use crate::{
    config::{ContextData, Guard},
    scan::{self, Entries, Fingerprint},
    secure,
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

pub struct Baseline {
    pub entries: Entries,
    pub guard: Guard,
}
fn calendar_digest(ctx: &ContextData) -> Result<String> {
    Ok(ctx
        .guards
        .iter()
        .find(|guard| guard.path == ctx.c.calendar)
        .context("Missing pinned calendar guard")?
        .digest
        .clone())
}
pub fn create(ctx: &ContextData) -> Result<()> {
    let c = &ctx.c;
    for guard in &ctx.guards {
        guard.verify(ctx.demo)?;
    }
    secure::trusted(c.database.parent().context("No baseline parent")?, ctx.demo)?;
    ensure!(
        !c.database.try_exists()?,
        "Baseline already exists; never auto-rebaseline"
    );
    let scanned = scan::run(ctx);
    ensure!(
        scanned.errors.is_empty(),
        "Cannot baseline incomplete/damaged tree: {:?}",
        scanned.errors
    );
    ensure!(
        scanned
            .entries
            .get(Path::new("index.html"))
            .is_some_and(|r| r.sha256 == secure::hash(ctx.template.as_bytes())),
        "Create/verify index.html against the trusted template before init"
    );
    let parent = c.database.parent().context("No baseline parent")?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut db = Connection::open(tmp.path())?;
        let tx = db.transaction()?;
        tx.execute_batch("PRAGMA user_version=3; CREATE TABLE baseline(path_hex TEXT PRIMARY KEY, record TEXT NOT NULL);\
            CREATE TABLE policy(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        for (path, record) in &scanned.entries {
            tx.execute(
                "INSERT INTO baseline VALUES(?1,?2)",
                params![secure::path_key(path), serde_json::to_string(record)?],
            )?;
        }
        for (k, v) in [
            ("calendar", calendar_digest(ctx)?),
            ("template", secure::hash(ctx.template.as_bytes())),
            ("timezone", c.timezone.clone()),
        ] {
            tx.execute("INSERT INTO policy VALUES(?1,?2)", params![k, v])?;
        }
        tx.commit()?;
    }
    // Do not publish a baseline if the policy changed while the tree was read.
    // The snapshot and the policy digests must describe the same trusted inputs.
    for guard in &ctx.guards {
        guard.verify(ctx.demo)?;
    }
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(&c.database).map_err(|e| e.error)?;
    secure::directory(parent)?.sync_all()?;
    Ok(())
}
pub fn load(ctx: &ContextData) -> Result<Baseline> {
    for guard in &ctx.guards {
        guard.verify(ctx.demo)?;
    }
    let (guard, bytes) = Guard::capture(&ctx.c.database, 128 * 1024 * 1024, ctx.demo)?;
    // Parse the EXACT validated bytes, not a later reopening of the live baseline path.
    // A private snapshot avoids depending on SQLite VFS resolution of /proc/self/fd.
    secure::private_dir(&ctx.c.state_dir)?;
    let mut snapshot = tempfile::NamedTempFile::new_in(&ctx.c.state_dir)?;
    snapshot.write_all(&bytes)?;
    snapshot.flush()?;
    let db = Connection::open_with_flags(
        snapshot.path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        version == 3,
        "Unsupported baseline version {version}; version 3 is required for file size and timestamps. Preserve the old database and audit, verify the website against trusted originals, configure a new database path and state_dir, then run init explicitly. Baselines are never automatically migrated or replaced"
    );
    for (k, expected) in [
        ("calendar", calendar_digest(ctx)?),
        ("template", secure::hash(ctx.template.as_bytes())),
        ("timezone", ctx.c.timezone.clone()),
    ] {
        let actual: String =
            db.query_row("SELECT value FROM policy WHERE key=?1", [k], |r| r.get(0))?;
        ensure!(actual == expected, "Baseline policy differs: {k}");
    }
    let mut entries = Entries::new();
    let mut stmt = db.prepare("SELECT path_hex,record FROM baseline ORDER BY path_hex")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        ensure!(
            entries.len() < ctx.c.max_entries,
            "Baseline entry limit exceeded"
        );
        let (encoded_path, json) = row?;
        let path = secure::key_path(&encoded_path)?;
        let record: Fingerprint = serde_json::from_str(&json)?;
        let ordinary_file = record.kind == "file" && path != Path::new("index.html");
        ensure!(
            (record.kind == "dir" && record.sha256.is_empty())
                || (record.kind == "file"
                    && record.sha256.len() == 64
                    && record
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "Invalid baseline record"
        );
        ensure!(record.mode <= 0o7777, "Invalid baseline permission bits");
        ensure!(
            record.size.is_some() == ordinary_file
                && record.modified.is_some() == ordinary_file
                && (ordinary_file || record.created.is_none())
                && record
                    .modified
                    .iter()
                    .chain(record.created.iter())
                    .all(|time| time.nanoseconds < 1_000_000_000),
            "Invalid baseline size/timestamp metadata for {path:?}"
        );
        ensure!(
            entries.insert(path, record).is_none(),
            "Duplicate canonical baseline path"
        );
    }
    ensure!(
        entries
            .get(Path::new(""))
            .is_some_and(|record| record.kind == "dir"),
        "Baseline root directory is missing"
    );
    for path in entries.keys().filter(|path| !path.as_os_str().is_empty()) {
        ensure!(
            path.parent()
                .and_then(|parent| entries.get(parent))
                .is_some_and(|parent| parent.kind == "dir"),
            "Baseline parent directory is missing or invalid for {path:?}"
        );
    }
    ensure!(
        entries
            .get(Path::new("index.html"))
            .is_some_and(|r| r.sha256 == secure::hash(ctx.template.as_bytes())),
        "Baseline does not match index template"
    );
    guard.verify(ctx.demo)?;
    for guard in &ctx.guards {
        guard.verify(ctx.demo)?;
    }
    Ok(Baseline { entries, guard })
}
