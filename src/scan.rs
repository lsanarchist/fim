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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}
impl Timestamp {
    fn from_system_time(time: SystemTime) -> Option<Self> {
        let (seconds, nanoseconds) = match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => (
                i64::try_from(duration.as_secs()).ok()?,
                duration.subsec_nanos(),
            ),
            Err(error) => {
                let duration = error.duration();
                let seconds = i64::try_from(duration.as_secs()).ok()?.checked_neg()?;
                if duration.subsec_nanos() == 0 {
                    (seconds, 0)
                } else {
                    (
                        seconds.checked_sub(1)?,
                        1_000_000_000 - duration.subsec_nanos(),
                    )
                }
            }
        };
        Some(Self {
            seconds,
            nanoseconds,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub kind: String,
    pub sha256: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    // Ordinary regular files carry raw metadata. The narrowly normalized daily
    // index and directories omit it: replacing index.html legitimately changes
    // its size/times and its parent directory's modification time.
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    pub created: Option<Timestamp>,
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
fn fingerprint(m: &Metadata, digest: String, path: &Path) -> Fingerprint {
    let track_raw_metadata = m.is_file() && path != Path::new("index.html");
    Fingerprint {
        kind: if m.is_dir() { "dir" } else { "file" }.into(),
        sha256: digest,
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        size: track_raw_metadata.then_some(m.len()),
        modified: track_raw_metadata.then_some(Timestamp {
            seconds: m.mtime(),
            nanoseconds: m.mtime_nsec() as u32,
        }),
        // Metadata::created reports filesystem birth time when available.
        // ctime is an inode-change timestamp and is never used as a substitute.
        created: track_raw_metadata
            .then(|| m.created().ok().and_then(Timestamp::from_system_time))
            .flatten(),
    }
}
struct Inspected {
    record: Fingerprint,
    warning: Option<Issue>,
    metadata: Metadata,
}
/// Exposed for deterministic tests of file-level failure isolation.
pub fn inspect(
    ctx: &ContextData,
    root: &File,
    path: &Path,
    deadline: Instant,
) -> Result<(Fingerprint, Option<Issue>)> {
    let inspected = inspect_entry(ctx, root, path, deadline)?;
    Ok((inspected.record, inspected.warning))
}
fn inspect_entry(
    ctx: &ContextData,
    root: &File,
    path: &Path,
    deadline: Instant,
) -> Result<Inspected> {
    let mut file = secure::beneath(root, path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() || before.is_dir(),
        "Unsupported object (symlink, FIFO, device, socket)"
    );
    if before.is_dir() {
        return Ok(Inspected {
            record: fingerprint(&before, String::new(), path),
            warning: None,
            metadata: before,
        });
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
    ensure!(
        secure::stable(&before, &after),
        "File changed/replaced during read; result is unverified"
    );
    // Revalidate all relevant metadata at the pathname, including a same-inode
    // write after the last fstat, rather than only comparing device/inode.
    secure::verify_beneath(root, path, &after)?;
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
    Ok(Inspected {
        record: fingerprint(&before, digest, path),
        warning: error,
        metadata: before,
    })
}
fn enumerate(
    root: &File,
    path: &Path,
    expected: &Metadata,
    stack: &mut Vec<PathBuf>,
    count: usize,
    max_entries: usize,
    deadline: Instant,
) -> Result<()> {
    let dir = secure::beneath(root, path, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let id = dir.metadata()?;
    ensure!(
        secure::stable(expected, &id),
        "Directory changed before enumeration"
    );
    // /proc/self/fd refers only to the descriptor opened above.
    for item in fs::read_dir(secure::proc_path(&dir))? {
        ensure!(
            Instant::now() < deadline,
            "Directory scan time budget exceeded"
        );
        let item = item?;
        ensure!(
            stack.len() + count < max_entries,
            "Directory entry budget exceeded"
        );
        stack.push(path.join(item.file_name()));
    }
    ensure!(
        secure::stable(&id, &dir.metadata()?),
        "Directory changed while enumerating"
    );
    secure::verify_beneath(root, path, &id)?;
    Ok(())
}

fn verify_directories(
    root: &File,
    directories: &BTreeMap<PathBuf, Metadata>,
    result: &mut Scan,
    deadline: Instant,
) {
    // A child may be added after its parent was enumerated while other files
    // are being hashed. Revalidate membership metadata before claiming coverage.
    for (path, expected) in directories {
        if Instant::now() >= deadline {
            result.error(
                Path::new(""),
                "SCAN_LIMIT",
                "Directory revalidation budget exceeded; incomplete coverage".into(),
            );
            break;
        }
        if !result.blocked(path) {
            if let Err(error) = secure::verify_beneath(root, path, expected) {
                result.error(path, "DIRECTORY_CHANGED", format!("{error:#}"));
            }
        }
    }
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
    let mut directories = BTreeMap::new();
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
        let Inspected {
            record,
            warning,
            metadata,
        } = match inspect_entry(ctx, &root, &path, deadline) {
            Err(e) => {
                result.error(&path, "UNREADABLE", format!("{e:#}"));
                continue;
            }
            Ok(inspected) => inspected,
        };
        if let Some(issue) = warning {
            add(&mut result.errors, issue);
        }
        let dir = record.kind == "dir";
        result.entries.insert(path.clone(), record);
        if !dir {
            continue;
        }
        let read_dir = enumerate(
            &root,
            &path,
            &metadata,
            &mut stack,
            count,
            ctx.c.max_entries,
            deadline,
        );
        if let Err(e) = read_dir {
            result.error(&path, "DIRECTORY_UNREADABLE", format!("{e:#}"));
        } else {
            directories.insert(path, metadata);
        }
    }
    verify_directories(&root, &directories, &mut result, deadline);
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
        if scan.blocked(path) {
            continue;
        }
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
                if (
                    old.mode,
                    old.uid,
                    old.gid,
                    old.size,
                    &old.modified,
                    &old.created,
                ) != (
                    new.mode,
                    new.uid,
                    new.gid,
                    new.size,
                    &new.modified,
                    &new.created,
                ) {
                    add(
                        &mut issues,
                        Issue::new(
                            "METADATA_CHANGED",
                            Some(path),
                            format!(
                                "mode {:o}->{:o}, uid {}->{}, gid {}->{}, size {:?}->{:?}, modified {:?}->{:?}, created {:?}->{:?}",
                                old.mode, new.mode, old.uid, new.uid, old.gid, new.gid,
                                old.size, new.size, old.modified, new.modified, old.created, new.created
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_replacement_between_inspection_and_enumeration_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("child")).unwrap();
        let root = secure::directory(temp.path()).unwrap();
        let observed = secure::beneath(&root, Path::new("child"), libc::O_PATH)
            .unwrap()
            .metadata()
            .unwrap();
        fs::rename(temp.path().join("child"), temp.path().join("old-child")).unwrap();
        fs::create_dir(temp.path().join("child")).unwrap();
        assert!(enumerate(
            &root,
            Path::new("child"),
            &observed,
            &mut Vec::new(),
            1,
            10,
            Instant::now() + Duration::from_secs(5),
        )
        .is_err());
    }

    #[test]
    fn late_directory_membership_change_marks_coverage_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = secure::directory(temp.path()).unwrap();
        let observed = root.metadata().unwrap();
        let mut stack = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        enumerate(&root, Path::new(""), &observed, &mut stack, 1, 10, deadline).unwrap();
        assert!(stack.is_empty());
        // This is the real boundary after enumeration while other entries are
        // being hashed: the new file was never added to the traversal stack.
        fs::write(temp.path().join("late.php"), "late addition").unwrap();
        let mut directories = BTreeMap::new();
        directories.insert(PathBuf::new(), observed);
        let mut scan = Scan::default();
        verify_directories(&root, &directories, &mut scan, deadline);
        assert!(scan.blocked(Path::new("late.php")));
        assert!(scan
            .errors
            .values()
            .any(|issue| issue.kind == "DIRECTORY_CHANGED"));
    }

    #[test]
    fn timestamp_conversion_preserves_pre_epoch_fraction() {
        assert_eq!(
            Timestamp::from_system_time(UNIX_EPOCH - Duration::from_nanos(1)),
            Some(Timestamp {
                seconds: -1,
                nanoseconds: 999_999_999,
            })
        );
        assert_eq!(
            Timestamp::from_system_time(UNIX_EPOCH - Duration::from_secs(2)),
            Some(Timestamp {
                seconds: -2,
                nanoseconds: 0,
            })
        );
    }
}
