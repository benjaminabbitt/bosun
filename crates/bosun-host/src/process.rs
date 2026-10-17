//! The real [`CommandRunner`] (§5.1): runs external programs through
//! `std::process`. On Unix, `execvp` resolves the program against `PATH` and
//! never the current directory (a bare name with no slash), so a planted
//! binary in the working directory cannot be picked up. A non-zero exit is an
//! [`Output`] with that code, not an `Err` — only being unable to launch the
//! program at all is an `Err` (Windows `PATHEXT` shims are deferred, §5.1).
//! A command's standard input is its [`Command::stdin`], or nothing.

use std::io::Write;
use std::process::{Command as StdCommand, Stdio};

use bosun_ports::{Command, CommandRunner, Output, PortError};

/// Runs commands on the local host via `std::process`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, command: &Command) -> Result<Output, PortError> {
        let mut cmd = StdCommand::new(&command.program);
        cmd.args(&command.args);
        for (key, value) in &command.env {
            cmd.env(key, value);
        }
        let failed = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PortError::CommandNotFound {
                    program: command.program.clone(),
                }
            } else {
                PortError::CommandFailed {
                    program: command.program.clone(),
                    code: None,
                    stderr: e.to_string(),
                }
            }
        };
        let output = match &command.stdin {
            None => cmd.output().map_err(failed)?,
            Some(input) => {
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = cmd.spawn().map_err(failed)?;
                let mut pipe = child.stdin.take().expect("stdin is piped");
                // Written from its own thread, so a child that prints before
                // it reads cannot block on a full pipe. A child that stops
                // reading early closes the pipe; what it then does is its
                // exit status and output, so the write's own result is not.
                std::thread::scope(|s| {
                    s.spawn(move || {
                        let _ = pipe.write_all(input.as_bytes());
                    });
                    child.wait_with_output()
                })
                .map_err(failed)?
            }
        };
        Ok(Output {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_program_and_captures_output() {
        let out = ProcessCommandRunner
            .run(&Command::new("echo", ["hi".to_string()]))
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_utf8().trim(), "hi");
    }

    #[test]
    fn a_nonzero_exit_is_an_output_not_an_error() {
        // `false` exits 1; that is an Output with code 1, not an Err.
        let out = ProcessCommandRunner
            .run(&Command::new("false", []))
            .unwrap();
        assert_eq!(out.code, Some(1));
        assert!(!out.success());
    }

    #[cfg(unix)]
    #[test]
    fn stdin_reaches_the_program() {
        let out = ProcessCommandRunner
            .run(&Command::new("cat", []).with_stdin("httpie>=3\n"))
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_utf8(), "httpie>=3\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_ignores_stdin_still_runs() {
        let out = ProcessCommandRunner
            .run(&Command::new("true", []).with_stdin("x".repeat(1 << 20)))
            .unwrap();
        assert!(out.success());
    }

    #[test]
    fn a_missing_program_is_command_not_found() {
        let err = ProcessCommandRunner
            .run(&Command::new("bosun-no-such-program-xyzzy", []))
            .unwrap_err();
        assert!(matches!(err, PortError::CommandNotFound { .. }));
    }
}
