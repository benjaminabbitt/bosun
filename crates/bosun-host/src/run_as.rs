//! [`CommandRunner`] as another user (§5.3, §5.5): how an elevated run asks
//! a user-scope backend (cargo, npm, uv) what the user who elevated it has
//! installed. Each command runs in a forked child that drops to the user's
//! groups, group id and user id exactly as [`AsUser`] does, moves to their
//! home, and execs the program with an environment of the user's own
//! (`HOME`, `USER`, `LOGNAME`, `PATH`, and what the command sets), never
//! root's: the tool reads the user's config and data, with the user's
//! rights.
//!
//! As in [`AsUser`], the child may share the process with other threads, so
//! everything it needs is made before the fork: the program's candidate
//! paths along `PATH` (tried in order with `execve`, as `execvp` would, but
//! with no allocation), the argument and environment arrays, the home, and
//! the pipes. The child makes system calls only, and on any failure writes
//! its errno to a close-on-exec status pipe and leaves by `_exit`; the
//! parent reads that pipe to tell a program that ran from one that never
//! started.

use std::ffi::{c_char, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bosun_ports::{Command, CommandRunner, FileAccess, Output, PortError};
use nix::unistd::ForkResult;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::as_user::{become_user, reap, AsUser, Creds};
use crate::errmsg;
use crate::invoker::Invoker;
use crate::logmsg;

/// Runs commands as one user, from a process that may become them (root, or
/// that user already).
#[derive(Debug, Clone)]
pub struct AsUserRunner {
    user: AsUser,
    /// Where each command runs: the user's home.
    home: PathBuf,
    /// The environment each command starts from.
    env: Vec<(String, String)>,
    /// `PATH`'s directories, where a program name is looked for.
    path: Vec<PathBuf>,
}

impl AsUserRunner {
    /// Run as `user`, from `home`, with `env` (whose `PATH` is where programs
    /// are looked for) and what each command adds.
    pub fn new(user: AsUser, home: PathBuf, env: Vec<(String, String)>) -> AsUserRunner {
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| std::env::split_paths(v).collect())
            .unwrap_or_default();
        AsUserRunner {
            user,
            home,
            env,
            path,
        }
    }

    /// Run as the user who elevated this run, from their home, with their
    /// environment ([`user_env`]); `path_var` is this process's `PATH`.
    pub fn invoker(user: &Invoker, path_var: Option<&str>) -> AsUserRunner {
        let as_user = AsUser::invoker(user);
        let env = user_env(user, path_var, &as_user);
        AsUserRunner::new(as_user, user.home.clone(), env)
    }

    /// The `PATH` the commands run with.
    pub fn path_var(&self) -> String {
        self.env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// The directories, under the user's home, where their own tools' installers
/// put programs: `~/.local/bin` (uv, pipx, bosun's own github-release) and
/// `~/.cargo/bin` (rustup, `cargo install`).
fn own_bin_dirs(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".local").join("bin"),
        home.join(".cargo").join("bin"),
    ]
}

/// The environment a command run as `user` starts from: `HOME`, `USER` and
/// `LOGNAME` the user's, and a `PATH` of their own tools' directories
/// ([`own_bin_dirs`]) then this process's `path_var`, keeping only absolute
/// directories `reach` (as the user) finds, each once. sudo's `secure_path`
/// lacks the user's own directories, and root-only ones (`/root/.cargo/bin`)
/// they cannot reach are dropped. Nothing else of root's environment is
/// passed.
pub fn user_env(
    user: &Invoker,
    path_var: Option<&str>,
    reach: &dyn FileAccess,
) -> Vec<(String, String)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let current = path_var.map(std::env::split_paths).into_iter().flatten();
    for dir in own_bin_dirs(&user.home).into_iter().chain(current) {
        if dir.is_absolute() && !dirs.contains(&dir) && reach.is_dir(&dir).unwrap_or(false) {
            dirs.push(dir);
        }
    }
    let path = std::env::join_paths(&dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    tracing::debug!(
        message = logmsg::RUN_AS_USER_ENV,
        user = %user.name,
        home = %user.home.display(),
        path = %path
    );
    vec![
        ("HOME".to_string(), user.home.display().to_string()),
        ("USER".to_string(), user.name.clone()),
        ("LOGNAME".to_string(), user.name.clone()),
        ("PATH".to_string(), path),
    ]
}

