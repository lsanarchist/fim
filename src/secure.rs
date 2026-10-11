//! Linux path confinement. Unsafe is restricted to syscall/FD wrappers.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsString},
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const NO_XDEV: u64 = 0x01;
const NO_SYMLINKS: u64 = 0x04;
const BENEATH: u64 = 0x08;

fn open2(fd: RawFd, path: &Path, flags: i32, resolve: u64) -> Result<File> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve,
    };
    // SAFETY: name and how are valid for the syscall; success yields an owned FD.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            fd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context(format!("openat2 {path:?}"));
    }
    Ok(unsafe { File::from_raw_fd(raw as RawFd) })
}
pub fn absolute(path: &Path, flags: i32) -> Result<File> {
    ensure!(path.is_absolute(), "Expected absolute path: {path:?}");
    open2(libc::AT_FDCWD, path, flags, NO_SYMLINKS)
}
pub fn beneath(root: &File, path: &Path, flags: i32) -> Result<File> {
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "Unsafe relative path: {path:?}"
    );
    let p = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    open2(root.as_raw_fd(), p, flags, BENEATH | NO_SYMLINKS | NO_XDEV)
}
pub fn directory(path: &Path) -> Result<File> {
    absolute(path, libc::O_RDONLY | libc::O_DIRECTORY)
}
pub fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn path_key(path: &Path) -> String {
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn key_path(key: &str) -> Result<PathBuf> {
    ensure!(key.len() % 2 == 0 && key.is_ascii(), "Invalid path hex");
    let bytes = (0..key.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let path = PathBuf::from(OsString::from_vec(bytes));
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "Invalid baseline path"
    );
    Ok(path)
}
pub fn identity(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
pub fn stable(a: &Metadata, b: &Metadata) -> bool {
    identity(a, b)
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && (a.mtime(), a.mtime_nsec(), a.ctime(), a.ctime_nsec())
            == (b.mtime(), b.mtime_nsec(), b.ctime(), b.ctime_nsec())
}
pub fn read_small(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = absolute(path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.nlink() == 1,
        "Not a single-link regular file: {path:?}"
    );
    ensure!(before.len() <= limit, "File too large: {path:?}");
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit && stable(&before, &file.metadata()?),
        "File changed while reading: {path:?}"
    );
    let again = absolute(path, libc::O_PATH)?.metadata()?;
    ensure!(
        identity(&before, &again),
        "Path replaced while reading: {path:?}"
    );
    Ok(bytes)
}
/// Strict mode trusts root-owned files and root-owned, non-writable ancestors.
/// A sticky root-owned /tmp is allowed only as an ancestor, never as a trust file.
pub fn trusted(path: &Path, demo: bool) -> Result<()> {
    if demo {
        return Ok(());
    }
    for (i, p) in path.ancestors().enumerate() {
        let m = absolute(p, libc::O_PATH)?.metadata()?;
        ensure!(m.uid() == 0, "Trusted path must be root-owned: {p:?}");
        let safe_sticky = i > 0 && m.is_dir() && m.mode() & libc::S_ISVTX != 0;
        ensure!(
            m.mode() & 0o022 == 0 || safe_sticky,
            "Untrusted writable path: {p:?}"
        );
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    let m = directory(path)?.metadata()?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        (m.uid() == uid || uid == 0) && m.mode() & 0o077 == 0,
        "Runtime directory must be owned by this user and mode 0700: {path:?}"
    );
    Ok(())
}
pub fn runtime_file(path: &Path) -> Result<File> {
    private_dir(path.parent().context("No parent")?)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "Unsafe runtime file: {path:?}"
    );
    Ok(file)
}
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    let f = runtime_file(path)?;
    // SAFETY: valid owned descriptor; nonblocking lock prevents duplicate processes.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(f))
}
pub fn lock(path: &Path) -> Result<File> {
    try_lock(path)?.with_context(|| format!("Another process holds {path:?}"))
}
pub fn rename(from_dir: &File, from: &Path, to_dir: &File, to: &Path, flags: u32) -> Result<()> {
    ensure!(
        from.components().count() == 1
            && to.components().count() == 1
            && from
                .components()
                .chain(to.components())
                .all(|c| matches!(c, Component::Normal(_))),
        "rename expects basenames"
    );
    let a = CString::new(from.as_os_str().as_bytes())?;
    let b = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: valid directory FDs and NUL-terminated basenames.
    if unsafe {
        libc::renameat2(
            from_dir.as_raw_fd(),
            a.as_ptr(),
            to_dir.as_raw_fd(),
            b.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("renameat2 (same filesystem required)");
    }
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("No parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    directory(parent)?.sync_all()?;
    Ok(())
}
