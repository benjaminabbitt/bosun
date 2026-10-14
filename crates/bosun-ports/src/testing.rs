//! Test doubles for the side-effect ports, so a backend can be driven without
//! touching a real process or the network. Shipped (not `#[cfg(test)]`) so
//! every downstream crate's tests can use them.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::error::PortError;
use crate::exec::{Command, CommandRunner, HttpFetcher, HttpRequest, HttpResponse, Output};

/// A [`CommandRunner`] that replays pre-programmed outputs in order and
/// records every command it was asked to run. `Mutex` (not `RefCell`) so it is
/// `Sync` and can be shared across parallel backend probes (§5.3).
#[derive(Default)]
pub struct FakeCommandRunner {
    responses: Mutex<VecDeque<Result<Output, PortError>>>,
    calls: Mutex<Vec<Command>>,
}

impl FakeCommandRunner {
    /// An empty runner; every call errors until outputs are enqueued.
    pub fn new() -> FakeCommandRunner {
        FakeCommandRunner::default()
    }

    /// Enqueue a successful output (exit 0, given stdout, empty stderr).
    pub fn with_stdout(self, stdout: impl Into<Vec<u8>>) -> FakeCommandRunner {
        self.with_output(Output {
            code: Some(0),
            stdout: stdout.into(),
            stderr: Vec::new(),
        })
    }

    /// Enqueue a raw output to return on the next call.
    pub fn with_output(self, output: Output) -> FakeCommandRunner {
        self.responses.lock().unwrap().push_back(Ok(output));
        self
    }

    /// The commands run so far, in order.
    pub fn calls(&self) -> Vec<Command> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run(&self, command: &Command) -> Result<Output, PortError> {
        self.calls.lock().unwrap().push(command.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(PortError::CommandNotFound {
                    program: command.program.clone(),
                })
            })
    }
}

/// An [`HttpFetcher`] that returns responses mapped by URL and records every
/// request. `Mutex` so it is `Sync` and can back a backend held by a
/// `Send + Sync` factory (§5.3).
#[derive(Default)]
pub struct FakeHttpFetcher {
    responses: Mutex<HashMap<String, HttpResponse>>,
    calls: Mutex<Vec<HttpRequest>>,
}

impl FakeHttpFetcher {
    /// An empty fetcher; every request errors until responses are mapped.
    pub fn new() -> FakeHttpFetcher {
        FakeHttpFetcher::default()
    }

    /// Map `url` to a 200 response with `body`.
    pub fn with_body(self, url: impl Into<String>, body: impl Into<Vec<u8>>) -> FakeHttpFetcher {
        self.with_response(
            url,
            HttpResponse {
                status: 200,
                body: body.into(),
            },
        )
    }

    /// Map `url` to a raw response.
    pub fn with_response(self, url: impl Into<String>, response: HttpResponse) -> FakeHttpFetcher {
        self.responses.lock().unwrap().insert(url.into(), response);
        self
    }

    /// The requests made so far, in order.
    pub fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap().clone()
    }
}

impl HttpFetcher for FakeHttpFetcher {
    fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, PortError> {
        self.calls.lock().unwrap().push(request.clone());
        self.responses
            .lock()
            .unwrap()
            .get(&request.url)
            .cloned()
            .ok_or_else(|| PortError::HttpRequest {
                url: request.url.clone(),
                cause: "no fake response mapped".to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_runner_replays_outputs_in_order_and_records_calls() {
        let runner = FakeCommandRunner::new()
            .with_stdout("first")
            .with_stdout("second");
        let a = runner.run(&Command::new("apt-get", [])).unwrap();
        let b = runner.run(&Command::new("dpkg", [])).unwrap();
        assert_eq!(a.stdout_utf8(), "first");
        assert_eq!(b.stdout_utf8(), "second");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "apt-get");
        assert_eq!(calls[1].program, "dpkg");
    }

    #[test]
    fn command_runner_errors_when_exhausted() {
        let runner = FakeCommandRunner::new();
        assert!(matches!(
            runner.run(&Command::new("apt", [])),
            Err(PortError::CommandNotFound { .. })
        ));
    }

    #[test]
    fn http_fetcher_returns_mapped_response_and_records_requests() {
        let fetcher = FakeHttpFetcher::new().with_body("https://api/x", "payload");
        let resp = fetcher.fetch(&HttpRequest::get("https://api/x")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"payload");
        assert_eq!(fetcher.calls().len(), 1);
        assert_eq!(fetcher.calls()[0].url, "https://api/x");
    }

    #[test]
    fn http_fetcher_errors_on_unmapped_url() {
        let fetcher = FakeHttpFetcher::new();
        assert!(matches!(
            fetcher.fetch(&HttpRequest::get("https://api/missing")),
            Err(PortError::HttpRequest { .. })
        ));
    }
}
