//! Commands `init`, `check` and `watch`: compare the website with the baseline,
//! open/close incidents and queue alerts.
use crate::{alert, daily, db, scan, App, Issue};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::{collections::BTreeMap, thread, time::Duration};

/// Scans the site; index.html is hashed only after its daily block has been verified.
fn scan_site(app: &App) -> (scan::Snapshot, Vec<Issue>) {
    let tz = app.cfg.timezone;
    let index = |bytes: &[u8], mtime: i64| -> Result<Vec<u8>> {
        let written = DateTime::from_timestamp(mtime, 0).unwrap_or_default();
        let dates =
            daily::accepted_dates(Utc::now().with_timezone(&tz), written.with_timezone(&tz));
        let html = std::str::from_utf8(bytes)?;
        Ok(daily::normalize(html, &app.names, &dates)?.into_bytes())
    };
    scan::scan(&app.cfg.site_dir, &index)
}

/// `init`: record a known-good site. index.html must be today's render of the template.
pub fn init(app: &App) -> Result<()> {
    let template = daily::load_template(&app.cfg.template)?;
    let (files, issues) = scan_site(app);
    ensure!(
        issues.is_empty(),
        "cannot create a baseline from a damaged site: {issues:?}"
    );
    ensure!(
        files.get("index.html").map(|e| &e.sha256) == Some(&scan::sha256(template.as_bytes())),
        "index.html does not match the template; run `page` first"
    );
    db::save_baseline(&mut db::open(&app.cfg.database)?, &files)?;
    app.log
        .write("BASELINE_CREATED", &format!("{} entries", files.len()));
    Ok(())
}

/// Lists every difference between the baseline and the current scan.
pub fn compare(base: &scan::Snapshot, now: &scan::Snapshot, problems: Vec<Issue>) -> Vec<Issue> {
    // A new file that cannot even be read is still a new file.
    let mut issues: Vec<Issue> = problems
        .into_iter()
        .map(|i| match i.kind.as_str() {
            "UNREADABLE" if !base.contains_key(&i.path) => {
                Issue::new("ADDED", &i.path, format!("unreadable: {}", i.detail))
            }
            _ => i,
        })
        .collect();
    for (path, old) in base {
        let Some(new) = now.get(path) else {
            // A file that could not be read (or sits in such a directory) is already reported.
            let hidden = |i: &Issue| {
                &i.path == path || i.path == "." || path.starts_with(&format!("{}/", i.path))
            };
            if !issues.iter().any(hidden) {
                issues.push(Issue::new("DELETED", path, String::new()));
            }
            continue;
        };
        let mut changes = Vec::new();
        if new.size != old.size {
            changes.push(format!("size {} -> {}", old.size, new.size));
        }
        if new.mtime != old.mtime {
            changes.push(format!("mtime {} -> {}", old.mtime, new.mtime));
        }
        if new.mode != old.mode {
            changes.push(format!("mode {:04o} -> {:04o}", old.mode, new.mode));
        }
        if (new.uid, new.gid) != (old.uid, old.gid) {
            changes.push(format!(
                "owner {}:{} -> {}:{}",
                old.uid, old.gid, new.uid, new.gid
            ));
        }
        if new.sha256 != old.sha256 {
            changes.insert(0, "content (SHA-256)".into());
            issues.push(Issue::new("MODIFIED", path, changes.join(", ")));
        } else if !changes.is_empty() {
            issues.push(Issue::new("METADATA", path, changes.join(", ")));
        }
    }
    for path in now.keys().filter(|p| !base.contains_key(*p)) {
        issues.push(Issue::new("ADDED", path, String::new()));
    }
    issues
}

fn message(title: &str, issue: &Issue, time: &str, host: &str) -> String {
    let detail = if issue.detail.is_empty() {
        "-"
    } else {
        &issue.detail
    };
    format!(
        "{title}\nTyp: {}\nSúbor: {}\nDetail: {detail}\nČas: {time}\nServer: {host}",
        issue.kind, issue.path
    )
}

/// One monitoring cycle. Returns the number of open incidents.
/// Only changes produce alerts: a new incident once, its recovery once.
pub fn cycle(app: &App, db: &Connection, send: &mut alert::Send) -> Result<usize> {
    let base = db::load_baseline(db)?;
    let (files, problems) = scan_site(app);
    let current: BTreeMap<String, Issue> = compare(&base, &files, problems)
        .into_iter()
        .map(|i| (i.key(), i))
        .collect();
    let open: BTreeMap<String, Issue> = db::open_incidents(db)?
        .into_iter()
        .map(|i| (i.key(), i))
        .collect();
    let time = app.log.now();
    let host = hostname();
    for (key, issue) in &current {
        if !open.contains_key(key) {
            db::add_incident(db, issue, &time)?;
            app.log.write(
                "INCIDENT",
                &format!("{} {} {}", issue.kind, issue.path, issue.detail),
            );
            db::queue_alert(db, &message("FIM: INCIDENT", issue, &time, &host))?;
        }
    }
    for (key, issue) in &open {
        if !current.contains_key(key) {
            db::close_incident(db, issue)?;
            app.log
                .write("RECOVERED", &format!("{} {}", issue.kind, issue.path));
            db::queue_alert(db, &message("FIM: obnovené", issue, &time, &host))?;
        }
    }
    alert::flush(db, &app.log, send)?;
    Ok(current.len())
}

/// `check`: one cycle; exit code 0 = clean, 1 = open incidents.
pub fn check(app: &App) -> Result<i32> {
    let db = db::open(&app.cfg.database)?;
    let open = cycle(app, &db, &mut alert::telegram(&app.cfg))?;
    app.log
        .write("CHECK_DONE", &format!("{open} open incident(s)"));
    Ok(if open == 0 { 0 } else { 1 })
}

/// `watch`: a cycle every `interval_seconds`. Errors are logged and the loop continues.
pub fn watch(app: &App) -> Result<i32> {
    let db = db::open(&app.cfg.database)?;
    db::load_baseline(&db)?; // refuse to start without a baseline
    let mut send = alert::telegram(&app.cfg);
    app.log.write(
        "WATCH_STARTED",
        &format!("every {} s", app.cfg.interval_seconds),
    );
    loop {
        if let Err(e) = cycle(app, &db, &mut send) {
            app.log.write("ERROR", &format!("{e:#}"));
        }
        thread::sleep(Duration::from_secs(app.cfg.interval_seconds));
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}
