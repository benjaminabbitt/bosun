//! [`FileAccess`] as another user (§4.5, §4.6): how an elevated run reads and
//! writes the files of the user who elevated it. Each operation runs in a
//! short-lived forked child that drops to the user's groups, group id and
//! user id (`setgroups`, `setgid`, `setuid`, all of them, for good) and then
//! does the I/O, so the kernel allows exactly what the user could do: a
//! symlink they planted resolves with their rights, never root's. The child
//! answers over a socket pair: a status, then the file's bytes for a read,
//! or an opened descriptor (`SCM_RIGHTS`): the lock file, which the parent
//! then locks and holds, or the file a path resolves to, whose canonical
//! path the parent reads from it.
//!
//! A fork, not a re-exec of bosun, so the binary need not be one the user
//! may run, and nothing of the environment or the command line reaches the
//! child; not per-thread credentials (`setfsuid`), which are Linux-only and
//! leave root's supplementary groups. The child may share a process with
//! other threads, so it only makes system calls on buffers made before the
//! fork (paths as C strings, the bytes to write, the group list), allocates
//! nothing, and leaves by `_exit`.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bosun_ports::{FileAccess, LockOpen};
use nix::unistd::{ForkResult, Gid, Uid};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

use crate::errmsg;
use crate::invoker::Invoker;
use crate::logmsg;

/// Reads and writes files as one user, from a process that may become them
/// (root, or that user already).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsUser {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

impl AsUser {
    /// As the user `uid`, with primary group `gid` and the supplementary
    /// `groups`.
    pub fn new(uid: u32, gid: u32, groups: Vec<u32>) -> AsUser {
        AsUser { uid, gid, groups }
    }

    /// As the user who elevated this run, in the groups the group database
    /// gives them (`getgrouplist`; their primary group alone where that
    /// cannot be read, a subset of their rights).
    pub fn invoker(user: &Invoker) -> AsUser {
        AsUser::new(user.uid, user.gid, groups_of(&user.name, user.gid))
    }

    /// The user id it does the I/O as.
    pub fn uid(&self) -> u32 {
        self.uid
    }
}

/// `name`'s groups by the group database, with `gid` among them.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn groups_of(name: &str, gid: u32) -> Vec<u32> {
    let listed = CString::new(name)
        .ok()
        .and_then(|n| nix::unistd::getgrouplist(&n, Gid::from_raw(gid)).ok());
    match listed {
        Some(groups) => groups.into_iter().map(Gid::as_raw).collect(),
        None => vec![gid],
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn groups_of(_name: &str, gid: u32) -> Vec<u32> {
    vec![gid]
}

/// What the child is asked to do.
enum Op<'a> {
    Exists,
    IsDir,
    Resolve,
    Read,
    Write(&'a [u8]),
    WriteInPlace(&'a [u8]),
    Lock(LockOpen),
}

impl Op<'_> {
    fn name(&self) -> &'static str {
        match self {
            Op::Exists => "exists",
            Op::IsDir => "is directory",
            Op::Resolve => "resolve",
            Op::Read => "read",
            Op::Write(_) => "write",
            Op::WriteInPlace(_) => "write in place",
            Op::Lock(LockOpen::Shared) => "open shared lock",
            Op::Lock(LockOpen::Exclusive) => "open exclusive lock",
        }
    }

    /// Whether the operation makes the file's directory.
    fn makes_dirs(&self) -> bool {
        matches!(self, Op::Write(_) | Op::Lock(LockOpen::Exclusive))
    }
}

/// Everything the child needs, made before the fork.
struct Prepared {
    path: CString,
    /// The file's directory and its ancestors, deepest first, for making
    /// the ones missing.
    dirs: Vec<CString>,
    /// The temp file a write goes to before it takes the file's name.
    tmp: CString,
    uid: Uid,
    gid: Gid,
    groups: Vec<Gid>,
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// The directory holding `path`.
fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// A temp file name beside `path` no other write uses: the process, a
/// counter, and the time.
fn temp_beside(path: &Path) -> std::path::PathBuf {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    parent(path).join(format!(".{name}.{}.{n}.{nanos}.tmp", std::process::id()))
}

impl AsUser {
    fn prepare(&self, path: &Path, op: &Op<'_>) -> io::Result<Prepared> {
        let dirs = if op.makes_dirs() {
            parent(path)
                .ancestors()
                .filter(|d| !d.as_os_str().is_empty())
                .map(c_path)
                .collect::<io::Result<_>>()?
        } else {
            Vec::new()
        };
        Ok(Prepared {
            path: c_path(path)?,
            dirs,
            tmp: c_path(&temp_beside(path))?,
            uid: Uid::from_raw(self.uid),
            gid: Gid::from_raw(self.gid),
            groups: self.groups.iter().copied().map(Gid::from_raw).collect(),
        })
    }

