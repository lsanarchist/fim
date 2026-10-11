use crate::{baseline, config::ContextData, daily, journal, secure};
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use notify::{
    event::{ModifyKind, RenameMode},
    Event, EventKind,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Receipt {
    at: i64,
    stage: PathBuf,
    inode: u64,
    dev: u64,
    sha256: String,
}
fn verify_old(ctx: &ContextData, bytes: &[u8], expected: &str) -> Result<()> {
    let date = Utc::now().with_timezone(&ctx.tz).date_naive();
    let normalized = daily::normalize_dates(
        std::str::from_utf8(bytes)?,
        &ctx.names,
        &[date, date.pred_opt().context("Date underflow")?],
    )?;
    ensure!(
        secure::hash(normalized.as_bytes()) == expected,
        "Refusing to overwrite changed index.html"
    );
    Ok(())
}
pub fn page(ctx: &ContextData) -> Result<()> {
    let _lock = secure::lock(&ctx.c.state_dir.join("publisher.lock"))?;
    let result = publish(ctx);
    match &result {
        Ok(()) => {
            let _ = journal::external(
                &ctx.c,
                "PAGE_UPDATED",
                "Page verified/published".into(),
                false,
            );
        }
        Err(e) => {
            if let Err(q) = journal::external(&ctx.c, "PUBLISH_FAILED", format!("{e:#}"), true) {
                eprintln!("FIM_PUBLISH_FAILED {e:#}; cannot persist alert: {q:#}");
            }
        }
    }
    result
}
fn publish(ctx: &ContextData) -> Result<()> {
    for guard in &ctx.guards {
        guard.verify(ctx.demo)?;
    }
    let root = secure::directory(&ctx.c.root)?;
    let stage = secure::directory(&ctx.c.staging_dir)?;
    ensure!(
        root.metadata()?.dev() == stage.metadata()?.dev(),
        "Staging and site must share a filesystem"
    );
    let index = ctx.c.root.join("index.html");
    let date = Utc::now().with_timezone(&ctx.tz).date_naive();
    // Reject oversized escaped content before creating a candidate or replacing the public file.
    let html = daily::render(&ctx.template, &ctx.names, date, ctx.c.max_index_bytes)?;
    let base = if ctx.c.database.try_exists()? {
        Some(baseline::load(ctx)?)
    } else {
        None
    };
    let old = if let Some(base) = &base {
        let expected = base
            .entries
            .get(Path::new("index.html"))
            .context("Index absent from baseline")?;
        let bytes = secure::read_small(&index, ctx.c.max_index_bytes)?;
        verify_old(ctx, &bytes, &expected.sha256)?;
        let m = secure::absolute(&index, libc::O_PATH)?.metadata()?;
        ensure!(
            (m.mode() & 0o7777, m.uid(), m.gid()) == (expected.mode, expected.uid, expected.gid),
            "Index metadata changed"
        );
        if bytes == html.as_bytes() {
            return Ok(());
        }
        Some((bytes, m))
    } else {
        ensure!(
            fs::symlink_metadata(&index)
                .err()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "Without a baseline, page can only create a missing index.html"
        );
        None
    };
    let mut tmp = tempfile::Builder::new()
        .prefix("candidate-")
        .tempfile_in(&ctx.c.staging_dir)?;
    tmp.write_all(html.as_bytes())?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    if let Some(base) = &base {
        let expected = base
            .entries
            .get(Path::new("index.html"))
            .context("No index baseline")?;
        let m = tmp.as_file().metadata()?;
        ensure!(
            (m.uid(), m.gid()) == (expected.uid, expected.gid),
            "Run publisher as the original publishing user/group (no silent chown)"
        );
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(expected.mode))?;
    }
    tmp.as_file().sync_all()?;
    let new_meta = tmp.as_file().metadata()?;
    let (file, path) = tmp.keep().map_err(|e| e.error)?;
    drop(file); // Close WRITE descriptor before rename; no ambiguous close-write event on index.
    let name = path.file_name().context("No temporary filename")?;
    let flag = if old.is_some() {
        libc::RENAME_EXCHANGE
    } else {
        libc::RENAME_NOREPLACE
    };
    if let Err(e) = secure::rename(
        &stage,
        Path::new(name),
        &root,
        Path::new("index.html"),
        flag,
    ) {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    stage.sync_all()?;
    root.sync_all()?;
    // RENAME_EXCHANGE preserves the ACTUAL displaced inode, closing the silent-evidence-loss race.
    // Never delete or roll back unverified evidence. A crash leaves candidate-* for investigation.
    if let (Some((old, old_meta)), Some(base)) = (&old, &base) {
        let displaced = secure::read_small(&path, ctx.c.max_index_bytes)
            .with_context(|| format!("Displaced object preserved at {path:?}"))?;
        let displaced_meta = secure::absolute(&path, libc::O_PATH)?.metadata()?;
        ensure!(
            &displaced == old
                && secure::identity(old_meta, &displaced_meta)
                && (old_meta.mode(), old_meta.uid(), old_meta.gid())
                    == (
                        displaced_meta.mode(),
                        displaced_meta.uid(),
                        displaced_meta.gid()
                    ),
            "Index changed during publication; actual displaced object preserved at {path:?}"
        );
        verify_old(
            ctx,
            &displaced,
            &base.entries[Path::new("index.html")].sha256,
        )?;
    }
    let current = secure::read_small(&index, ctx.c.max_index_bytes)?;
    let meta = secure::absolute(&index, libc::O_PATH)?.metadata()?;
    ensure!(
        current == html.as_bytes() && secure::identity(&new_meta, &meta),
        "Index changed immediately after publication; no automatic rollback"
    );
    let receipt = Receipt {
        at: Utc::now().timestamp(),
        stage: path.clone(),
        inode: meta.ino(),
        dev: meta.dev(),
        sha256: secure::hash(html.as_bytes()),
    };
    secure::atomic_write(
        &ctx.c.state_dir.join("publication.json"),
        &serde_json::to_vec(&receipt)?,
        0o600,
    )?;
    // Preserve verified previous versions; keep the last seven. Unverified candidates are never pruned.
    if old.is_some() {
        let archive = ctx.c.staging_dir.join(format!(
            "verified-{}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id()
        ));
        fs::rename(&path, archive)?;
        let mut saved = fs::read_dir(&ctx.c.staging_dir)?
            .filter_map(|x| x.ok())
            .map(|x| x.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("verified-"))
            })
            .collect::<Vec<_>>();
        saved.sort();
        let excess = saved.len().saturating_sub(7);
        for p in saved.into_iter().take(excess) {
            fs::remove_file(p)?;
        }
        stage.sync_all()?;
    }
    Ok(())
}
/// Only paired kernel rename cookies involving the PRIVATE staging pathname are authorized.
/// A valid hash alone never authorizes arbitrary data/metadata/rename events on index.html.
pub fn authorized_trackers(ctx: &ContextData, events: &[Event]) -> BTreeSet<usize> {
    let checked = (|| -> Result<BTreeSet<usize>> {
        let receipt: Receipt = serde_json::from_slice(&secure::read_small(
            &ctx.c.state_dir.join("publication.json"),
            65536,
        )?)?;
        ensure!(
            Utc::now().timestamp().abs_diff(receipt.at) <= 120,
            "Expired receipt"
        );
        ensure!(
            receipt.stage.parent() == Some(ctx.c.staging_dir.as_path()),
            "Bad staging receipt"
        );
        let index = ctx.c.root.join("index.html");
        let bytes = secure::read_small(&index, ctx.c.max_index_bytes)?;
        let m = secure::absolute(&index, libc::O_PATH)?.metadata()?;
        ensure!(
            (m.dev(), m.ino()) == (receipt.dev, receipt.inode)
                && secure::hash(&bytes) == receipt.sha256,
            "Receipt mismatch"
        );
        let mut trackers = BTreeSet::new();
        for e in events {
            if e.kind == EventKind::Modify(ModifyKind::Name(RenameMode::Both)) && e.paths.len() == 2
            {
                let a = &e.paths[0];
                let b = &e.paths[1];
                if (a == &receipt.stage && b == &index) || (a == &index && b == &receipt.stage) {
                    if let Some(t) = e.tracker() {
                        trackers.insert(t);
                    }
                }
            }
        }
        Ok(trackers)
    })();
    checked.unwrap_or_default() // Uncertain publication is NOT silently authorized.
}