/// Everything the child needs, made before the fork.
struct Prepared {
    creds: Creds,
    /// Where the program may be, in the order to try them.
    candidates: Vec<CString>,
    /// Keeps the strings `argv` points into alive.
    _args: Vec<CString>,
    argv: Vec<*const c_char>,
    _vars: Vec<CString>,
    envp: Vec<*const c_char>,
    home: CString,
}

fn c_string(bytes: &[u8]) -> Result<CString, PortError> {
    CString::new(bytes).map_err(|e| PortError::CommandFailed {
        program: String::from_utf8_lossy(bytes).into_owned(),
        code: None,
        stderr: e.to_string(),
    })
}

/// A null-terminated array of pointers into `strings`.
fn pointers(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

impl AsUserRunner {
    /// The environment for `command`: the runner's, with the command's own
    /// variables in place of any of the same name.
    fn env_for(&self, command: &Command) -> Vec<(String, String)> {
        self.env
            .iter()
            .filter(|(k, _)| !command.env.iter().any(|(ck, _)| ck == k))
            .chain(command.env.iter())
            .cloned()
            .collect()
    }

    /// Where `program` may be: itself when it names a path, else in each
    /// `PATH` directory, in order.
    fn candidates(&self, program: &str) -> Vec<PathBuf> {
        if program.contains('/') {
            return vec![PathBuf::from(program)];
        }
        self.path.iter().map(|dir| dir.join(program)).collect()
    }

    fn prepare(&self, command: &Command) -> Result<Prepared, PortError> {
        let candidates = self
            .candidates(&command.program)
            .iter()
            .map(|p| c_string(p.as_os_str().as_bytes()))
            .collect::<Result<_, _>>()?;
        let args: Vec<CString> = std::iter::once(&command.program)
            .chain(&command.args)
            .map(|a| c_string(a.as_bytes()))
            .collect::<Result<_, _>>()?;
        let vars: Vec<CString> = self
            .env_for(command)
            .iter()
            .map(|(k, v)| c_string(format!("{k}={v}").as_bytes()))
            .collect::<Result<_, _>>()?;
        Ok(Prepared {
            creds: self.user.creds(),
            candidates,
            argv: pointers(&args),
            _args: args,
            envp: pointers(&vars),
            _vars: vars,
            home: c_string(self.home.as_os_str().as_bytes())?,
        })
    }
}

/// A pipe whose ends close on exec.
#[cfg(not(target_vendor = "apple"))]
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?)
}

/// Apple has no `pipe2`: the ends are made close-on-exec just after.
#[cfg(target_vendor = "apple")]
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (r, w) = rustix::pipe::pipe()?;
    rustix::io::fcntl_setfd(&r, rustix::io::FdFlags::CLOEXEC)?;
    rustix::io::fcntl_setfd(&w, rustix::io::FdFlags::CLOEXEC)?;
    Ok((r, w))
}

/// `fd`, moved above the standard streams (0, 1, 2), so putting one of the
/// child's ends in place never closes another (this process may have been
/// started with a standard stream closed, and a pipe end then is one).
fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    Ok(rustix::io::fcntl_dupfd_cloexec(&fd, 3)?)
}

/// The child's ends of its standard streams, and the parent's.
struct Pipes {
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
    status: OwnedFd,
    to_stdin: Option<OwnedFd>,
    from_stdout: OwnedFd,
    from_stderr: OwnedFd,
    from_status: OwnedFd,
}

