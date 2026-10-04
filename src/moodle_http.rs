//! Fetch a Moodle calendar as text. Parsing and mapping remain in `crate::moodle`.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// The feed label is public; the URL is a bearer secret and is deliberately private.
/// Production feeds require HTTPS and forbid URL userinfo.
pub struct MoodleFeed {
    scope: String,
    url: Url,
}

impl MoodleFeed {
    pub fn new(scope: impl Into<String>, secret_url: &str) -> Result<Self, MoodleHttpError> {
        let scope = scope.into();
        if scope.is_empty()
            || !scope
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(MoodleHttpError::InvalidScope);
        }

        let url = Url::parse(secret_url).map_err(|_| MoodleHttpError::InvalidUrl)?;
        if url.scheme() != "https" {
            return Err(MoodleHttpError::InsecureScheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(MoodleHttpError::UserInfoNotAllowed);
        }
        if url.host().is_none() {
            return Err(MoodleHttpError::InvalidUrl);
        }
        Ok(Self { scope, url })
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }
}

impl fmt::Debug for MoodleFeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoodleFeed")
            .field("scope", &self.scope)
            .field("url", &"[redacted]")
            .finish()
    }
}

/// Fixed classifications only; no URL, Location, response body or transport cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleHttpError {
    InvalidScope,
    InvalidUrl,
    InsecureScheme,
    UserInfoNotAllowed,
    InvalidTimeout,
    ClientInitialization,
    Timeout,
    Request,
    HttpStatus(StatusCode),
    BodyRead,
    BodyDecode,
}

impl fmt::Display for MoodleHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope => write!(f, "invalid Moodle feed scope"),
            Self::InvalidUrl => write!(f, "invalid Moodle calendar URL"),
            Self::InsecureScheme => write!(f, "Moodle calendar URL must use HTTPS"),
            Self::UserInfoNotAllowed => write!(f, "Moodle calendar URL must not contain userinfo"),
            Self::InvalidTimeout => write!(f, "invalid Moodle request timeout"),
            Self::ClientInitialization => write!(f, "could not initialize Moodle HTTP client"),
            Self::Timeout => write!(f, "Moodle calendar request timed out"),
            Self::Request => write!(f, "Moodle calendar request failed"),
            Self::HttpStatus(status) => write!(f, "Moodle calendar returned HTTP {status}"),
            Self::BodyRead => write!(f, "could not read Moodle calendar response"),
            Self::BodyDecode => write!(f, "Moodle calendar response is not UTF-8"),
        }
    }
}

impl Error for MoodleHttpError {}

/// Never follows redirects, including same-origin redirects, or sends an automatic Referer.
pub struct MoodleClient {
    http: Client,
}

