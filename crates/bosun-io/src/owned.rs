//! Writing a file that stays its user's when an elevated run writes it into
//! their tree (§4.5): the directories it makes and the file itself are given
//! to that user, so nothing there is left root's.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// The user and group a written file is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    /// The user id.
    pub uid: u32,
    /// The group id.
    pub gid: u32,
}

/// Write `bytes` to `path` atomically: into a temp file beside it, renamed
/// over it, so a reader sees the old file or the new one, never part of one.
/// Makes the directory. With an `owner`, each directory it makes, and the
/// file before it takes its name, are given to them.
pub fn write_owned(path: &Path, bytes: &[u8], owner: Option<Owner>) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    make_dirs(dir, owner)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    readable(tmp.as_file())?;
    if let Some(owner) = owner {
        give_file(tmp.as_file(), owner)?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Make `dir` and its missing parents, giving each one made to `owner`.
fn make_dirs(dir: &Path, owner: Option<Owner>) -> io::Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
    fs::create_dir_all(dir)?;
    if let Some(owner) = owner {
        for made in missing {
            give_path(made, owner)?;
        }
    }
    Ok(())
}

/// A temp file is its creator's alone; the file it becomes is readable by
/// all, as a plain write leaves it.
#[cfg(unix)]
fn readable(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn readable(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ids(owner: Owner) -> (rustix::fs::Uid, rustix::fs::Gid) {
    (
        rustix::fs::Uid::from_raw(owner.uid),
        rustix::fs::Gid::from_raw(owner.gid),
    )
}

#[cfg(unix)]
fn give_file(file: &fs::File, owner: Owner) -> io::Result<()> {
    let (uid, gid) = ids(owner);
    Ok(rustix::fs::fchown(file, Some(uid), Some(gid))?)
}

/// Give `path` itself, not what it may link to, to `owner`.
#[cfg(unix)]
fn give_path(path: &Path, owner: Owner) -> io::Result<()> {
    let (uid, gid) = ids(owner);
    Ok(rustix::fs::chownat(
        rustix::fs::CWD,
        path,
        Some(uid),
        Some(gid),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

#[cfg(not(unix))]
fn give_file(_file: &fs::File, _owner: Owner) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn give_path(_path: &Path, _owner: Owner) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Command;

    fn id(flag: &str) -> Vec<u32> {
        let out = Command::new("id").arg(flag).output().expect("run id");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(|n| n.parse().expect("a numeric id"))
            .collect()
    }

    /// This user, and a group of theirs other than the one their files get,
    /// which they may give a file to without being root.
    fn another_group() -> Option<Owner> {
        let uid = id("-u")[0];
        let gid = id("-g")[0];
        let other = id("-G").into_iter().find(|g| *g != gid)?;
        Some(Owner { uid, gid: other })
    }

    #[test]
    fn writes_the_file_whole_making_its_directories_readable_by_all() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("lock.json");
        write_owned(&path, b"{}", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn replaces_a_file_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lock.json");
        fs::write(&path, "old").unwrap();
        write_owned(&path, b"new", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn an_owner_gets_the_file_and_each_directory_made_but_not_one_there_before() {
        let Some(owner) = another_group() else {
            eprintln!("skipped: this user has one group, so no file can be given away");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let before = fs::metadata(tmp.path()).unwrap().gid();
        let path = tmp.path().join("a").join("b").join("lock.json");
        write_owned(&path, b"{}", Some(owner)).unwrap();
        let gid = |p: &Path| fs::metadata(p).unwrap().gid();
        assert_eq!(gid(&path), owner.gid);
        assert_eq!(gid(&tmp.path().join("a")), owner.gid);
        assert_eq!(gid(&tmp.path().join("a").join("b")), owner.gid);
        assert_eq!(gid(tmp.path()), before);
        assert_eq!(fs::metadata(&path).unwrap().uid(), owner.uid);
    }
}
