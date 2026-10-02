//! Fetch a Moodle calendar as text. Parsing and mapping remain in `crate::moodle`.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// The feed label is public; the URL may contain a token and is deliberately private.
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
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleHttpError {
    InvalidScope,
    InvalidUrl,
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
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .referer(false)
            .build()
            .map_err(|_| MoodleHttpError::ClientInitialization)?;
        Ok(Self { http })
    }

    /// Return the response text without interpreting iCalendar or creating candidates.
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
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    use super::*;
    use crate::import::OriginKind;
    use crate::moodle::{MoodleParseError, map_events, parse_calendar};

    const SCOPE: &str = "cis-moodle-main";
    const BASIC: &str = include_str!("../fixtures/moodle/calendar_basic.ics");
    const SYNTHETIC_TOKEN: &str = "synthetic-test-token";

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

    fn mock_response(status: &'static str, body: Vec<u8>) -> (MoodleFeed, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            assert!(request.starts_with("GET /calendar/export?authtoken="));
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let url = format!("http://{address}/calendar/export?authtoken={SYNTHETIC_TOKEN}");
        (MoodleFeed::new(SCOPE, &url).unwrap(), server)
    }

    #[test]
    fn fetched_ics_is_unchanged_and_uses_existing_parser_and_mapper() {
        let (feed, server) = mock_response("200 OK", BASIC.as_bytes().to_vec());
        let raw = MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap();
        server.join().unwrap();
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
        server.join().unwrap();
        assert_eq!(error, MoodleHttpError::HttpStatus(StatusCode::FORBIDDEN));
        assert!(!format!("{error:?} {error}").contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn malformed_ics_is_a_parse_error_after_successful_fetch() {
        let (feed, server) = mock_response("200 OK", b"not a calendar".to_vec());
        let raw = MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap();
        server.join().unwrap();
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
        server.join().unwrap();
        assert_eq!(error, MoodleHttpError::BodyDecode);
    }

    #[test]
    fn follows_redirect_without_sending_secret_url_as_referer() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target.local_addr().unwrap();
        let target_server = thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            let request = read_request_headers(&mut stream).to_ascii_lowercase();
            assert!(!request.contains("referer:"));
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BASIC.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(BASIC.as_bytes()).unwrap();
        });

        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_address = redirect.local_addr().unwrap();
        let redirect_server = thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            read_request_headers(&mut stream);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/calendar\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let url = format!("http://{redirect_address}/calendar/export?authtoken={SYNTHETIC_TOKEN}");
        let feed = MoodleFeed::new(SCOPE, &url).unwrap();
        let raw = MoodleClient::new().unwrap().fetch_calendar(&feed).unwrap();
        redirect_server.join().unwrap();
        target_server.join().unwrap();
        assert_eq!(raw, BASIC);
    }

    #[test]
    fn feed_debug_and_configuration_errors_hide_secret_url() {
        let url = format!("http://127.0.0.1/calendar?authtoken={SYNTHETIC_TOKEN}");
        let feed = MoodleFeed::new(SCOPE, &url).unwrap();
        assert!(!format!("{feed:?}").contains(SYNTHETIC_TOKEN));
        assert!(matches!(
            MoodleFeed::new(&url, &url),
            Err(MoodleHttpError::InvalidScope)
        ));
        let error = MoodleFeed::new(SCOPE, "invalid?authtoken=synthetic-test-token").unwrap_err();
        assert_eq!(error, MoodleHttpError::InvalidUrl);
        assert!(!format!("{error:?} {error}").contains(SYNTHETIC_TOKEN));
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
        let url = format!("http://{address}/calendar/export?authtoken={SYNTHETIC_TOKEN}");
        let feed = MoodleFeed::new(SCOPE, &url).unwrap();
        let client = MoodleClient::with_timeout(Duration::from_millis(50)).unwrap();
        let error = client.fetch_calendar(&feed).unwrap_err();
        server.join().unwrap();
        assert_eq!(error, MoodleHttpError::Timeout);
    }
}
