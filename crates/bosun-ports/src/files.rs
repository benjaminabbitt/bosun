//! The file port (§4.5, §4.6): every read and write of one owner's files —
//! the manifest and the path that keys its lock, the user config, the lock,
//! the user-scope machine state, and the directories they need — goes
//! through a [`FileAccess`], so who does the I/O is chosen once. A run
//! does its own files itself; an elevated run does the files of the user who
//! elevated it as that user, so the kernel allows exactly what the user could
//! do and a symlink they planted cannot steer a write as root. The adapters
//! live in bosun-io (this process) and bosun-host (as another user).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// How a lock file is opened for an advisory lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockOpen {
    /// For a shared lock: read-only, created (without its directory) only
    /// when missing, so a reader needs no write access to a lock someone else
    /// made.
    Shared,
    /// For an exclusive lock: read-write, made with its directory.
    Exclusive,
}

/// Reads and writes one owner's files. Errors keep their [`io::ErrorKind`]
/// (`NotFound`, `PermissionDenied`), which callers map to their own.
pub trait FileAccess {
    /// Whether `path` exists (following symlinks).
    fn exists(&self, path: &Path) -> io::Result<bool>;

    /// Whether `path` is a directory (following symlinks); `false` when
    /// nothing is there.
    fn is_dir(&self, path: &Path) -> io::Result<bool>;

    /// The absolute path `path` names with every symlink resolved, as its
    /// owner reaches it.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// The whole of the file at `path`.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Replace `path` with `bytes` atomically, making its directory: a temp
    /// file beside it, synced, renamed over it, readable by all (mode 0644).
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Write `bytes` over the file at `path` in place, as `std::fs::write`
    /// does: through a symlink there, keeping the file's mode and owner, and
    /// making it (mode 0666 less the umask) when it is missing. For a file a
    /// person keeps, such as a manifest, which may be a link to where they
    /// keep it.
    fn write_in_place(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;

    /// Open the lock file at `path` for an advisory lock (see [`LockOpen`]).
    fn open_lock(&self, path: &Path, how: LockOpen) -> io::Result<File>;
}
