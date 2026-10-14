//! The real [`CommandRunner`] (§5.1): runs external programs through
//! `std::process`. On Unix, `execvp` resolves the program against `PATH` and
//! never the current directory (a bare name with no slash), so a planted
//! binary in the working directory cannot be picked up. A non-zero exit is an
//! [`Output`] with that code, not an `Err` — only being unable to launch the
//! program at all is an `Err` (Windows `PATHEXT` shims are deferred, §5.1).

use std::process::Command as StdCommand;

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
        let output = cmd.output().map_err(|e| {
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
        })?;
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

    #[test]
    fn a_missing_program_is_command_not_found() {
        let err = ProcessCommandRunner
            .run(&Command::new("bosun-no-such-program-xyzzy", []))
            .unwrap_err();
        assert!(matches!(err, PortError::CommandNotFound { .. }));
    }
}