    /// Run `op` on `path` in a child that is this user, and take its answer.
    fn run(&self, path: &Path, op: Op<'_>) -> io::Result<Answer> {
        tracing::debug!(
            message = logmsg::AS_USER,
            uid = self.uid,
            op = op.name(),
            path = %path.display()
        );
        let prepared = self.prepare(path, &op)?;
        let (ours, theirs) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )?;
        // SAFETY: the child runs only `child`, which makes system calls on
        // buffers prepared above, allocates nothing and takes no lock, and
        // leaves by `_exit`; so a lock another thread held at the fork is
        // never waited on, and nothing of the parent's runs twice.
        match unsafe { nix::unistd::fork() }.map_err(io::Error::from)? {
            ForkResult::Child => {
                drop(ours);
                child(&prepared, &op, theirs.as_fd())
            }
            ForkResult::Parent { child } => {
                drop(theirs);
                let answer = receive(ours, &op);
                let status = reap(child)?;
                match (answer, status) {
                    (Ok(answer), 0) => Ok(answer),
                    (Err(e), _) => Err(e),
                    (Ok(_), _) => Err(io::Error::other(errmsg::AS_USER_FAILED)),
                }
            }
        }
    }
}

/// Wait for the child; its exit code, or `-1` when a signal ended it.
fn reap(child: nix::unistd::Pid) -> io::Result<i32> {
    use nix::sys::wait::{waitpid, WaitStatus};
    loop {
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => return Ok(code),
            Ok(WaitStatus::Signaled(..)) => return Ok(-1),
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// What the child answered.
enum Answer {
    Done,
    Flag(bool),
    Bytes(Vec<u8>),
    Fd(OwnedFd),
}

/// The status the child sends first: `OK`, or `FAILED` and the errno.
const OK: u8 = 0;
const FAILED: u8 = 1;
const HEADER: usize = 5;

fn receive(sock: OwnedFd, op: &Op<'_>) -> io::Result<Answer> {
    let mut header = [0u8; HEADER];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = loop {
        let mut iov = [IoSliceMut::new(&mut header)];
        match rustix::net::recvmsg(&sock, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC) {
            Ok(msg) => break msg.bytes,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    };
    let mut fd: Option<OwnedFd> = None;
    for msg in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = msg {
            for received in fds {
                fd.get_or_insert(received);
            }
        }
    }
    let mut sock = File::from(sock);
    if got == 0 {
        return Err(io::Error::other(errmsg::AS_USER_NO_ANSWER));
    }
    sock.read_exact(&mut header[got..])?;
    if header[0] != OK {
        let errno = i32::from_le_bytes([header[1], header[2], header[3], header[4]]);
        return Err(io::Error::from_raw_os_error(errno));
    }
    match op {
        Op::Write(_) | Op::WriteInPlace(_) => Ok(Answer::Done),
        Op::Exists | Op::IsDir => {
            let mut flag = [0u8; 1];
            sock.read_exact(&mut flag)?;
            Ok(Answer::Flag(flag[0] == 1))
        }
        Op::Read => {
            let mut bytes = Vec::new();
            sock.read_to_end(&mut bytes)?;
            Ok(Answer::Bytes(bytes))
        }
        Op::Resolve | Op::Lock(_) => fd
            .map(Answer::Fd)
            .ok_or_else(|| io::Error::other(errmsg::AS_USER_NO_ANSWER)),
    }
}

// --- the child: system calls only, on what `Prepared` holds ---------------

/// Become the user, do `op`, answer on `sock`, and exit.
fn child(p: &Prepared, op: &Op<'_>, sock: BorrowedFd<'_>) -> ! {
    let code = match become_user(p) {
        Ok(()) => serve(p, op, sock),
        Err(e) => answer(sock, Err(e), None),
    };
    // SAFETY: `_exit` is async-signal-safe and ends the child without running
    // the parent's exit handlers, destructors or stdio flushes.
    unsafe { nix::libc::_exit(code) }
}

/// Drop to the user's groups, group and user id. Root may set the groups;
/// a process already the user keeps its own. Checked after, so a half-made
/// change never does I/O.
fn become_user(p: &Prepared) -> Result<(), Errno> {
    let nix_errno = |e: nix::errno::Errno| Errno::from_raw_os_error(e as i32);
    if nix::unistd::geteuid().is_root() {
        set_groups(&p.groups)?;
    }
    nix::unistd::setgid(p.gid).map_err(nix_errno)?;
    nix::unistd::setuid(p.uid).map_err(nix_errno)?;
    let became = nix::unistd::getuid() == p.uid
        && nix::unistd::geteuid() == p.uid
        && nix::unistd::getgid() == p.gid
        && nix::unistd::getegid() == p.gid;
    if became {
        Ok(())
    } else {
        Err(Errno::PERM)
    }
}

#[cfg(not(target_vendor = "apple"))]
fn set_groups(groups: &[Gid]) -> Result<(), Errno> {
    nix::unistd::setgroups(groups).map_err(|e| Errno::from_raw_os_error(e as i32))
}

/// nix has no `setgroups` on Apple platforms; libc's is called directly.
#[cfg(target_vendor = "apple")]
fn set_groups(groups: &[Gid]) -> Result<(), Errno> {
    let mut raw = [0 as nix::libc::gid_t; 16];
    let n = groups.len().min(raw.len());
    for (slot, g) in raw.iter_mut().zip(groups) {
        *slot = g.as_raw();
    }
    // SAFETY: `raw` holds `n` initialised group ids and outlives the call.
    let rc = unsafe { nix::libc::setgroups(n as nix::libc::c_int, raw.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(nix::errno::Errno::last_raw()))
    }
}

/// Do `op` as the user and answer; the exit code.
fn serve(p: &Prepared, op: &Op<'_>, sock: BorrowedFd<'_>) -> i32 {
    match op {
        Op::Exists => match rustix::fs::stat(p.path.as_c_str()) {
            Ok(_) => answer_with(sock, &[1]),
            Err(Errno::NOENT) => answer_with(sock, &[0]),
            Err(e) => answer(sock, Err(e), None),
        },
        Op::IsDir => match rustix::fs::stat(p.path.as_c_str()) {
            Ok(st) => {
                let dir = rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir();
                answer_with(sock, &[u8::from(dir)])
            }
            Err(Errno::NOENT) => answer_with(sock, &[0]),
            Err(e) => answer(sock, Err(e), None),
        },
        Op::Resolve => match open(&p.path, RESOLVE) {
            Ok(file) => answer(sock, Ok(()), Some(file.as_fd())),
            Err(e) => answer(sock, Err(e), None),
        },
        Op::Read => match open(&p.path, OFlags::RDONLY) {
            Ok(file) => {
                let code = answer(sock, Ok(()), None);
                if code != 0 {
                    return code;
                }
                copy(&file, sock).map_or(1, |()| 0)
            }
            Err(e) => answer(sock, Err(e), None),
        },
        Op::Write(bytes) => answer(sock, write(p, bytes), None),
        Op::WriteInPlace(bytes) => answer(sock, write_in_place(p, bytes), None),
        Op::Lock(how) => match open_lock(p, *how) {
            Ok(file) => answer(sock, Ok(()), Some(file.as_fd())),
            Err(e) => answer(sock, Err(e), None),
        },
    }
}

fn open(path: &CString, flags: OFlags) -> Result<OwnedFd, Errno> {
    rustix::fs::open(path.as_c_str(), flags | OFlags::CLOEXEC, Mode::from(0o644))
}

/// How a path is opened to resolve it: for no access but the right to
/// reach it (`O_PATH`), so a file the user may not read still resolves.
#[cfg(any(target_os = "linux", target_os = "android"))]
const RESOLVE: OFlags = OFlags::PATH;

/// Without `O_PATH`, read-only and non-blocking, so a FIFO does not wait.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RESOLVE: OFlags = OFlags::RDONLY.union(OFlags::NONBLOCK);

/// Write `bytes` over the file at its path, as `std::fs::write` does:
/// through a symlink, keeping the file's mode, making it (0666 less the
/// umask) when missing.
fn write_in_place(p: &Prepared, bytes: &[u8]) -> Result<(), Errno> {
    let file = rustix::fs::open(
        p.path.as_c_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from(0o666),
    )?;
    write_all(file.as_fd(), bytes)
}

/// Make the missing directories among `dirs` (deepest first), outermost
/// first, as `create_dir_all` does.
fn make_dirs(dirs: &[CString]) -> Result<(), Errno> {
    let mut missing = 0;
    while let Some(dir) = dirs.get(missing) {
        match rustix::fs::stat(dir.as_c_str()) {
            Ok(_) => break,
            Err(Errno::NOENT) => missing += 1,
            Err(e) => return Err(e),
        }
    }
    for dir in dirs[..missing].iter().rev() {
        match rustix::fs::mkdir(dir.as_c_str(), Mode::from(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write the temp file whole, synced, then rename it over the file.
fn write(p: &Prepared, bytes: &[u8]) -> Result<(), Errno> {
    make_dirs(&p.dirs)?;
    let tmp = open(
        &p.tmp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
    )?;
    let written = rustix::fs::fchmod(&tmp, Mode::from(0o644))
        .and_then(|()| write_all(tmp.as_fd(), bytes))
        .and_then(|()| rustix::fs::fsync(&tmp));
    drop(tmp);
    let renamed = written.and_then(|()| rustix::fs::rename(p.tmp.as_c_str(), p.path.as_c_str()));
    if renamed.is_err() {
        let _ = rustix::fs::unlink(p.tmp.as_c_str());
    }
    renamed
}

fn open_lock(p: &Prepared, how: LockOpen) -> Result<OwnedFd, Errno> {
    match how {
        LockOpen::Shared => match open(&p.path, OFlags::RDONLY) {
            Err(Errno::NOENT) => open(&p.path, OFlags::RDWR | OFlags::CREATE),
            other => other,
        },
        LockOpen::Exclusive => {
            make_dirs(&p.dirs)?;
            open(&p.path, OFlags::RDWR | OFlags::CREATE)
        }
    }
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copy the rest of `file` to `sock` through a buffer on the stack.
fn copy(file: &OwnedFd, sock: BorrowedFd<'_>) -> Result<(), Errno> {
    let mut buf = [0u8; 8192];
    loop {
        match rustix::io::read(file, &mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => write_all(sock, buf.get(..n).unwrap_or_default())?,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Send the status, with `fd` attached when given; the exit code.
fn answer(sock: BorrowedFd<'_>, result: Result<(), Errno>, fd: Option<BorrowedFd<'_>>) -> i32 {
    let mut header = [OK, 0, 0, 0, 0];
    if let Err(e) = result {
        header[0] = FAILED;
        let raw = e.raw_os_error().to_le_bytes();
        header[1..].copy_from_slice(&raw);
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds;
    if let Some(fd) = fd {
        fds = [fd];
        control.push(SendAncillaryMessage::ScmRights(&fds));
    }
    let sent = loop {
        match rustix::net::sendmsg(
            sock,
            &[IoSlice::new(&header)],
            &mut control,
            SendFlags::empty(),
        ) {
            Err(Errno::INTR) => {}
            other => break other,
        }
    };
    let rest = match sent {
        Ok(n) => write_all(sock, header.get(n..).unwrap_or_default()),
        Err(e) => Err(e),
    };
    rest.map_or(1, |()| 0)
}

/// Answer success followed by `bytes`; the exit code.
fn answer_with(sock: BorrowedFd<'_>, bytes: &[u8]) -> i32 {
    match answer(sock, Ok(()), None) {
        0 => write_all(sock, bytes).map_or(1, |()| 0),
        code => code,
    }
}

/// The canonical path of the file `fd` was opened on, as the kernel
/// names it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn path_of(fd: &OwnedFd) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

#[cfg(target_vendor = "apple")]
fn path_of(fd: &OwnedFd) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let path = rustix::fs::getpath(fd)?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(
        path.into_bytes(),
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn path_of(_fd: &OwnedFd) -> io::Result<PathBuf> {
    Err(io::ErrorKind::Unsupported.into())
}

impl AsUser {
    fn flag(&self, path: &Path, op: Op<'_>) -> io::Result<bool> {
        match self.run(path, op)? {
            Answer::Flag(set) => Ok(set),
            _ => Err(io::Error::other(errmsg::AS_USER_NO_ANSWER)),
        }
    }

    fn fd(&self, path: &Path, op: Op<'_>) -> io::Result<OwnedFd> {
        match self.run(path, op)? {
            Answer::Fd(fd) => Ok(fd),
            _ => Err(io::Error::other(errmsg::AS_USER_NO_ANSWER)),
        }
    }
}

impl FileAccess for AsUser {
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.flag(path, Op::Exists)
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.flag(path, Op::IsDir)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path_of(&self.fd(path, Op::Resolve)?)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self.run(path, Op::Read)? {
            Answer::Bytes(bytes) => Ok(bytes),
            _ => Err(io::Error::other(errmsg::AS_USER_NO_ANSWER)),
        }
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.run(path, Op::Write(bytes)).map(|_| ())
    }

    fn write_in_place(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.run(path, Op::WriteInPlace(bytes)).map(|_| ())
    }

    fn open_lock(&self, path: &Path, how: LockOpen) -> io::Result<File> {
        self.fd(path, Op::Lock(how)).map(File::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// This process's own user, which it may always become.
    fn me() -> AsUser {
        AsUser::new(
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
            Vec::new(),
        )
    }

    fn root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    #[test]
    fn writes_a_file_whole_making_its_directories_and_reads_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("lock.json");
        me().write(&path, b"{\"x\":1}").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{\"x\":1}");
        assert_eq!(me().read(&path).unwrap(), b"{\"x\":1}");
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o644);
        assert_eq!(meta.uid(), me().uid());
        assert!(tmp.path().join("a").join("b").is_dir());
    }

    #[test]
    fn replaces_a_file_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, "old").unwrap();
        me().write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn reads_a_file_larger_than_the_socket_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big");
        let bytes: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        me().write(&path, &bytes).unwrap();
        assert_eq!(me().read(&path).unwrap(), bytes);
    }

    #[test]
    fn says_whether_a_file_exists_and_a_missing_one_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        assert!(!me().exists(&path).unwrap());
        assert_eq!(
            me().read(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::write(&path, "").unwrap();
        assert!(me().exists(&path).unwrap());
        assert!(me().exists(tmp.path()).unwrap());
    }

    #[test]
    fn an_exclusive_lock_file_is_made_with_its_directory_and_can_be_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("state.lock");
        let file = me().open_lock(&path, LockOpen::Exclusive).unwrap();
        assert!(path.is_file());
        file.lock().unwrap();
        let other = fs::File::open(&path).unwrap();
        assert!(other.try_lock_shared().is_err(), "the lock is held");
    }

    #[test]
    fn a_shared_lock_file_opens_read_only_and_is_made_only_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        me().open_lock(&path, LockOpen::Shared).unwrap();
        assert!(path.is_file());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let file = me().open_lock(&path, LockOpen::Shared).unwrap();
        file.lock_shared().unwrap();
        let no_dir = tmp.path().join("no").join("state.lock");
        let err = me().open_lock(&no_dir, LockOpen::Shared).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_write_the_user_may_not_make_fails_and_leaves_nothing() {
        if root() {
            eprintln!("skipped: root may write anywhere");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let shut = tmp.path().join("shut");
        fs::create_dir(&shut).unwrap();
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o555)).unwrap();
        let link = tmp.path().join("locks");
        std::os::unix::fs::symlink(&shut, &link).unwrap();
        for path in [
            shut.join("x.json"),
            link.join("x.json"),
            link.join("d").join("x.json"),
        ] {
            let err = me().write(&path, b"{}").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{path:?}");
            let err = me().open_lock(&path, LockOpen::Exclusive).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{path:?}");
        }
        assert_eq!(fs::read_dir(&shut).unwrap().count(), 0);
    }

    #[test]
    fn another_user_cannot_be_become_without_root_and_nothing_is_done() {
        if root() {
            eprintln!("skipped: root may become anyone");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.json");
        let other = AsUser::new(me().uid + 1, me().gid, Vec::new());
        assert_eq!(
            other.write(&path, b"{}").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!path.exists());
        let group = AsUser::new(me().uid, me().gid + 1, Vec::new());
        assert_eq!(
            group.exists(tmp.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// Root becomes `nobody`: a directory root alone may write takes nothing,
    /// one of nobody's takes a file that is theirs, and root's groups are
    /// gone. Runs as root only (the real tier's container).
    #[test]
    fn root_writes_as_the_user_with_their_rights_alone() {
        if !root() {
            eprintln!("skipped: needs root to become another user");
            return;
        }
        // Root in group 0, so a child that kept root's groups could write
        // where group 0 may.
        #[cfg(not(target_vendor = "apple"))]
        nix::unistd::setgroups(&[Gid::from_raw(0)]).unwrap();
        let nobody = AsUser::new(65534, 65534, vec![65534]);
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let theirs = tmp.path().join("theirs");
        fs::create_dir(&theirs).unwrap();
        std::os::unix::fs::chown(&theirs, Some(65534), Some(65534)).unwrap();
        let roots = tmp.path().join("roots");
        fs::create_dir(&roots).unwrap();
        fs::set_permissions(&roots, fs::Permissions::from_mode(0o775)).unwrap();
        let err = nobody.write(&roots.join("x"), b"{}").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_dir(&roots).unwrap().count(), 0);
        let file = theirs.join("d").join("x");
        nobody.write(&file, b"{}").unwrap();
        for p in [&file, &theirs.join("d")] {
            let meta = fs::metadata(p).unwrap();
            assert_eq!((meta.uid(), meta.gid()), (65534, 65534), "{p:?}");
        }
        let private = tmp.path().join("private");
        fs::write(&private, "secret").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o640)).unwrap();
        let err = nobody.read(&private).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let in_group_0 = AsUser::new(65534, 65534, vec![65534, 0]);
        assert_eq!(in_group_0.read(&private).unwrap(), b"secret");
        in_group_0.write(&roots.join("x"), b"{}").unwrap();
    }

    #[test]
    fn a_write_or_exclusive_lock_makes_every_missing_directory_and_others_none() {
        let path = Path::new("/h/u/.local/state/bosun/locks/x.json");
        let dirs = |op: Op<'_>| -> Vec<String> {
            me().prepare(path, &op)
                .unwrap()
                .dirs
                .iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect()
        };
        let want = [
            "/h/u/.local/state/bosun/locks",
            "/h/u/.local/state/bosun",
            "/h/u/.local/state",
            "/h/u/.local",
            "/h/u",
            "/h",
            "/",
        ];
        assert_eq!(dirs(Op::Write(b"")), want);
        assert_eq!(dirs(Op::Lock(LockOpen::Exclusive)), want);
        assert!(dirs(Op::Read).is_empty());
        assert!(dirs(Op::Exists).is_empty());
        assert!(dirs(Op::Lock(LockOpen::Shared)).is_empty());
        assert_eq!(dirs(Op::Write(b""))[0], "/h/u/.local/state/bosun/locks");
    }

    #[test]
    fn each_write_has_its_own_temp_file_beside_the_file() {
        let path = Path::new("/s/locks/x.json");
        let a = temp_beside(path);
        let b = temp_beside(path);
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(Path::new("/s/locks")));
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(".x.json.") && name.ends_with(".tmp"),
            "{name}"
        );
        assert_eq!(parent(Path::new("x.json")), Path::new("."));
    }

    #[test]
    fn a_shared_lock_file_this_user_may_not_read_is_refused_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.lock");
        fs::write(&path, "kept").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        if fs::File::open(&path).is_ok() {
            eprintln!("skipped: modes do not bind this user (root)");
            return;
        }
        let err = me().open_lock(&path, LockOpen::Shared).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_read_that_fails_after_the_file_opened_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = me().read(tmp.path()).unwrap_err();
        assert_eq!(err.to_string(), errmsg::AS_USER_FAILED);
    }

    /// The temp file is made new, never through what is at its name: a
    /// symlink planted there is refused and its target left alone. Called in
    /// this process, as the child would.
    #[test]
    fn a_write_never_follows_a_symlink_at_the_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::write(&target, "untouched").unwrap();
        let path = tmp.path().join("lock.json");
        let mut prepared = me().prepare(&path, &Op::Write(b"{}")).unwrap();
        let planted = tmp.path().join("planted.tmp");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        prepared.tmp = c_path(&planted).unwrap();
        assert_eq!(write(&prepared, b"{}"), Err(Errno::EXIST));
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        assert!(!path.exists());
    }

    #[test]
    fn says_whether_a_path_is_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(me().is_dir(tmp.path()).unwrap());
        let file = tmp.path().join("f");
        assert!(!me().is_dir(&file).unwrap());
        fs::write(&file, "").unwrap();
        assert!(!me().is_dir(&file).unwrap());
    }

    /// Resolution needs only the right to reach the file, not to read it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn canonicalizes_through_every_symlink_and_a_missing_path_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let file = real.join("bosun.yaml");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let want = fs::canonicalize(&file).unwrap();
        assert_eq!(me().canonicalize(&link.join("bosun.yaml")).unwrap(), want);
        assert_eq!(
            me().canonicalize(&link).unwrap(),
            fs::canonicalize(&real).unwrap()
        );
        let err = me().canonicalize(&link.join("gone")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn writes_in_place_through_a_symlink_keeping_the_mode_and_makes_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("kept.yaml");
        fs::write(&target, "old old old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = tmp.path().join("bosun.yaml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        me().write_in_place(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let fresh = tmp.path().join("fresh.yaml");
        me().write_in_place(&fresh, b"made").unwrap();
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "made");
        let names: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(names.len(), 3, "{names:?}");
    }

    #[test]
    fn a_write_in_place_the_user_may_not_make_fails_and_changes_nothing() {
        if root() {
            eprintln!("skipped: root may write anywhere");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("theirs.yaml");
        fs::write(&target, "kept").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
        let link = tmp.path().join("bosun.yaml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = me().write_in_place(&link, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
        let missing_dir = tmp.path().join("no").join("bosun.yaml");
        let err = me().write_in_place(&missing_dir, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// Root becomes `nobody`, who may neither write a root file through a
    /// symlink of theirs nor reach a root-only directory to resolve a path
    /// in it. Runs as root only (the real tier's container).
    #[test]
    fn root_writes_in_place_and_resolves_as_the_user_with_their_rights_alone() {
        if !root() {
            eprintln!("skipped: needs root to become another user");
            return;
        }
        let nobody = AsUser::new(65534, 65534, vec![65534]);
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let roots = tmp.path().join("roots.yaml");
        fs::write(&roots, "kept").unwrap();
        fs::set_permissions(&roots, fs::Permissions::from_mode(0o644)).unwrap();
        let theirs = tmp.path().join("theirs");
        fs::create_dir(&theirs).unwrap();
        let link = theirs.join("bosun.yaml");
        std::os::unix::fs::symlink(&roots, &link).unwrap();
        std::os::unix::fs::lchown(&link, Some(65534), Some(65534)).unwrap();
        std::os::unix::fs::chown(&theirs, Some(65534), Some(65534)).unwrap();
        let err = nobody.write_in_place(&link, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&roots).unwrap(), "kept");
        let private = tmp.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(private.join("x"), "").unwrap();
        let err = nobody.canonicalize(&private.join("x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let err = nobody.is_dir(&private.join("x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_write_in_place_makes_no_directories() {
        let path = Path::new("/h/u/proj/bosun.yaml");
        let prepared = me().prepare(path, &Op::WriteInPlace(b"")).unwrap();
        assert!(prepared.dirs.is_empty());
        assert!(me().prepare(path, &Op::Resolve).unwrap().dirs.is_empty());
        assert!(me().prepare(path, &Op::IsDir).unwrap().dirs.is_empty());
    }

    #[test]
    fn each_operation_is_named_for_the_log() {
        let names = [
            Op::Exists.name(),
            Op::Read.name(),
            Op::Write(b"").name(),
            Op::Lock(LockOpen::Shared).name(),
            Op::Lock(LockOpen::Exclusive).name(),
            Op::IsDir.name(),
            Op::Resolve.name(),
            Op::WriteInPlace(b"").name(),
        ];
        assert_eq!(
            names,
            [
                "exists",
                "read",
                "write",
                "open shared lock",
                "open exclusive lock",
                "is directory",
                "resolve",
                "write in place",
            ]
        );
    }

    #[test]
    fn a_path_with_a_nul_is_refused_before_anything_runs() {
        let err = me().read(Path::new("a\0b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_invoker_s_groups_come_from_the_group_database() {
        let user = Invoker {
            name: "root".to_string(),
            uid: 0,
            gid: 0,
            home: std::path::PathBuf::from("/root"),
        };
        let as_root = AsUser::invoker(&user);
        assert_eq!((as_root.uid, as_root.gid), (0, 0));
        assert!(as_root.groups.contains(&0), "{:?}", as_root.groups);
        let ghost = Invoker {
            name: "bosun-no-such-user".to_string(),
            uid: 4242,
            gid: 4343,
            home: std::path::PathBuf::from("/nonexistent"),
        };
        assert!(AsUser::invoker(&ghost).groups.contains(&4343));
    }
}
