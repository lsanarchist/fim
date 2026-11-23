//! Walks the website directory and fingerprints every entry.
use crate::Issue;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    io::{ErrorKind, Read},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

/// index.html is read into memory (its daily block must be checked), so it has a limit.
/// Every other file is hashed as a stream and may have any size.
pub const INDEX_LIMIT: u64 = 1024 * 1024;

/// One row of the baseline. Directories have `sha256 = "directory"`, size 0 and mtime 0
/// (their mtime changes whenever index.html is replaced). index.html also has size 0 and
/// mtime 0, because its size and time change every day; its hash covers the normalized page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

pub type IndexFilter<'a> = dyn Fn(&[u8], i64) -> Result<Vec<u8>> + 'a;

/// Relative path ("." for the site root) -> fingerprint.
pub type Snapshot = BTreeMap<String, Entry>;

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Hashes a file in small blocks, so even a huge file needs almost no memory.
fn hash_file(path: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

fn read_index(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(INDEX_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > INDEX_LIMIT {
        return Err(io::Error::other("index.html is larger than 1 MiB"));
    }
    Ok(bytes)
}

/// Returns the fingerprints plus problems (unreadable files, bad daily block).
/// `index` turns the raw bytes (and mtime) of the root index.html into the bytes to hash.
pub fn scan(root: &Path, index: &IndexFilter) -> (Snapshot, Vec<Issue>) {
    let mut files = Snapshot::new();
    let mut issues = Vec::new();
    walk(root, root, index, &mut files, &mut issues);
    (files, issues)
}

fn walk(
    root: &Path,
    dir: &Path,
    index: &IndexFilter,
    files: &mut Snapshot,
    issues: &mut Vec<Issue>,
) {
    let rel = |p: &Path| match p.strip_prefix(root) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    };
    let meta_entry = |m: &fs::Metadata| Entry {
        sha256: String::new(),
        size: m.len(),
        mtime: m.mtime(),
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
    };
    match fs::symlink_metadata(dir) {
        Ok(m) => {
            let entry = Entry {
                sha256: "directory".into(),
                size: 0,
                mtime: 0,
                ..meta_entry(&m)
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
        let hashed = if meta.is_symlink() {
            // A symlink is fingerprinted by its target, never followed.
            fs::read_link(&path).map(|t| sha256(t.as_os_str().as_bytes()))
        } else if !meta.is_file() {
            issues.push(Issue::new(
                "UNSUPPORTED",
                &name,
                "not a regular file".into(),
            ));
            continue;
        } else if name == "index.html" {
            match read_index(&path).map(|bytes| index(&bytes, meta.mtime())) {
                Ok(Ok(normalized)) => Ok(sha256(&normalized)),
                Ok(Err(e)) => {
                    issues.push(Issue::new("DAILY_BLOCK", &name, format!("{e:#}")));
                    continue;
                }
                Err(e) => Err(e),
            }
        } else {
            hash_file(&path)
        };
        let sha256 = match hashed {
            Ok(h) => h,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                issues.push(Issue::new("UNREADABLE", &name, e.to_string()));
                continue;
            }
        };
        let entry = if name == "index.html" {
            Entry {
                sha256,
                size: 0,
                mtime: 0,
                ..meta_entry(&meta)
            }
        } else {
            Entry {
                sha256,
                ..meta_entry(&meta)
            }
        };
        files.insert(name, entry);
    }
}