impl MoodleClient {
    pub fn new() -> Result<Self, MoodleHttpError> {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Result<Self, MoodleHttpError> {
        if timeout.is_zero() {
            return Err(MoodleHttpError::InvalidTimeout);
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .referer(false)
            .build()
            .map_err(|_| MoodleHttpError::ClientInitialization)?;
        Ok(Self { http })
    }

    /// Return the response text without interpreting iCalendar or creating candidates.
    /// Redirects return `HttpStatus` without reading or requesting their Location.
    pub fn fetch_calendar(&self, feed: &MoodleFeed) -> Result<String, MoodleHttpError> {
        let response = self.http.get(feed.url.clone()).send().map_err(|error| {
            if error.is_timeout() {
                MoodleHttpError::Timeout
            } else {
                MoodleHttpError::Request
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(MoodleHttpError::HttpStatus(status));
        }
        let body = response.bytes().map_err(|error| {
            if error.is_timeout() {
                MoodleHttpError::Timeout
            } else {
                MoodleHttpError::BodyRead
            }
        })?;
        String::from_utf8(body.to_vec()).map_err(|_| MoodleHttpError::BodyDecode)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    use super::*;
    use crate::import::OriginKind;
    use crate::moodle::{MoodleParseError, map_events, parse_calendar};

    const SCOPE: &str = "cis-moodle-main";
    const BASIC: &str = include_str!("../fixtures/moodle/calendar_basic.ics");
    const SYNTHETIC_TOKEN: &str = "VERY_PRIVATE_MARKER_5_5B";
    const SYNTHETIC_LOCATION: &str = "VERY_PRIVATE_LOCATION_5_5B";
    type MockServer = JoinHandle<(String, TcpListener)>;

    fn assert_secret_free(error: &dyn Error) {
        for marker in [SYNTHETIC_TOKEN, SYNTHETIC_LOCATION] {
            assert!(!format!("{error}").contains(marker));
            assert!(!format!("{error:?}").contains(marker));
        }
        assert!(error.source().is_none());
    }

    fn local_feed(address: SocketAddr) -> MoodleFeed {
        assert!(address.ip().is_loopback());
        let mut feed = MoodleFeed::new(
            SCOPE,
            &format!("https://example.invalid/calendar/export?authtoken={SYNTHETIC_TOKEN}"),
        )
        .unwrap();
        // Test-only access to the private URL; production APIs never accept HTTP.
        feed.url = Url::parse(&format!(
            "http://{address}/calendar/export?authtoken={SYNTHETIC_TOKEN}"
        ))
        .unwrap();
        feed
    }

    fn finish(server: MockServer) -> TcpListener {
        let (request, listener) = server.join().unwrap();
        assert!(request.starts_with("GET /calendar/export?authtoken="));
        assert!(!request.to_ascii_lowercase().contains("referer:"));
        listener
    }

    fn assert_no_request(listener: &TcpListener) {
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    fn read_request_headers(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0 && request.len() + read <= 8192);
            request.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(request).unwrap()
    }

    fn mock_response(status: &'static str, body: Vec<u8>) -> (MoodleFeed, MockServer) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        mock_response_at(listener, status, "", body)
    }

    fn mock_response_at(
        listener: TcpListener,
        status: &'static str,
        extra_headers: &str,
        body: Vec<u8>,
    ) -> (MoodleFeed, MockServer) {
        let address = listener.local_addr().unwrap();
        let extra_headers = extra_headers.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            let headers = format!(
                "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            // Keep the port bound so follow-up requests cannot race with port reuse.
            (request, listener)
        });
        (local_feed(address), server)
    }

    #[test]
    fn fetched_ics_is_unchanged_and_uses_existing_parser_and_mapper() {
        let (feed, server) = mock_response("200 OK", BASIC.as_bytes().to_vec());
        let raw = MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap();
        finish(server);
        assert_eq!(raw, BASIC);

        let events = parse_calendar(&raw).unwrap();
        let candidates = map_events(&events, feed.scope());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "演習課題01");
        assert_eq!(candidates[0].external_key.origin, OriginKind::Moodle);
        assert_eq!(candidates[0].external_key.scope, SCOPE);
        assert_eq!(candidates[0].external_key.id, "1001@cms.example.edu");
        assert!(!format!("{candidates:?}").contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn non_success_status_is_an_http_error() {
        let (feed, server) = mock_response("403 Forbidden", b"Forbidden".to_vec());
        let error = MoodleClient::new()
            .unwrap()
            .fetch_calendar(&feed)
            .unwrap_err();
        finish(server);
        assert_eq!(error, MoodleHttpError::HttpStatus(StatusCode::FORBIDDEN));
        assert_secret_free(&error);
    }

    #[test]
    fn malformed_ics_is_a_parse_error_after_successful_fetch() {
        let (feed, server) = mock_response("200 OK", b"not a calendar".to_vec());
        let raw = MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap();
        finish(server);
        assert!(matches!(
            parse_calendar(&raw),
            Err(MoodleParseError::InvalidIcs(_))
        ));
    }

    #[test]
    fn invalid_utf8_is_a_body_decode_error() {
        let (feed, server) = mock_response("200 OK", vec![0xff]);
        let error = MoodleClient::new()
            .unwrap()
            .fetch_calendar(&feed)
            .unwrap_err();
        finish(server);
        assert_eq!(error, MoodleHttpError::BodyDecode);
        assert_secret_free(&error);
    }

    #[test]
    fn cross_origin_redirect_is_not_followed_and_location_is_not_retained() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let location = format!("http://{target_address}/calendar?token={SYNTHETIC_LOCATION}");
        let (feed, server) = mock_response_at(
            listener,
            "302 Found",
            &format!("Location: {location}\r\n"),
            SYNTHETIC_TOKEN.as_bytes().to_vec(),
        );
        let error = MoodleClient::with_timeout(Duration::from_millis(200))
            .unwrap()
            .fetch_calendar(&feed)
            .unwrap_err();
        let origin = finish(server);
        assert_eq!(error, MoodleHttpError::HttpStatus(StatusCode::FOUND));
        assert_secret_free(&error);
        assert!(!format!("{error} {error:?}").contains(&location));
        assert_no_request(&origin);
        assert_no_request(&target);
    }

    #[test]
    fn same_origin_redirects_and_loops_stop_after_the_first_response() {
        for (status, code) in [
            ("301 Moved Permanently", StatusCode::MOVED_PERMANENTLY),
            ("302 Found", StatusCode::FOUND),
            ("303 See Other", StatusCode::SEE_OTHER),
            ("307 Temporary Redirect", StatusCode::TEMPORARY_REDIRECT),
            ("308 Permanent Redirect", StatusCode::PERMANENT_REDIRECT),
        ] {
            for is_loop in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let address = listener.local_addr().unwrap();
                let location = if is_loop {
                    local_feed(address).url.to_string()
                } else {
                    format!("http://{address}/redirect-target?token={SYNTHETIC_LOCATION}")
                };
                let (feed, server) = mock_response_at(
                    listener,
                    status,
                    &format!("Location: {location}\r\n"),
                    Vec::new(),
                );
                let error = MoodleClient::with_timeout(Duration::from_millis(200))
                    .unwrap()
                    .fetch_calendar(&feed)
                    .unwrap_err();
                let origin = finish(server);
                assert_eq!(error, MoodleHttpError::HttpStatus(code));
                assert_secret_free(&error);
                assert_no_request(&origin);
            }
        }
    }

    #[test]
    fn secret_feed_url_is_not_sent_as_referer() {
        let (feed, server) = mock_response("200 OK", BASIC.as_bytes().to_vec());
        assert_eq!(
            MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap(),
            BASIC
        );
        // Inspects the received headers, also for every redirect mock above.
        finish(server);
    }

    #[test]
    fn production_feed_requires_https() {
        let url = format!("https://example.invalid/calendar?authtoken={SYNTHETIC_TOKEN}");
        assert!(MoodleFeed::new(SCOPE, &url).is_ok());
        for scheme in ["http", "ftp", "ws", "file"] {
            let url = format!("{scheme}://example.invalid/calendar?authtoken={SYNTHETIC_TOKEN}");
            let error = MoodleFeed::new(SCOPE, &url).unwrap_err();
            assert_eq!(error, MoodleHttpError::InsecureScheme);
            assert_secret_free(&error);
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let feed = local_feed(listener.local_addr().unwrap());
        let error = MoodleFeed::new(SCOPE, feed.url.as_str()).unwrap_err();
        assert_eq!(error, MoodleHttpError::InsecureScheme);
        assert_secret_free(&error);
        assert_no_request(&listener);
    }

    #[test]
    fn production_feed_rejects_userinfo_without_retaining_it() {
        for userinfo in [
            SYNTHETIC_TOKEN.to_owned(),
            format!("{SYNTHETIC_TOKEN}:{SYNTHETIC_TOKEN}"),
            format!(":{SYNTHETIC_TOKEN}"),
            format!("{SYNTHETIC_TOKEN}:"),
        ] {
            let url =
                format!("https://{userinfo}@example.invalid/calendar?authtoken={SYNTHETIC_TOKEN}");
            let error = MoodleFeed::new(SCOPE, &url).unwrap_err();
            assert_eq!(error, MoodleHttpError::UserInfoNotAllowed);
            assert_secret_free(&error);
        }
    }

    #[test]
    fn feed_debug_and_configuration_errors_hide_secret_url() {
        let url = format!("https://example.invalid/calendar?authtoken={SYNTHETIC_TOKEN}");
        let feed = MoodleFeed::new(SCOPE, &url).unwrap();
        assert!(!format!("{feed:?}").contains(SYNTHETIC_TOKEN));
        let error = MoodleFeed::new(&url, &url).unwrap_err();
        assert_eq!(error, MoodleHttpError::InvalidScope);
        assert_secret_free(&error);
        let error =
            MoodleFeed::new(SCOPE, &format!("invalid?authtoken={SYNTHETIC_TOKEN}")).unwrap_err();
        assert_eq!(error, MoodleHttpError::InvalidUrl);
        assert_secret_free(&error);
    }

    #[test]
    fn timeout_is_reported_separately() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&mut stream);
            thread::sleep(Duration::from_millis(200));
        });
        let feed = local_feed(address);
        let client = MoodleClient::with_timeout(Duration::from_millis(50)).unwrap();
        let error = client.fetch_calendar(&feed).unwrap_err();
        server.join().unwrap();
        assert_eq!(error, MoodleHttpError::Timeout);
        assert_secret_free(&error);
    }

    #[test]
    fn every_http_error_has_secret_free_display_debug_and_source() {
        for error in [
            MoodleHttpError::InvalidScope,
            MoodleHttpError::InvalidUrl,
            MoodleHttpError::InsecureScheme,
            MoodleHttpError::UserInfoNotAllowed,
            MoodleHttpError::InvalidTimeout,
            MoodleHttpError::ClientInitialization,
            MoodleHttpError::Timeout,
            MoodleHttpError::Request,
            MoodleHttpError::HttpStatus(StatusCode::FOUND),
            MoodleHttpError::BodyRead,
            MoodleHttpError::BodyDecode,
        ] {
            assert_secret_free(&error);
        }
    }
}
