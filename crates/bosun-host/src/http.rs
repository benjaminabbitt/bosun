//! The real [`HttpFetcher`] (§5.1, §4.7): fetches over HTTPS with ureq on
//! rustls (no OpenSSL, so a static musl build links cleanly). An error *status*
//! (404, 429, …) is returned in the [`HttpResponse`] for the backend to read —
//! only a transport failure is an `Err` — so the agent is configured not to
//! treat non-2xx as an error. The body limit is raised well above a release
//! archive's size so downloads (§12) are not truncated.

use bosun_ports::{HttpFetcher, HttpRequest, HttpResponse, PortError};

/// 512 MiB: comfortably above any release archive bosun fetches.
const BODY_LIMIT: u64 = 512 * 1024 * 1024;

/// Fetches over HTTP(S) via ureq.
pub struct UreqHttpFetcher {
    agent: ureq::Agent,
}

impl UreqHttpFetcher {
    /// Build a fetcher whose agent returns error statuses as responses rather
    /// than `Err` (the port contract, §5.1).
    pub fn new() -> UreqHttpFetcher {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        UreqHttpFetcher { agent }
    }
}

impl Default for UreqHttpFetcher {
    fn default() -> UreqHttpFetcher {
        UreqHttpFetcher::new()
    }
}

impl HttpFetcher for UreqHttpFetcher {
    fn fetch(&self, request: &HttpRequest) -> Result<HttpResponse, PortError> {
        let fail = |cause: String| PortError::HttpRequest {
            url: request.url.clone(),
            cause,
        };
        let mut builder = self.agent.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let mut response = builder.call().map_err(|e| fail(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|e| fail(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}