fn pipes(feed: bool) -> io::Result<Pipes> {
    let (stdin, to_stdin) = if feed {
        let (r, w) = pipe()?;
        (r, Some(w))
    } else {
        let null = rustix::fs::open(
            c"/dev/null",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        (null, None)
    };
    let (from_stdout, stdout) = pipe()?;
    let (from_stderr, stderr) = pipe()?;
    let (from_status, status) = pipe()?;
    Ok(Pipes {
        stdin: above_stdio(stdin)?,
        stdout: above_stdio(stdout)?,
        stderr: above_stdio(stderr)?,
        status,
        to_stdin,
        from_stdout,
        from_stderr,
        from_status,
    })
}

/// Where the child stopped, sent with the errno.
const CREDENTIALS: u8 = 1;
const EXEC: u8 = 2;
const STREAMS: u8 = 3;

impl CommandRunner for AsUserRunner {
    fn run(&self, command: &Command) -> Result<Output, PortError> {
        tracing::debug!(
            message = logmsg::RUN_AS_USER,
            uid = self.user.uid(),
            program = %command.program
        );
        let failed = |detail: String| PortError::CommandFailed {
            program: command.program.clone(),
            code: None,
            stderr: detail,
        };
        let prepared = self.prepare(command)?;
        let pipes = pipes(command.stdin.is_some()).map_err(|e| failed(e.to_string()))?;
        // SAFETY: the child runs only `child`, which makes system calls on
        // what `prepared` and `pipes` hold (made above), allocates nothing,
        // takes no lock, and ends in `execve` or `_exit`; so a lock another
        // thread held at the fork is never waited on, and nothing of the
        // parent's runs twice.
        match unsafe { nix::unistd::fork() }.map_err(|e| failed(e.to_string()))? {
            ForkResult::Child => child(&prepared, &pipes),
            ForkResult::Parent { child } => {
                let Pipes {
                    stdin,
                    stdout,
                    stderr,
                    status,
                    to_stdin,
                    from_stdout,
                    from_stderr,
                    from_status,
                } = pipes;
                drop((stdin, stdout, stderr, status));
                let collected = collect(command, to_stdin, from_stdout, from_stderr, from_status);
                let code = reap(child).map_err(|e| failed(e.to_string()))?;
                let collected = collected.map_err(|e| failed(e.to_string()))?;
                match collected.stopped {
                    Some((EXEC, errno)) if errno == Errno::NOENT.raw_os_error() => {
                        Err(PortError::CommandNotFound {
                            program: command.program.clone(),
                        })
                    }
                    Some((stage, errno)) => Err(failed(format!(
                        "{}: {}",
                        stage_text(stage),
                        io::Error::from_raw_os_error(errno)
                    ))),
                    None => Ok(Output {
                        code: (code >= 0).then_some(code),
                        stdout: collected.stdout,
                        stderr: collected.stderr,
                    }),
                }
            }
        }
    }
}

/// What the child was doing when it stopped.
fn stage_text(stage: u8) -> &'static str {
    match stage {
        CREDENTIALS => errmsg::RUN_AS_USER_CREDENTIALS,
        STREAMS => errmsg::RUN_AS_USER_STREAMS,
        _ => errmsg::RUN_AS_USER_EXEC,
    }
}

/// What the parent took from the child.
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Where the child stopped, and the errno, when it never ran the
    /// program.
    stopped: Option<(u8, i32)>,
}

