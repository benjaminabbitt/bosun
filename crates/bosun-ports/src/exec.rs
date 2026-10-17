//! Side-effect ports every backend runs through, never calling `std::process`
//! or an HTTP client directly — so a backend is driven by a fake in tests and
//! a real adapter in production (bosun-host).
//!
//! [`Command`] names the program by its **logical name** (`npm`, `apt-get`),
//! not a resolved path: a real runner resolves PATH and, on Windows, the
//! `PATHEXT` shims (`npm` → `npm.cmd`, Appendix B #184). Keeping the name
//! logical here is what makes that resolution possible without changing this
//! port.

use crate::error::PortError;

/// A command to run: a logical program name plus its arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// The logical program name (resolved by the runner, incl. Windows shims).
    pub program: String,
    /// The arguments, already split (never a shell string).
    pub args: Vec<String>,
    /// Environment variables to set for this command (e.g.
    /// `DEBIAN_FRONTEND=noninteractive`), as `(name, value)` pairs.
    pub env: Vec<(String, String)>,
    /// What to write to the command's standard input, if anything.
    pub stdin: Option<String>,
}

impl Command {
    /// Build a command from a program name and an iterator of arguments, with
    /// no extra environment.
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Command {
        Command {
            program: program.into(),
            args: args.into_iter().collect(),
            env: Vec::new(),
            stdin: None,
        }
    }

    /// Give the command `input` on its standard input.
    pub fn with_stdin(mut self, input: impl Into<String>) -> Command {
        self.stdin = Some(input.into());
        self
    }

    /// Set the environment for this command.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Command {
        self.env = env;
        self
    }
}

/// The captured result of running a [`Command`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

impl Output {
    /// Whether the process exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Standard output decoded as UTF-8, lossily (for parsing tool output).
    pub fn stdout_utf8(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

/// Runs external commands. The mutation side of a backend goes through this;
/// it runs only on the local machine. `Send + Sync` so one runner can be
/// shared across parallel backend probes (§5.3).
pub trait CommandRunner: Send + Sync {
    /// Run `command` to completion, capturing its output. A non-zero exit is
    /// **not** an `Err` — it is an `Output` with a non-zero `code`, so the
    /// caller decides whether that is a failure (some backends read a
    /// non-zero status as "not installed"). `Err` is reserved for not being
    /// able to run the program at all.
    fn run(&self, command: &Command) -> Result<Output, PortError>;
}

/// An HTTP request: a URL plus headers (e.g. a GitHub auth token for rate
/// limits, §12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The absolute URL.
    pub url: String,
    /// Request headers, as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
}

impl HttpRequest {
    /// A GET request with no extra headers.
    pub fn get(url: impl Into<String>) -> HttpRequest {
        HttpRequest {
            url: url.into(),
            headers: Vec::new(),
        }
    }
}

/// An HTTP response: status and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body bytes.
    pub body: Vec<u8>,
}

/// Fetches over HTTP. The query side of a repo-bound backend (listing
/// versions, downloading a release archive) goes through this; it can run
/// anywhere, for any target (§4.7). `Send + Sync` so a backend holding one can
/// sit behind a `Send + Sync` factory (§5.3).
pub trait HttpFetcher: Send + Sync {
    /// Perform `request` and return the response. `Err` is reserved for
    /// transport failures; an error *status* is returned in [`HttpResponse`]
    /// for the caller to interpret.
    fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: Option<i32>) -> Output {
        Output {
            code,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn output_success_is_true_only_for_zero_exit() {
        assert!(out(Some(0)).success());
        assert!(!out(Some(1)).success());
        assert!(!out(None).success()); // killed by a signal
    }

    #[test]
    fn with_env_attaches_the_environment_and_preserves_the_command() {
        let cmd = Command::new("apt-get", ["install".to_string()]).with_env(vec![(
            "DEBIAN_FRONTEND".to_string(),
            "noninteractive".to_string(),
        )]);
        assert_eq!(cmd.program, "apt-get");
        assert_eq!(cmd.args, vec!["install".to_string()]);
        assert_eq!(
            cmd.env,
            vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]
        );
    }

    #[test]
    fn with_stdin_attaches_the_input() {
        let cmd = Command::new("uv", ["pip".to_string()]);
        assert_eq!(cmd.stdin, None);
        assert_eq!(
            cmd.with_stdin("httpie\n").stdin.as_deref(),
            Some("httpie\n")
        );
    }

    #[test]
    fn output_stdout_utf8_decodes_lossily() {
        let o = Output {
            code: Some(0),
            stdout: b"1.7.1\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(o.stdout_utf8(), "1.7.1\n");
    }
}
