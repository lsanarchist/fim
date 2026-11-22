//! Command `page`: writes today's date and name day into index.html.
//!
//! If a baseline exists, the page is only rewritten when everything outside the daily
//! block still matches it, so a tampered page is never silently "repaired".
use crate::{daily, db, scan::sha256, App};
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

pub fn page(app: &App) -> Result<()> {
    let result = publish(app);
    match &result {
        Ok(text) => app.log.write("PAGE_UPDATED", text),
        Err(e) => app.log.write("PAGE_REFUSED", &format!("{e:#}")),
    }
    result.map(|_| ())
}

fn publish(app: &App) -> Result<String> {
    let cfg = &app.cfg;
    let template = daily::load_template(&cfg.template)?;
    let index = cfg.site_dir.join("index.html");
    if cfg.database.exists() {
        let db = db::open_read_only(&cfg.database)?;
        if let Some(expected) = db::baseline_hash(&db, "index.html")? {
            ensure!(
                sha256(template.as_bytes()) == expected,
                "template differs from the baseline"
            );
            let current = fs::read_to_string(&index).context("cannot read index.html")?;
            ensure!(
                sha256(daily::strip(&current)?.as_bytes()) == expected,
                "index.html differs from the baseline outside the daily block"
            );
        }
    }
    let today = Utc::now().with_timezone(&cfg.timezone).date_naive();
    let html = daily::render(&template, &app.names, today)?;
    // The temporary file lives next to the site directory (same filesystem, so `rename`
    // is atomic) but outside it, so the monitor never sees a half-written file.
    let site_name = cfg.site_dir.file_name().context("site_dir has no name")?;
    let parent = cfg.site_dir.parent().context("site_dir has no parent")?;
    let tmp = parent.join(format!(".{}-index.tmp", site_name.to_string_lossy()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(0o644))?; // independent of umask
    file.write_all(html.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &index).context("cannot replace index.html")?;
    daily::text(&app.names, today)
}