/// The child's output, and where it stopped if it never ran the program:
/// stdin written from its own thread and stdout and stderr read from
/// theirs, so no pipe fills while another is waited on.
fn collect(
    command: &Command,
    to_stdin: Option<OwnedFd>,
    from_stdout: OwnedFd,
    from_stderr: OwnedFd,
    from_status: OwnedFd,
) -> io::Result<Collected> {
    let read_all = |fd: OwnedFd| -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::from(fd).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    std::thread::scope(|s| {
        if let (Some(fd), Some(input)) = (to_stdin, &command.stdin) {
            // A child that stops reading early closes the pipe; what it then
            // does is its exit status and output, so the write's own result
            // is not.
            s.spawn(move || {
                let _ = File::from(fd).write_all(input.as_bytes());
            });
        }
        let err = s.spawn(move || read_all(from_stderr));
        let out = read_all(from_stdout)?;
        let err = err.join().unwrap_or_else(|_| Ok(Vec::new()))?;
        let status = read_all(from_status)?;
        let stopped = match status.as_slice() {
            [stage, a, b, c, d] => Some((*stage, i32::from_le_bytes([*a, *b, *c, *d]))),
            _ => None,
        };
        Ok(Collected {
            stdout: out,
            stderr: err,
            stopped,
        })
    })
}

// --- the child: system calls only, on what `Prepared` and `Pipes` hold -----

/// Set up the streams, become the user, move to their home, and exec the
/// program; on failure, say where and why on the status pipe, and exit.
fn child(p: &Prepared, pipes: &Pipes) -> ! {
    let (stage, errno) = match streams(pipes) {
        Err(e) => (STREAMS, e),
        Ok(()) => match become_user(&p.creds) {
            Err(e) => (CREDENTIALS, e),
            Ok(()) => {
                if rustix::process::chdir(p.home.as_c_str()).is_err() {
                    let _ = rustix::process::chdir(c"/");
                }
                (EXEC, exec(p))
            }
        },
    };
    let mut report = [stage, 0, 0, 0, 0];
    report[1..].copy_from_slice(&errno.raw_os_error().to_le_bytes());
    let _ = rustix::io::write(&pipes.status, &report);
    // SAFETY: `_exit` is async-signal-safe and ends the child without running
    // the parent's exit handlers, destructors or stdio flushes.
    unsafe { nix::libc::_exit(127) }
}

/// The child's standard streams (its ends, each above 0, 1 and 2, copied
/// there open across the exec), and signals as a new program expects them:
/// none blocked, and `SIGPIPE` back to its default, which this process
/// ignores.
fn streams(pipes: &Pipes) -> Result<(), Errno> {
    rustix::stdio::dup2_stdin(pipes.stdin.as_fd())?;
    rustix::stdio::dup2_stdout(pipes.stdout.as_fd())?;
    rustix::stdio::dup2_stderr(pipes.stderr.as_fd())?;
    // SAFETY: `sigemptyset` fills the set on this stack, `sigprocmask` reads
    // it, and `signal` takes constants; all three are async-signal-safe.
    unsafe {
        let mut none = std::mem::MaybeUninit::<nix::libc::sigset_t>::uninit();
        nix::libc::sigemptyset(none.as_mut_ptr());
        nix::libc::sigprocmask(nix::libc::SIG_SETMASK, none.as_ptr(), std::ptr::null_mut());
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_DFL);
    }
    Ok(())
}

