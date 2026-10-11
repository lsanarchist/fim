use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};
use crate::{config::{ContextData, Guard}, scan::{self, Entries, Fingerprint}, secure};

pub struct Baseline { pub entries: Entries, pub guard: Guard }
pub fn create(ctx: &ContextData) -> Result<()> {
    let c = &ctx.c;
    secure::trusted(c.database.parent().context("No baseline parent")?, ctx.demo)?;
    ensure!(!c.database.try_exists()?, "Baseline already exists; never auto-rebaseline");
    let scanned = scan::run(ctx);
    ensure!(scanned.errors.is_empty(), "Cannot baseline incomplete/damaged tree: {:?}", scanned.errors);
    ensure!(scanned.entries.get(Path::new("index.html")).is_some_and(|r| r.sha256 == secure::hash(ctx.template.as_bytes())),
        "Create/verify index.html against the trusted template before init");
    let parent = c.database.parent().context("No baseline parent")?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut db = Connection::open(tmp.path())?;
        let tx = db.transaction()?;
        tx.execute_batch("PRAGMA user_version=2; CREATE TABLE baseline(path_hex TEXT PRIMARY KEY, record TEXT NOT NULL);\
            CREATE TABLE policy(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        for (path, record) in &scanned.entries {
            tx.execute("INSERT INTO baseline VALUES(?1,?2)", params![secure::path_key(path), serde_json::to_string(record)?])?;
        }
        for (k,v) in [("calendar", secure::hash(&secure::read_small(&c.calendar,1024*1024)?)),
                      ("template", secure::hash(ctx.template.as_bytes())), ("timezone", c.timezone.clone())] {
            tx.execute("INSERT INTO policy VALUES(?1,?2)", params![k,v])?;
        }
        tx.commit()?;
    }
    tmp.as_file().set_permissions(fs::Permissions::from_mode(0o644))?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(&c.database).map_err(|e| e.error)?;
    secure::directory(parent)?.sync_all()?;
    Ok(())
}
pub fn load(ctx: &ContextData) -> Result<Baseline> {
    let (guard, bytes) = Guard::capture(&ctx.c.database, 128*1024*1024, ctx.demo)?;
    // Parse the EXACT validated bytes, not a later reopening of the live baseline path.
    // A private snapshot avoids depending on SQLite VFS resolution of /proc/self/fd.
    secure::private_dir(&ctx.c.state_dir)?;
    let mut snapshot = tempfile::NamedTempFile::new_in(&ctx.c.state_dir)?;
    snapshot.write_all(&bytes)?;
    snapshot.flush()?;
    let db = Connection::open_with_flags(snapshot.path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(version == 2, "Unsupported baseline version; v1 is intentionally not auto-migrated");
    for (k, expected) in [("calendar", secure::hash(&secure::read_small(&ctx.c.calendar,1024*1024)?)),
                          ("template", secure::hash(ctx.template.as_bytes())), ("timezone",ctx.c.timezone.clone())] {
        let actual: String = db.query_row("SELECT value FROM policy WHERE key=?1", [k], |r| r.get(0))?;
        ensure!(actual == expected, "Baseline policy differs: {k}");
    }
    let mut entries = Entries::new();
    let mut stmt = db.prepare("SELECT path_hex,record FROM baseline ORDER BY path_hex")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
    for row in rows {
        ensure!(entries.len() < ctx.c.max_entries, "Baseline entry limit exceeded");
        let (path, json) = row?;
        let record: Fingerprint = serde_json::from_str(&json)?;
        ensure!(record.kind == "dir" || (record.kind == "file" && record.sha256.len()==64
            && record.sha256.bytes().all(|b| b.is_ascii_hexdigit())), "Invalid baseline record");
        entries.insert(secure::key_path(&path)?, record);
    }
    ensure!(entries.get(Path::new("index.html")).is_some_and(|r| r.sha256==secure::hash(ctx.template.as_bytes())),
        "Baseline does not match index template");
    guard.verify(ctx.demo)?;
    Ok(Baseline { entries, guard })
}
