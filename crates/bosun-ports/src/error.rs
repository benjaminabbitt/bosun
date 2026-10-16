//! The error type every port returns. Typed (thiserror), never `anyhow`, so
//! callers can match on failure modes — and so Appendix B's "write state only
//! after the command actually succeeded" discipline has distinct variants to
//! branch on.

use bosun_core::VersionError;

use crate::errmsg;

/// A failure from any port: command execution, HTTP, state I/O, or a backend
/// operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    /// The program is not on PATH (or its shim could not be resolved).
    #[error("{kind}: {program}", kind = errmsg::COMMAND_NOT_FOUND)]
    CommandNotFound {
        /// The program name requested.
        program: String,
    },

    /// A command exited non-zero. `code` is `None` if it was killed by a
    /// signal. The message ends with the last line it printed to stderr,
    /// usually the one that says why.
    #[error(
        "{kind}: {program} ({exit}){last}",
        kind = errmsg::COMMAND_FAILED,
        exit = exit_text(*.code),
        last = last_line(.stderr)
    )]
    CommandFailed {
        /// The program name.
        program: String,
        /// The exit code, or `None` if terminated by a signal.
        code: Option<i32>,
        /// Captured stderr, for the diagnostic.
        stderr: String,
    },

    /// A command's output did not match what the backend expected to parse.
    #[error("{kind}: {detail}", kind = errmsg::COMMAND_OUTPUT)]
    CommandOutput {
        /// What could not be parsed.
        detail: String,
    },

    /// An HTTP request could not be completed at all.
    #[error("{kind}: {url} ({cause})", kind = errmsg::HTTP_REQUEST)]
    HttpRequest {
        /// The URL requested.
        url: String,
        /// The transport-level cause.
        cause: String,
    },

    /// An HTTP request returned an error status.
    #[error("{kind}: {url} ({status})", kind = errmsg::HTTP_STATUS)]
    HttpStatus {
        /// The URL requested.
        url: String,
        /// The HTTP status code.
        status: u16,
    },

    /// Reading or writing a machine-state file failed.
    #[error("{kind}: {detail}", kind = errmsg::STATE_IO)]
    StateIo {
        /// What failed.
        detail: String,
    },

    /// A backend operation failed for a backend-specific reason.
    #[error("{kind}: {detail}", kind = errmsg::BACKEND)]
    Backend {
        /// The backend-specific detail.
        detail: String,
    },

    /// A backend was asked for a capability it does not have (§5.2); callers
    /// degrade with a warning.
    #[error("{kind}: {capability}", kind = errmsg::UNSUPPORTED)]
    Unsupported {
        /// The capability that is missing.
        capability: String,
    },

    /// A version string a backend reported could not be parsed under its
    /// scheme.
    #[error(transparent)]
    Version(#[from] VersionError),
}

/// How a command ended: `exit <code>`, or killed by a signal.
fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit {code}"),
        None => errmsg::KILLED_BY_SIGNAL.to_string(),
    }
}

/// `: <line>` for the last non-blank line of `stderr`, trimmed; empty when
/// there is none.
fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map_or_else(String::new, |l| format!(": {l}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(code: Option<i32>, stderr: &str) -> String {
        PortError::CommandFailed {
            program: "apt-get".to_string(),
            code,
            stderr: stderr.to_string(),
        }
        .to_string()
    }

    #[test]
    fn a_failed_command_names_its_exit_and_the_last_line_it_printed() {
        assert_eq!(
            failed(Some(100), "W: one\n  E: two  \n \n"),
            "command failed: apt-get (exit 100): E: two"
        );
    }

    #[test]
    fn a_failed_command_that_printed_nothing_names_only_its_exit() {
        assert_eq!(failed(Some(1), ""), "command failed: apt-get (exit 1)");
        assert_eq!(failed(Some(1), " \n\n"), "command failed: apt-get (exit 1)");
    }

    #[test]
    fn a_command_killed_by_a_signal_says_so() {
        assert_eq!(
            failed(None, "Killed"),
            "command failed: apt-get (killed by a signal): Killed"
        );
    }
}
