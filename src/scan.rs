//! Walks the website directory and fingerprints every entry.
use crate::Issue;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

/// One row of the baseline. Directories have `sha256 = "directory"`, size 0 and mtime 0
/// (their mtime changes whenever index.html is replaced). index.html also has size 0 and
/// mtime 0, because its size and time change every day; its hash covers the normalized page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}

/// Relative path ("." for the site root) -> fingerprint.
pub type Snapshot = BTreeMap<String, Entry>;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Returns the fingerprints plus problems (unreadable files, bad daily block).
/// `index` turns the raw bytes of the root index.html into the bytes to hash.
pub fn scan(root: &Path, index: &dyn Fn(&[u8]) -> Result<Vec<u8>>) -> (Snapshot, Vec<Issue>) {
    let mut files = Snapshot::new();
    let mut issues = Vec::new();
    walk(root, root, index, &mut files, &mut issues);
    (files, issues)
}

fn walk(
    root: &Path,
    dir: &Path,
    index: &dyn Fn(&[u8]) -> Result<Vec<u8>>,
    files: &mut Snapshot,
    issues: &mut Vec<Issue>,
) {
    let rel = |p: &Path| match p.strip_prefix(root) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    };
    match fs::symlink_metadata(dir) {
        Ok(m) => {
            let entry = Entry {
                sha256: "directory".into(),
                size: 0,
                mtime: 0,
                mode: m.mode() & 0o7777,
            };
            files.insert(rel(dir), entry);
        }
        Err(e) => return issues.push(Issue::new("UNREADABLE", &rel(dir), e.to_string())),
    }
    let list = match fs::read_dir(dir) {
        Ok(list) => list,
        Err(e) => return issues.push(Issue::new("UNREADABLE", &rel(dir), e.to_string())),
    };
    for item in list {
        let path = match item {
            Ok(item) => item.path(),
            Err(e) => {
                issues.push(Issue::new("UNREADABLE", &rel(dir), e.to_string()));
                continue;
            }
        };
        let name = rel(&path);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue, // deleted meanwhile
            Err(e) => {
                issues.push(Issue::new("UNREADABLE", &name, e.to_string()));
                continue;
            }
        };
        if meta.is_dir() {
            walk(root, &path, index, files, issues);
            continue;
        }
        // A symlink is fingerprinted by its target, never followed.
        let bytes = if meta.is_symlink() {
            fs::read_link(&path).map(|t| t.as_os_str().as_bytes().to_vec())
        } else if meta.is_file() {
            fs::read(&path)
        } else {
            issues.push(Issue::new(
                "UNSUPPORTED",
                &name,
                "not a regular file".into(),
            ));
            continue;
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                issues.push(Issue::new("UNREADABLE", &name, e.to_string()));
                continue;
            }
        };
        let mode = meta.mode() & 0o7777;
        let entry = if name == "index.html" {
            match index(&bytes) {
                Ok(normalized) => Entry {
                    sha256: sha256(&normalized),
                    size: 0,
                    mtime: 0,
                    mode,
                },
                Err(e) => {
                    issues.push(Issue::new("DAILY_BLOCK", &name, format!("{e:#}")));
                    continue;
                }
            }
        } else {
            Entry {
                sha256: sha256(&bytes),
                size: meta.len(),
                mtime: meta.mtime(),
                mode,
            }
        };
        files.insert(name, entry);
    }
}
