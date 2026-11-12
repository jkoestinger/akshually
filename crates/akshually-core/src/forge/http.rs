//! The HTTP plumbing every Forge adapter shares: one agent, the host's token (fetched again after
//! the host refuses it), endpoints on the host's API, and turning a refused request into a
//! [`ForgeError`]. What differs between Forges (where the token comes from, how it's sent, how
//! an error body reads) each adapter supplies as a [`Dialect`].

use std::sync::Mutex;

use ureq::http::Response;
use ureq::{Body, RequestBuilder};
use url::Url;

use super::{CredentialSources, ForgeError, HostConfig, PullRequestRef, REQUEST_TIMEOUT};

/// The most of a file read for its contents: GitHub serves raw contents up to 100 MB.
const MAX_FILE: u64 = 100 * 1024 * 1024;

/// What one Forge's API does its own way, as far as the [`Client`] cares.
pub(super) struct Dialect {
    /// The token for `host`, from wherever this Forge's tokens come from.
    pub(super) token: fn(&CredentialSources, &HostConfig) -> Option<String>,
    /// The `Authorization` header's value carrying `token`.
    pub(super) authorization: fn(&str) -> String,
    /// The message in an error answer's `body`, or the body itself when it has none.
    pub(super) error_message: fn(String) -> String,
}

/// Requests to one Forge host's API.
pub(super) struct Client {
    host: HostConfig,
    /// The REST API's base URL, without a trailing `/`.
    api: String,
    credentials: CredentialSources,
    dialect: Dialect,
    /// Resolved on first use; forgotten when the host refuses it, so a fresh login is picked up.
    token: Mutex<Option<String>>,
    agent: ureq::Agent,
}

impl Client {
    /// The client for `host`, whose API is at `api` (a trailing `/` is dropped).
    pub(super) fn new(host: HostConfig, api: &str, credentials: CredentialSources, dialect: Dialect) -> Self {
        let agent = ureq::Agent::config_builder()
            // Read 4xx/5xx bodies for the Forge's error messages.
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .user_agent(concat!("akshually/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let api = api.trim_end_matches('/').to_owned();
        Client { host, api, credentials, dialect, token: Mutex::new(None), agent }
    }

    /// The Forge's web host, as configured.
    pub(super) fn host(&self) -> &str {
        &self.host.host
    }

    /// The REST API's base URL, without a trailing `/`.
    pub(super) fn api(&self) -> &str {
        &self.api
    }

    /// The agent to build requests with, for [`Client::send`].
    pub(super) fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// `segments` appended to the REST base, each percent-encoded (so a GitLab project path's
    /// `/` becomes `%2F`, as GitLab wants).
    pub(super) fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = Url::parse(&self.api).expect("the API base is a URL");
        url.path_segments_mut().expect("the API base is a URL with a path").pop_if_empty().extend(segments);
        url
    }

    /// Sends `request` with the token, through `send`. Any status but 2xx and 304 (which
    /// answers a conditional request: unchanged) is an error; a 401 forgets the token, so the
    /// next request fetches it again. Blocks on the network.
    pub(super) fn send<B>(
        &self,
        request: RequestBuilder<B>,
        send: impl FnOnce(RequestBuilder<B>) -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, ForgeError> {
        let request = request.header("Authorization", (self.dialect.authorization)(&self.token()?));
        let mut response = send(request).map_err(|e| self.http(e))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) || status == 304 {
            return Ok(response);
        }
        let host = self.host.host.clone();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        Err(match status {
            401 => {
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
                ForgeError::Unauthorized { host, status }
            }
            404 => ForgeError::NotFound(host),
            _ => ForgeError::Api { host, status, message: (self.dialect.error_message)(body) },
        })
    }

    /// The contents `request` answers, up to [`MAX_FILE`]. Blocks on the network.
    pub(super) fn file(&self, request: RequestBuilder<ureq::typestate::WithoutBody>) -> Result<Vec<u8>, ForgeError> {
        self.send(request, RequestBuilder::call)?
            .body_mut()
            .with_config()
            .limit(MAX_FILE)
            .read_to_vec()
            .map_err(|e| self.http(e))
    }

    /// The request never got an answer.
    pub(super) fn http(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Http { host: self.host.host.clone(), message: error.to_string() }
    }

    /// The answer made no sense.
    pub(super) fn unexpected(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Unexpected { host: self.host.host.clone(), message: error.to_string() }
    }

    fn token(&self) -> Result<String, ForgeError> {
        let mut cached = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            *cached = (self.dialect.token)(&self.credentials, &self.host);
        }
        cached.clone().ok_or_else(|| self.credentials.none_for(&self.host))
    }
}

/// `error`, with a 404 naming the Pull Request `pr` rather than the host.
pub(super) fn not_found_as(error: ForgeError, pr: &PullRequestRef) -> ForgeError {
    match error {
        ForgeError::NotFound(_) => ForgeError::NotFound(pr.to_string()),
        e => e,
    }
}

/// Whether `url` is a web page on `host` (a `host:port` when it has a port), whatever its case.
pub(super) fn host_matches(url: &Url, host: &str) -> bool {
    if !matches!(url.scheme(), "https" | "http") {
        return false;
    }
    let Some(name) = url.host_str() else { return false };
    let at = match url.port() {
        Some(port) => format!("{name}:{port}"),
        None => name.to_owned(),
    };
    at.eq_ignore_ascii_case(host)
}