/// Exec the first candidate that runs, as `execvp` searches: one missing
/// or in a directory that is not one goes on to the next, one this user may
/// not run is remembered and the search goes on; anything else stops it.
/// Returns only on failure, with the errno to report.
fn exec(p: &Prepared) -> Errno {
    let mut denied = false;
    for path in &p.candidates {
        // SAFETY: `path`, and each pointer in `argv` and `envp`, point into
        // NUL-terminated strings `p` holds, and both arrays end in a null
        // pointer; `execve` only returns on failure.
        unsafe {
            nix::libc::execve(path.as_ptr(), p.argv.as_ptr(), p.envp.as_ptr());
        }
        let errno = Errno::from_raw_os_error(nix::errno::Errno::last_raw());
        match errno {
            Errno::NOENT | Errno::NOTDIR => {}
            Errno::ACCESS => denied = true,
            other => return other,
        }
    }
    if denied {
        Errno::ACCESS
    } else {
        Errno::NOENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::FdFlags;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

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

    /// A runner as this process's own user, from `home`, with this
    /// process's `PATH`.
    fn runner(home: &Path) -> AsUserRunner {
        let path = std::env::var("PATH").unwrap_or_default();
        AsUserRunner::new(
            me(),
            home.to_path_buf(),
            vec![
                ("HOME".to_string(), home.display().to_string()),
                ("USER".to_string(), "ben".to_string()),
                ("LOGNAME".to_string(), "ben".to_string()),
                ("PATH".to_string(), path),
            ],
        )
    }

    fn cmd(program: &str, args: &[&str]) -> Command {
        Command::new(program, args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn runs_a_program_on_path_and_captures_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let out = runner(tmp.path())
            .run(&cmd("echo", &["hi", "there"]))
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout_utf8(), "hi there\n");
        let out = runner(tmp.path())
            .run(&cmd("sh", &["-c", "echo oops >&2; exit 3"]))
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn a_program_killed_by_a_signal_has_no_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let out = runner(tmp.path())
            .run(&cmd("sh", &["-c", "kill -9 $$"]))
            .unwrap();
        assert_eq!(out.code, None);
    }

    #[test]
    fn the_program_gets_the_user_s_environment_and_none_of_this_process_s() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().display().to_string();
        let command = cmd("env", &[]).with_env(vec![
            ("LOGNAME".to_string(), "ann".to_string()),
            ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
        ]);
        let out = runner(tmp.path()).run(&command).unwrap();
        let mut got: Vec<String> = out.stdout_utf8().lines().map(str::to_string).collect();
        got.sort();
        let path = std::env::var("PATH").unwrap_or_default();
        let mut want = vec![
            format!("HOME={home}"),
            "USER=ben".to_string(),
            "LOGNAME=ann".to_string(),
            format!("PATH={path}"),
            "DEBIAN_FRONTEND=noninteractive".to_string(),
        ];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn the_program_runs_in_the_user_s_home() {
        let tmp = tempfile::tempdir().unwrap();
        let out = runner(tmp.path()).run(&cmd("pwd", &[])).unwrap();
        let want = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(out.stdout_utf8().trim_end(), want.display().to_string());
        let gone = tmp.path().join("gone");
        let out = runner(&gone).run(&cmd("pwd", &[])).unwrap();
        assert_eq!(out.stdout_utf8(), "/\n");
    }

    #[test]
    fn stdin_reaches_the_program_and_one_that_ignores_it_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let out = runner(tmp.path())
            .run(&cmd("cat", &[]).with_stdin("httpie>=3\n"))
            .unwrap();
        assert_eq!(out.stdout_utf8(), "httpie>=3\n");
        let out = runner(tmp.path())
            .run(&cmd("true", &[]).with_stdin("x".repeat(1 << 20)))
            .unwrap();
        assert_eq!(out.code, Some(0));
        let out = runner(tmp.path()).run(&cmd("cat", &[])).unwrap();
        assert!(out.stdout.is_empty(), "stdin is empty without input");
    }

    #[test]
    fn output_larger_than_a_pipe_holds_is_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let script = "head -c 1000000 /dev/zero; head -c 300000 /dev/zero >&2";
        let out = runner(tmp.path()).run(&cmd("sh", &["-c", script])).unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
        assert_eq!(out.stderr.len(), 300_000);
    }

    #[test]
    fn a_missing_program_is_not_found_and_a_path_runs_as_given() {
        let tmp = tempfile::tempdir().unwrap();
        let err = runner(tmp.path())
            .run(&cmd("bosun-no-such-program-xyzzy", &[]))
            .unwrap_err();
        assert_eq!(
            err,
            PortError::CommandNotFound {
                program: "bosun-no-such-program-xyzzy".to_string()
            }
        );
        let script = tmp.path().join("hello");
        fs::write(&script, "#!/bin/sh\necho from a path\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let out = runner(tmp.path())
            .run(&cmd(&script.display().to_string(), &[]))
            .unwrap();
        assert_eq!(out.stdout_utf8(), "from a path\n");
    }

    /// As `execvp`: a file this user may not run is passed over for the
    /// next on `PATH`, and with none after it the run is refused, not "not
    /// found".
    #[test]
    fn a_program_the_user_may_not_run_is_passed_over_then_refused() {
        if root() {
            eprintln!("skipped: modes do not bind root");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("tool"), "#!/bin/sh\necho first\n").unwrap();
        fs::set_permissions(first.join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(second.join("tool"), "#!/bin/sh\necho second\n").unwrap();
        fs::set_permissions(second.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        let env = |dirs: &[&Path]| {
            let path = std::env::join_paths(dirs).unwrap();
            vec![("PATH".to_string(), path.to_string_lossy().into_owned())]
        };
        let both = AsUserRunner::new(me(), tmp.path().into(), env(&[&first, &second]));
        assert_eq!(
            both.run(&cmd("tool", &[])).unwrap().stdout_utf8(),
            "second\n"
        );
        let only = AsUserRunner::new(me(), tmp.path().into(), env(&[&first]));
        match only.run(&cmd("tool", &[])).unwrap_err() {
            PortError::CommandFailed { stderr, code, .. } => {
                assert_eq!(code, None);
                assert!(stderr.contains(errmsg::RUN_AS_USER_EXEC), "{stderr}");
                assert!(stderr.contains("Permission denied"), "{stderr}");
            }
            other => panic!("want a refusal, got {other:?}"),
        }
    }

    #[test]
    fn another_user_cannot_be_become_without_root_and_nothing_runs() {
        if root() {
            eprintln!("skipped: root may become anyone");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let other = AsUser::new(me().uid() + 1, nix::unistd::getgid().as_raw(), Vec::new());
        let path = std::env::var("PATH").unwrap_or_default();
        let runner = AsUserRunner::new(other, tmp.path().into(), vec![("PATH".into(), path)]);
        let made = tmp.path().join("made");
        let script = format!("touch {}", made.display());
        match runner.run(&cmd("sh", &["-c", &script])).unwrap_err() {
            PortError::CommandFailed { stderr, .. } => {
                assert!(stderr.contains(errmsg::RUN_AS_USER_CREDENTIALS), "{stderr}");
            }
            other => panic!("want a refusal, got {other:?}"),
        }
        assert!(!made.exists());
    }

    /// Root becomes `nobody`: the program runs with nobody's ids and groups
    /// alone, and may not read a file only root may. Runs as root only (the
    /// real tier's container).
    #[test]
    fn root_runs_the_program_as_the_user_with_their_rights_alone() {
        if !root() {
            eprintln!("skipped: needs root to become another user");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let private = tmp.path().join("private");
        fs::write(&private, "secret").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
        let nobody = AsUser::new(65534, 65534, vec![65534]);
        let path = std::env::var("PATH").unwrap_or_default();
        let runner = AsUserRunner::new(nobody, tmp.path().into(), vec![("PATH".into(), path)]);
        let out = runner.run(&cmd("id", &[])).unwrap();
        assert_eq!(
            out.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = out.stdout_utf8();
        assert!(text.starts_with("uid=65534"), "{text}");
        assert!(text.contains("groups=65534"), "{text}");
        assert!(!text.contains("(root)"), "{text}");
        let cat = runner
            .run(&cmd("cat", &[&private.display().to_string()]))
            .unwrap();
        assert_ne!(cat.code, Some(0));
        assert!(cat.stdout.is_empty());
    }

    /// File access that finds the directories it is given.
    struct Finds(Vec<PathBuf>);

    impl FileAccess for Finds {
        fn exists(&self, path: &Path) -> io::Result<bool> {
            Ok(self.0.iter().any(|d| d == path))
        }
        fn is_dir(&self, path: &Path) -> io::Result<bool> {
            if path == Path::new("/denied") {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            self.exists(path)
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }
        fn read(&self, _: &Path) -> io::Result<Vec<u8>> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn write_in_place(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn open_lock(&self, _: &Path, _: bosun_ports::LockOpen) -> io::Result<File> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    fn ben() -> Invoker {
        Invoker {
            name: "ben".to_string(),
            uid: 1000,
            gid: 1000,
            home: PathBuf::from("/home/ben"),
        }
    }

    #[test]
    fn the_user_s_environment_is_theirs_with_their_own_tools_first_on_path() {
        let found = Finds(
            [
                "/home/ben/.cargo/bin",
                "/usr/bin",
                "/bin",
                "/root/.cargo/bin",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        );
        let env = user_env(
            &ben(),
            Some("/root/.cargo/bin:/usr/bin:relative::/usr/bin:/denied:/nonexistent:/bin"),
            &found,
        );
        assert_eq!(
            env,
            vec![
                ("HOME".to_string(), "/home/ben".to_string()),
                ("USER".to_string(), "ben".to_string()),
                ("LOGNAME".to_string(), "ben".to_string()),
                (
                    "PATH".to_string(),
                    "/home/ben/.cargo/bin:/root/.cargo/bin:/usr/bin:/bin".to_string()
                ),
            ]
        );
        let none = user_env(&ben(), None, &Finds(Vec::new()));
        assert_eq!(none[3], ("PATH".to_string(), String::new()));
    }

    #[test]
    fn the_user_s_own_tools_are_in_local_bin_and_cargo_bin() {
        assert_eq!(
            own_bin_dirs(Path::new("/home/ben")),
            [
                PathBuf::from("/home/ben/.local/bin"),
                PathBuf::from("/home/ben/.cargo/bin")
            ]
        );
    }

    #[test]
    fn the_path_the_commands_run_with_is_the_environment_s() {
        let with = AsUserRunner::new(
            me(),
            PathBuf::from("/h"),
            vec![
                ("HOME".to_string(), "/h".to_string()),
                ("PATH".to_string(), "/a:/b".to_string()),
            ],
        );
        assert_eq!(with.path_var(), "/a:/b");
        let without = AsUserRunner::new(me(), PathBuf::from("/h"), Vec::new());
        assert_eq!(without.path_var(), "");
    }

    #[test]
    fn a_program_is_looked_for_along_path_unless_it_names_a_path() {
        let runner = AsUserRunner::new(
            me(),
            PathBuf::from("/h"),
            vec![("PATH".to_string(), "/a:/b".to_string())],
        );
        assert_eq!(
            runner.candidates("npm"),
            [PathBuf::from("/a/npm"), PathBuf::from("/b/npm")]
        );
        assert_eq!(runner.candidates("./x"), [PathBuf::from("./x")]);
        assert_eq!(
            runner.candidates("/usr/bin/x"),
            [PathBuf::from("/usr/bin/x")]
        );
        let no_path = AsUserRunner::new(me(), PathBuf::from("/h"), Vec::new());
        assert!(no_path.candidates("npm").is_empty());
    }

    #[test]
    fn a_child_s_end_is_never_a_standard_stream() {
        let null = rustix::fs::open(
            c"/dev/null",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let moved = above_stdio(null).unwrap();
        assert!(moved.as_raw_fd() > 2);
        let flags = rustix::io::fcntl_getfd(&moved).unwrap();
        assert!(flags.contains(FdFlags::CLOEXEC));
    }

    #[test]
    fn each_stage_is_named_in_the_error() {
        assert_eq!(stage_text(CREDENTIALS), errmsg::RUN_AS_USER_CREDENTIALS);
        assert_eq!(stage_text(STREAMS), errmsg::RUN_AS_USER_STREAMS);
        assert_eq!(stage_text(EXEC), errmsg::RUN_AS_USER_EXEC);
    }

    #[test]
    fn an_argument_with_a_nul_is_refused_before_anything_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let err = runner(tmp.path()).run(&cmd("echo", &["a\0b"])).unwrap_err();
        assert!(matches!(err, PortError::CommandFailed { .. }), "{err:?}");
    }
}
