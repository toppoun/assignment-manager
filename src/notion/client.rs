use std::{error::Error, fmt, io::Read, thread, time::Duration};

use reqwest::{
    Method, Url,
    blocking::{Client, Request, RequestBuilder},
    header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER},
};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::wire::{property_id, uuid};

pub const NOTION_VERSION: &str = "2026-03-11";
const BASE: &str = "https://api.notion.com";
const ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotionError {
    Configuration,
    Authentication,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Overloaded,
    Server(u16),
    Rejected(u16),
    Timeout,
    Transport,
    InvalidResponse,
    IncompleteSnapshot,
    SchemaMismatch,
    UnsupportedProperty,
    InvalidWrite,
    MethodDisabled,
}

impl fmt::Display for NotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(status) | Self::Rejected(status) => write!(f, "Notion HTTP {status}"),
            category => write!(f, "Notion {category:?}"),
        }
    }
}

impl Error for NotionError {}

/// The production transport exposes only read operations. Mutation requests
/// constructed by the dry-run have no production dispatch method.
pub struct NotionClient {
    http: Client,
    base: Url,
    retry_base: Duration,
}

struct Failed {
    error: NotionError,
    retry_after: Option<Duration>,
}

impl From<NotionError> for Failed {
    fn from(error: NotionError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

impl NotionClient {
    pub fn new(token: &str) -> Result<Self, NotionError> {
        Self::build(
            BASE,
            token,
            true,
            Duration::from_secs(12),
            Duration::from_millis(250),
        )
    }

    fn build(
        base: &str,
        token: &str,
        https_only: bool,
        timeout: Duration,
        retry_base: Duration,
    ) -> Result<Self, NotionError> {
        if token.is_empty() || token.bytes().any(|b| !b.is_ascii_graphic()) {
            return Err(NotionError::Configuration);
        }
        let mut headers = HeaderMap::new();
        let authorization_text = Zeroizing::new(format!("Bearer {token}"));
        let mut authorization =
            HeaderValue::from_str(&authorization_text).map_err(|_| NotionError::Configuration)?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        headers.insert("notion-version", HeaderValue::from_static(NOTION_VERSION));
        let http = Client::builder()
            .https_only(https_only)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .connect_timeout(timeout.min(Duration::from_secs(4)))
            .timeout(timeout)
            .tls_sslkeylogfile(false)
            .default_headers(headers)
            .build()
            .map_err(|_| NotionError::Configuration)?;
        Ok(Self {
            http,
            base: Url::parse(base).map_err(|_| NotionError::Configuration)?,
            retry_base,
        })
    }

    #[cfg(any(test, feature = "synthetic-https-fixture"))]
    pub(super) fn for_mock(
        address: std::net::SocketAddr,
        token: &str,
        timeout: Duration,
    ) -> Result<Self, NotionError> {
        if !address.ip().is_loopback() {
            return Err(NotionError::Configuration);
        }
        Self::build(
            &format!("http://{address}"),
            token,
            false,
            timeout,
            Duration::ZERO,
        )
    }

    pub(super) fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
    ) -> RequestBuilder {
        // Paths are built from fixed prefixes and validated identifiers.
        let mut url = self.base.clone();
        url.set_path(path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        self.http.request(method, url)
    }

    pub(super) fn read<T: DeserializeOwned>(
        &self,
        request: impl Fn() -> RequestBuilder,
    ) -> Result<T, NotionError> {
        for attempt in 0..ATTEMPTS {
            match self.send_read(request()) {
                Ok(result) => return Ok(result),
                Err(failure) => {
                    if attempt + 1 == ATTEMPTS || !retryable(failure.error) {
                        return Err(failure.error);
                    }
                    let Some(delay) = retry_delay(failure.retry_after, self.retry_base, attempt)
                    else {
                        return Err(failure.error);
                    };
                    thread::sleep(delay);
                }
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    #[cfg(test)]
    pub(super) fn send_mock_write(
        &self,
        request: &super::wire::PreparedWrite,
    ) -> Result<serde_json::Value, NotionError> {
        assert_eq!(
            self.base.scheme(),
            "http",
            "mock writes require a loopback client"
        );
        let request = self
            .request(request.method().clone(), request.path(), &[])
            .json(request.body())
            .build()
            .map_err(|_| NotionError::Configuration)?;
        self.execute(request).map_err(|failure| failure.error)
    }

    fn send_read<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Failed> {
        let request = request.build().map_err(|_| NotionError::Configuration)?;
        let url = request.url();
        if url.origin() != self.base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || !permits_read(request.method(), url.path())
        {
            return Err(NotionError::MethodDisabled.into());
        }
        self.execute(request)
    }

    fn execute<T: DeserializeOwned>(&self, request: Request) -> Result<T, Failed> {
        let response = self.http.execute(request).map_err(|error| {
            if error.is_timeout() {
                NotionError::Timeout
            } else {
                NotionError::Transport
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            let error = match status.as_u16() {
                401 => NotionError::Authentication,
                403 => NotionError::Forbidden,
                404 => NotionError::NotFound,
                409 => NotionError::Conflict,
                429 => NotionError::RateLimited,
                529 => NotionError::Overloaded,
                code @ 500..=599 => NotionError::Server(code),
                code => NotionError::Rejected(code),
            };
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(Failed { error, retry_after });
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE)
        {
            return Err(NotionError::InvalidResponse.into());
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE + 1)
            .read_to_end(&mut body)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut {
                    NotionError::Timeout
                } else {
                    NotionError::Transport
                }
            })?;
        if body.len() as u64 > MAX_RESPONSE {
            return Err(NotionError::InvalidResponse.into());
        }
        serde_json::from_slice(&body).map_err(|_| NotionError::InvalidResponse.into())
    }
}

fn permits_read(method: &Method, path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    let id = match (method, parts.as_slice()) {
        (&Method::GET, ["", "v1", "data_sources" | "pages", id])
        | (&Method::POST, ["", "v1", "data_sources", id, "query"]) => id,
        (&Method::GET, ["", "v1", "pages", id, "properties", property])
            if property_id(property) =>
        {
            id
        }
        _ => return false,
    };
    uuid(id).as_deref() == Some(*id)
}

fn retryable(error: NotionError) -> bool {
    matches!(
        error,
        NotionError::Timeout
            | NotionError::Transport
            | NotionError::RateLimited
            | NotionError::Overloaded
            | NotionError::Server(500 | 502 | 503 | 504)
    )
}

fn retry_delay(after: Option<Duration>, base: Duration, attempt: u32) -> Option<Duration> {
    // A large Retry-After aborts this operation rather than sending too early.
    let minimum = after.unwrap_or_default();
    if minimum > MAX_RETRY_AFTER {
        return None;
    }
    let fallback = base * 2u32.pow(attempt);
    // Small independent jitter for concurrent clients, without logging response data.
    let jitter = if base.is_zero() {
        Duration::ZERO
    } else {
        let mut byte = [0];
        let _ = aws_lc_rs::rand::fill(&mut byte);
        Duration::from_millis(u64::from(byte[0]))
    };
    Some(minimum.max(fallback) + jitter)
}

#[cfg(test)]
#[test]
fn retry_after_is_a_lower_bound_and_over_budget_stops() {
    let base = Duration::from_millis(250);
    assert!(retry_delay(Some(Duration::from_secs(3)), base, 0).unwrap() >= Duration::from_secs(3));
    assert_eq!(retry_delay(Some(Duration::from_secs(61)), base, 0), None);
}
