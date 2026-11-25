use std::{
    error::Error,
    fmt,
    io::Read,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use reqwest::{
    Method, Url,
    blocking::{Client, Request, RequestBuilder},
    header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER},
};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::{
    apply::WriteAccess,
    trial::PRODUCTION_DATA_SOURCES,
    wire::{PreparedWrite, property_id, uuid},
};

pub const NOTION_VERSION: &str = "2026-03-11";
const BASE: &str = "https://api.notion.com";
const ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
const REQUEST_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotionError {
    Configuration,
    Authentication,
    CredentialMismatch,
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
    CourseMappingOutsideTarget,
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

/// The regular API exposes reads. Mutations require a scoped, approved write path.
pub struct NotionClient {
    http: Client,
    base: Url,
    retry_base: Duration,
    interval: Duration,
    pacing: Mutex<Pacing>,
    read_operations: AtomicUsize,
    read_retries: AtomicUsize,
    #[cfg(all(test, target_os = "macos"))]
    pub(super) verification_writes: AtomicUsize,
    #[cfg(all(test, target_os = "macos"))]
    pub(super) lose_verification_ack: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "synthetic-https-fixture"))]
    mock_writes: bool,
}

struct Pacing {
    next: Instant,
    cooldown: Option<(NotionError, Option<Instant>)>,
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
            interval: REQUEST_INTERVAL,
            pacing: Mutex::new(Pacing {
                next: Instant::now(),
                cooldown: None,
            }),
            read_operations: AtomicUsize::new(0),
            read_retries: AtomicUsize::new(0),
            #[cfg(all(test, target_os = "macos"))]
            verification_writes: AtomicUsize::new(0),
            #[cfg(all(test, target_os = "macos"))]
            lose_verification_ack: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "synthetic-https-fixture"))]
            mock_writes: false,
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
        let mut client = Self::build(
            &format!("http://{address}"),
            token,
            false,
            timeout,
            Duration::ZERO,
        )?;
        client.interval = Duration::ZERO;
        client.mock_writes = true;
        Ok(client)
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
        self.read_operations.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..ATTEMPTS {
            if attempt != 0 {
                self.read_retries.fetch_add(1, Ordering::Relaxed);
            }
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

    /// Logical read operations and additional attempts initiated by this client.
    pub fn read_counts(&self) -> (usize, usize) {
        (
            self.read_operations.load(Ordering::Relaxed),
            self.read_retries.load(Ordering::Relaxed),
        )
    }

    #[cfg(any(test, feature = "synthetic-https-fixture"))]
    pub(super) fn require_mock_writes(&self) -> Result<(), NotionError> {
        use std::net::IpAddr;

        let loopback = |url: &Url| {
            url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
                })
        };
        if !self.mock_writes || !loopback(&self.base) {
            return Err(NotionError::MethodDisabled);
        }
        Ok(())
    }

    pub(super) fn send_write(
        &self,
        request: &PreparedWrite,
        access: &WriteAccess<'_>,
    ) -> Result<serde_json::Value, NotionError> {
        match access {
            #[cfg(any(test, feature = "synthetic-https-fixture"))]
            WriteAccess::Mock => self.require_mock_writes()?,
            WriteAccess::CourseSetup(approval) => {
                self.require_course_setup_origin()?;
                if !approval.permits(self, request) {
                    return Err(NotionError::MethodDisabled);
                }
            }
            WriteAccess::VerificationAssignments { writer, properties } => {
                self.require_course_setup_origin()?;
                let allowed = match *request.method() {
                    Method::POST => request.body()["parent"]["type"] == "data_source_id",
                    Method::PATCH => {
                        request
                            .body()
                            .as_object()
                            .is_some_and(|body| body.len() == 1)
                            && request.body()["properties"]
                                .as_object()
                                .is_some_and(|fields| {
                                    !fields.is_empty()
                                        && fields.keys().all(|id| {
                                            id == &properties.title || id == &properties.deadline
                                        })
                                })
                    }
                    _ => false,
                };
                if !std::ptr::eq(self, *writer)
                    || request.target_id != super::trial::MOODLE_VERIFICATION_ASSIGNMENTS
                    || !allowed
                {
                    return Err(NotionError::MethodDisabled);
                }
            }
            WriteAccess::ProductionFirstSync(approval) => {
                self.require_course_setup_origin()?;
                if !approval.permits(self, request) {
                    return Err(NotionError::MethodDisabled);
                }
            }
            WriteAccess::ProductionRecurring(approval) => {
                self.require_course_setup_origin()?;
                if !approval.permits(self, request) {
                    return Err(NotionError::MethodDisabled);
                }
            }
            #[cfg(all(test, target_os = "macos"))]
            WriteAccess::VerificationRecurring(approval, sample) => {
                self.require_course_setup_origin()?;
                if !approval.permits_verification(self, request, sample) {
                    return Err(NotionError::MethodDisabled);
                }
            }
            #[cfg(all(test, target_os = "macos"))]
            WriteAccess::VerificationSeed(seed) => {
                self.require_course_setup_origin()?;
                if !seed.permits(self, request) {
                    return Err(NotionError::MethodDisabled);
                }
            }
            WriteAccess::Trial(approval) => {
                self.require_trial_destination(request, approval.target())?
            }
            #[cfg(any(windows, test, feature = "synthetic-https-fixture"))]
            WriteAccess::MoodleCreate { approval, key } => {
                self.require_trial_destination(request, approval.target())?;
                if request.method() != Method::POST || request.key() != *key {
                    return Err(NotionError::MethodDisabled);
                }
            }
            #[cfg(feature = "synthetic-https-fixture")]
            WriteAccess::MoodleUpdate {
                approval,
                key,
                page_id,
                properties,
            } => {
                self.require_trial_destination(request, approval.target())?;
                let exact_properties = request.body().as_object().is_some_and(|body| {
                    body.len() == 1
                        && body
                            .get("properties")
                            .and_then(serde_json::Value::as_object)
                            .is_some_and(|fields| {
                                fields.len() == properties.len()
                                    && properties.iter().all(|id| fields.contains_key(*id))
                            })
                });
                if request.method() != Method::PATCH
                    || request.key() != *key
                    || request.path().strip_prefix("/v1/pages/") != Some(*page_id)
                    || !exact_properties
                {
                    return Err(NotionError::MethodDisabled);
                }
            }
        }
        if (PRODUCTION_DATA_SOURCES.contains(&request.target_id.as_str())
            && !matches!(
                access,
                WriteAccess::ProductionFirstSync(_) | WriteAccess::ProductionRecurring(_)
            ))
            || (request.method() == Method::POST
                && request.body()["parent"]["data_source_id"] != request.target_id)
        {
            return Err(NotionError::MethodDisabled);
        }
        let request = self
            .request(request.method().clone(), request.path(), &[])
            .json(request.body())
            .build()
            .map_err(|_| NotionError::Configuration)?;
        let url = request.url();
        let valid_path = match *request.method() {
            Method::POST => url.path() == "/v1/pages",
            Method::PATCH => url
                .path()
                .strip_prefix("/v1/pages/")
                .is_some_and(|id| uuid(id).as_deref() == Some(id)),
            _ => false,
        };
        if url.origin() != self.base.origin() || url.query().is_some() || !valid_path {
            return Err(NotionError::MethodDisabled);
        }
        #[cfg(all(test, target_os = "macos"))]
        if matches!(access, WriteAccess::VerificationAssignments { .. }) {
            self.verification_writes.fetch_add(1, Ordering::Relaxed);
            let result = self.execute(request).map_err(|failure| failure.error);
            if result.is_ok() && self.lose_verification_ack.swap(false, Ordering::Relaxed) {
                return Err(NotionError::Timeout);
            }
            return result;
        }
        self.execute(request).map_err(|failure| failure.error)
    }

    fn require_trial_destination(
        &self,
        request: &PreparedWrite,
        target: &super::TrialTarget,
    ) -> Result<(), NotionError> {
        let real_origin = self.base.origin() == Url::parse(BASE).expect("fixed URL").origin();
        #[cfg(test)]
        let real_origin = real_origin || self.require_mock_writes().is_ok();
        if !real_origin || request.target_id != target.assignment_id() {
            return Err(NotionError::MethodDisabled);
        }
        Ok(())
    }

    pub(super) fn require_course_setup_origin(&self) -> Result<(), NotionError> {
        if self.base.origin() == Url::parse(BASE).expect("fixed URL").origin() {
            return Ok(());
        }
        #[cfg(test)]
        if self.require_mock_writes().is_ok() {
            return Ok(());
        }
        Err(NotionError::MethodDisabled)
    }

    #[cfg(feature = "synthetic-https-fixture")]
    pub(super) fn create_verification_course(
        &self,
        title: &str,
    ) -> Result<serde_json::Value, NotionError> {
        use super::{trial::MOODLE_VERIFICATION_COURSES, wire::title_value};
        use serde_json::json;

        let course_source = MOODLE_VERIFICATION_COURSES;
        let real_origin = self.base.origin() == Url::parse(BASE).expect("fixed URL").origin();
        #[cfg(test)]
        let real_origin = real_origin || self.require_mock_writes().is_ok();
        if !real_origin || PRODUCTION_DATA_SOURCES.contains(&course_source) {
            return Err(NotionError::MethodDisabled);
        }
        let request = self
            .request(Method::POST, "/v1/pages", &[])
            .json(&json!({
                "parent": { "type": "data_source_id", "data_source_id": course_source },
                "properties": { "title": title_value(title)? },
            }))
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
        // Serialize dispatch through the response headers so a rate limit can
        // pause subsequent requests, including reconciliation reads.
        let mut pacing = self.pacing.lock().map_err(|_| NotionError::Transport)?;
        let now = Instant::now();
        if let Some((error, until)) = pacing.cooldown {
            let Some(until) =
                until.filter(|until| until.saturating_duration_since(now) <= MAX_RETRY_AFTER)
            else {
                return Err(Failed {
                    error,
                    retry_after: Some(MAX_RETRY_AFTER + Duration::from_secs(1)),
                });
            };
            pacing.next = pacing.next.max(until);
            pacing.cooldown = None;
        }
        thread::sleep(pacing.next.saturating_duration_since(now));
        pacing.next = Instant::now() + self.interval;
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
            if matches!(error, NotionError::RateLimited | NotionError::Overloaded) {
                let delay = retry_after.unwrap_or(self.retry_base);
                pacing.cooldown = Some((error, Instant::now().checked_add(delay)));
            }
            return Err(Failed { error, retry_after });
        }
        drop(pacing);
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
        (&Method::GET, ["", "v1", "users", "me"]) => return true,
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
