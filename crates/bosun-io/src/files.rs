//! [`FileAccess`] as this process (§4.5, §4.6): the files of the account bosun
//! runs as, and the system state, read and written with plain std I/O.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use bosun_ports::{FileAccess, LockOpen};

/// This process's own files.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectFiles;

impl FileAccess for DirectFiles {
    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = parent(path);
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(bytes)?;
        readable(tmp.as_file())?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    fn open_lock(&self, path: &Path, how: LockOpen) -> io::Result<File> {
        match how {
            LockOpen::Shared => match File::open(path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => File::create(path),
                other => other,
            },
            LockOpen::Exclusive => {
                fs::create_dir_all(parent(path))?;
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
            }
        }
    }
}

/// The directory holding `path`.
fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// A temp file is its creator's alone; the file it becomes is readable by
/// all, as a plain write leaves it.
#[cfg(unix)]
fn readable(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn readable(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_file_whole_making_its_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("lock.json");
        DirectFiles.write(&path, b"{}").unwrap();
        assert_eq!(DirectFiles.read(&path).unwrap(), b"{}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644);
        }
    }

    #[test]
    fn replaces_a_file_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock.json");
        fs::write(&path, "old").unwrap();
        DirectFiles.write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_directory_of_a_bare_file_name_is_the_working_one() {
        assert_eq!(parent(Path::new("lock.json")), Path::new("."));
        assert_eq!(parent(Path::new("d/lock.json")), Path::new("d"));
    }

    #[test]
    fn exists_and_read_say_what_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        assert!(!DirectFiles.exists(&path).unwrap());
        let err = DirectFiles.read(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        fs::write(&path, "x").unwrap();
        assert!(DirectFiles.exists(&path).unwrap());
    }

    #[test]
    fn an_exclusive_lock_file_is_made_with_its_directory_and_kept_as_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("state.lock");
        DirectFiles.open_lock(&path, LockOpen::Exclusive).unwrap();
        assert!(path.is_file());
        fs::write(&path, "held").unwrap();
        let file = DirectFiles.open_lock(&path, LockOpen::Exclusive).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "held");
        file.lock().unwrap();
    }

    #[test]
    fn a_shared_lock_file_opens_read_only_and_is_made_only_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        DirectFiles.open_lock(&path, LockOpen::Shared).unwrap();
        assert!(path.is_file());
        let file = DirectFiles.open_lock(&path, LockOpen::Shared).unwrap();
        file.lock_shared().unwrap();
        let missing_dir = tmp.path().join("no").join("state.lock");
        assert!(DirectFiles
            .open_lock(&missing_dir, LockOpen::Shared)
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_lock_file_needs_no_write_access() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        DirectFiles.open_lock(&path, LockOpen::Shared).unwrap();
    }

    /// Only a missing lock file is made: one this user may write but not
    /// read is refused, not replaced.
    #[cfg(unix)]
    #[test]
    fn a_shared_lock_file_this_user_may_not_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        if File::open(&path).is_ok() {
            eprintln!("skipped: modes do not bind this user (root)");
            return;
        }
        let err = DirectFiles.open_lock(&path, LockOpen::Shared).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
