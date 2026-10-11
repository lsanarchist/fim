use crate::{add, config::ContextData, daily, secure, Issue, Issues};
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub kind: String,
    pub sha256: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}
pub type Entries = BTreeMap<PathBuf, Fingerprint>;
#[derive(Default)]
pub struct Scan {
    pub entries: Entries,
    pub errors: Issues,
    // Prefixes whose contents are unknown. They must NOT be reported as deleted/recovered.
    pub unknown: BTreeSet<PathBuf>,
}
impl Scan {
    pub fn blocked(&self, path: &Path) -> bool {
        path.ancestors().any(|p| self.unknown.contains(p))
    }
    fn error(&mut self, path: &Path, kind: &str, msg: String) {
        self.unknown.insert(path.into());
        add(&mut self.errors, Issue::new(kind, Some(path), msg));
    }
}
fn fingerprint(m: &Metadata, digest: String) -> Fingerprint {
    Fingerprint {
        kind: if m.is_dir() { "dir" } else { "file" }.into(),
        sha256: digest,
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
    }
}
/// Exposed for deterministic tests of file-level failure isolation.
pub fn inspect(
    ctx: &ContextData,
    root: &File,
    path: &Path,
    deadline: Instant,
) -> Result<(Fingerprint, Option<Issue>)> {
    let mut file = secure::beneath(root, path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() || before.is_dir(),
        "Unsupported object (symlink, FIFO, device, socket)"
    );
    if before.is_dir() {
        return Ok((fingerprint(&before, String::new()), None));
    }
    ensure!(before.nlink() == 1, "Hard-linked files are not supported");
    let is_index = path == Path::new("index.html");
    let limit = if is_index {
        ctx.c.max_index_bytes
    } else {
        ctx.c.max_file_bytes
    };
    ensure!(before.len() <= limit, "File exceeds configured size limit");
    // A large file gets at most five seconds of userspace hashing budget, not the whole tree budget.
    let deadline = deadline.min(Instant::now() + Duration::from_secs(5));
    let begin = Utc::now().with_timezone(&ctx.tz);
    let mut hasher = Sha256::new();
    let mut html = Vec::new();
    let mut buf = [0u8; 65536];
    let mut total = 0u64;
    loop {
        ensure!(Instant::now() < deadline, "Read/scan time budget exceeded");
        let count = file.read(&mut buf)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= limit, "File grew beyond configured limit");
        hasher.update(&buf[..count]);
        if is_index {
            html.extend_from_slice(&buf[..count]);
        }
    }
    let end = Utc::now().with_timezone(&ctx.tz);
    let after = file.metadata()?;
    let current = secure::beneath(root, path, libc::O_PATH)?.metadata()?;
    ensure!(
        secure::stable(&before, &after) && secure::identity(&before, &current),
        "File changed/replaced during read; result is unverified"
    );
    let raw = format!("{:x}", hasher.finalize());
    let (digest, error) = if is_index {
        match std::str::from_utf8(&html)
            .context("index.html is not UTF-8")
            .and_then(|s| daily::normalize(s, &ctx.names, begin, end, ctx.c.midnight_grace_seconds))
        {
            Ok(normalized) => (secure::hash(normalized.as_bytes()), None),
            Err(e) => (
                format!("invalid:{raw}"),
                Some(Issue::new(
                    "DAILY_INVALID",
                    Some(path),
                    format!("{e:#}; raw_sha256={raw}"),
                )),
            ),
        }
    } else {
        (raw, None)
    };
    Ok((fingerprint(&before, digest), error))
}
pub fn run(ctx: &ContextData) -> Scan {
    let mut result = Scan::default();
    let root = match secure::directory(&ctx.c.root) {
        Ok(root) => root,
        Err(e) => {
            result.error(Path::new(""), "ROOT_UNREADABLE", format!("{e:#}"));
            return result;
        }
    };
    let root_id = match root.metadata() {
        Ok(m) => m,
        Err(e) => {
            result.error(Path::new(""), "ROOT_UNREADABLE", e.to_string());
            return result;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(ctx.c.scan_budget_seconds);
    let mut stack = vec![PathBuf::new()];
    let mut count = 0usize;
    while let Some(path) = stack.pop() {
        if count >= ctx.c.max_entries || Instant::now() >= deadline {
            result.error(
                Path::new(""),
                "SCAN_LIMIT",
                "Tree/scan budget exceeded; incomplete coverage".into(),
            );
            break;
        }
        count += 1;
        let inspected = inspect(ctx, &root, &path, deadline);
        match inspected {
            Err(e) => {
                result.error(&path, "UNREADABLE", format!("{e:#}"));
                continue;
            }
            Ok((record, warning)) => {
                if let Some(issue) = warning {
                    add(&mut result.errors, issue);
                }
                let dir = record.kind == "dir";
                result.entries.insert(path.clone(), record);
                if !dir {
                    continue;
                }
            }
        }
        let read_dir = (|| -> Result<()> {
            let dir = secure::beneath(&root, &path, libc::O_RDONLY | libc::O_DIRECTORY)?;
            let id = dir.metadata()?;
            // /proc/self/fd is used ONLY for a descriptor opened above; never an untrusted pathname.
            for item in fs::read_dir(secure::proc_path(&dir))? {
                let item = item?;
                if stack.len() + count >= ctx.c.max_entries {
                    ensure!(false, "Directory entry budget exceeded");
                }
                stack.push(path.join(item.file_name()));
            }
            let latest =
                secure::beneath(&root, &path, libc::O_RDONLY | libc::O_DIRECTORY)?.metadata()?;
            ensure!(
                secure::identity(&id, &latest),
                "Directory replaced while enumerating"
            );
            Ok(())
        })();
        if let Err(e) = read_dir {
            result.error(&path, "DIRECTORY_UNREADABLE", format!("{e:#}"));
        }
    }
    if let Err(e) = (|| -> Result<()> {
        ensure!(
            secure::identity(&root_id, &secure::directory(&ctx.c.root)?.metadata()?),
            "Web root replaced during scan"
        );
        Ok(())
    })() {
        result.error(Path::new(""), "ROOT_REPLACED", format!("{e:#}"));
    }
    result
}
pub fn compare(baseline: &Entries, scan: &Scan, previous: &Issues) -> Issues {
    let mut issues = scan.errors.clone();
    for (path, new) in &scan.entries {
        match baseline.get(path) {
            None => add(
                &mut issues,
                Issue::new("ADDED", Some(path), format!("{new:?}")),
            ),
            Some(old) => {
                if old.kind != new.kind || old.sha256 != new.sha256 {
                    add(
                        &mut issues,
                        Issue::new(
                            "CONTENT_CHANGED",
                            Some(path),
                            format!(
                                "old={} {} new={} {}",
                                old.kind, old.sha256, new.kind, new.sha256
                            ),
                        ),
                    );
                }
                if (old.mode, old.uid, old.gid) != (new.mode, new.uid, new.gid) {
                    add(
                        &mut issues,
                        Issue::new(
                            "METADATA_CHANGED",
                            Some(path),
                            format!(
                                "mode {:o}->{:o}, uid {}->{}, gid {}->{}",
                                old.mode, new.mode, old.uid, new.uid, old.gid, new.gid
                            ),
                        ),
                    );
                }
            }
        }
    }
    for path in baseline.keys() {
        if !scan.entries.contains_key(path) && !scan.blocked(path) {
            add(
                &mut issues,
                Issue::new("DELETED", Some(path), "Baseline entry missing"),
            );
        }
    }
    // Unknown is not recovered. Preserve last-known incidents until the path can be checked.
    for (key, issue) in previous {
        if let Some(hex) = &issue.path_hex {
            if secure::key_path(hex).is_ok_and(|p| scan.blocked(&p)) {
                issues.entry(key.clone()).or_insert_with(|| issue.clone());
            }
        }
    }
    issues
}
