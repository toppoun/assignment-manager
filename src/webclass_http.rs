//! Fetch WebClass API responses as text. Parsing and mapping remain in `crate::webclass`.

use std::error::Error;
use std::fmt;

use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, REFERER, USER_AGENT};
use reqwest::{StatusCode, Url};

const ORIGIN: &str = "https://lms2025.hosei.ac.jp";
const COURSES_PATH: &str = "/webclass/ip_mods.php/plugin/score_summary_table/courses";
const CONTENTS_PATH: &str = "/webclass/ip_mods.php/plugin/score_summary_table/contents";
const DASHBOARD_PATH: &str = "/webclass/ip_mods.php/plugin/score_summary_table/dashboard";
const LOGIN_PATH: &str = "/webclass/login.php";
const APPLICATION_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Errors contain no request URL, headers, response body, or underlying transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebClassHttpError {
    AuthExpired,
    Timeout,
    Request,
    HttpStatus(StatusCode),
    BodyRead,
    BodyDecode,
    UnexpectedResponse,
}

impl fmt::Display for WebClassHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthExpired => write!(f, "WebClass authentication expired"),
            Self::Timeout => write!(f, "WebClass API request timed out"),
            Self::Request => write!(f, "WebClass API request failed"),
            Self::HttpStatus(status) => write!(f, "WebClass API returned HTTP {status}"),
            Self::BodyRead => write!(f, "could not read WebClass API response"),
            Self::BodyDecode => write!(f, "WebClass API response is not UTF-8"),
            Self::UnexpectedResponse => write!(f, "unexpected WebClass API response"),
        }
    }
}

impl Error for WebClassHttpError {}

/// Uses an existing authenticated HTTP client without logging in or inspecting cookies.
pub struct WebClassClient {
    http: Client,
    base_url: Url,
}

impl WebClassClient {
    /// Accept the authenticated client supplied by the authentication module.
    ///
    /// The caller must build it with `reqwest::redirect::Policy::none()`,
    /// `reqwest::retry::never()`, and a finite timeout (for example, 15 seconds).
    /// reqwest cannot inspect or change these policies on an already built client.
    /// This constructor trusts that crate-internal contract; it creates no HTTP client.
    pub(crate) fn from_http(http: Client) -> Self {
        Self {
            http,
            base_url: Url::parse(ORIGIN).expect("the fixed WebClass origin is a valid URL"),
        }
    }

    /// Internal offline test seam; production callers always use the fixed origin.
    #[cfg(test)]
    pub(crate) fn set_test_origin(&mut self, origin: Url) {
        assert!(matches!(origin.host_str(), Some("127.0.0.1" | "localhost")));
        self.base_url = origin;
    }

    /// Return raw JSON text; JSON validity and course interpretation belong to the parser.
    pub fn fetch_courses_json(&self) -> Result<String, WebClassHttpError> {
        self.fetch_json(self.endpoint(COURSES_PATH))
    }

    /// Return raw JSON text with one safely encoded `group_id` query parameter.
    ///
    /// Empty IDs are passed through, consistent with the existing parser's string IDs.
    pub fn fetch_contents_json(&self, group_id: &str) -> Result<String, WebClassHttpError> {
        let mut url = self.endpoint(CONTENTS_PATH);
        url.query_pairs_mut().append_pair("group_id", group_id);
        self.fetch_json(url)
    }

    fn endpoint(&self, path: &str) -> Url {
        self.base_url
            .join(path)
            .expect("fixed WebClass endpoint paths are valid URLs")
    }

    fn fetch_json(&self, url: Url) -> Result<String, WebClassHttpError> {
        let response = self
            .http
            .get(url)
            .header(ACCEPT, "application/json")
            .header(REFERER, self.endpoint(DASHBOARD_PATH).as_str())
            .header(USER_AGENT, APPLICATION_USER_AGENT)
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    WebClassHttpError::Timeout
                } else {
                    WebClassHttpError::Request
                }
            })?;

        let status = response.status();
        if status.is_redirection()
            && response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| response.url().join(location).ok())
                .is_some_and(|target| {
                    target.origin() == self.base_url.origin() && target.path() == LOGIN_PATH
                })
        {
            return Err(WebClassHttpError::AuthExpired);
        }
        if !status.is_success() {
            return Err(WebClassHttpError::HttpStatus(status));
        }
        if status != StatusCode::OK {
            return Err(WebClassHttpError::UnexpectedResponse);
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or("");
        let is_json = content_type.eq_ignore_ascii_case("application/json");
        let is_html = content_type.eq_ignore_ascii_case("text/html");
        if !is_json && !is_html {
            return Err(WebClassHttpError::UnexpectedResponse);
        }

        let body = response.bytes().map_err(|error| {
            if error.is_timeout() {
                WebClassHttpError::Timeout
            } else {
                WebClassHttpError::BodyRead
            }
        })?;
        let text = String::from_utf8(body.to_vec()).map_err(|_| WebClassHttpError::BodyDecode)?;

        if is_json {
            Ok(text)
        } else if text.contains(LOGIN_PATH) {
            // Only the confirmed literal HTML pattern is recognized in Phase 5A.
            Err(WebClassHttpError::AuthExpired)
        } else {
            Err(WebClassHttpError::UnexpectedResponse)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use reqwest::header::{COOKIE, HeaderMap, HeaderValue};

    use super::*;
    use crate::webclass::{parse_contents, parse_courses};

    const COURSES: &str = include_str!("../fixtures/webclass/courses_basic.json");
    const CONTENTS: &str = include_str!("../fixtures/webclass/contents_basic.json");
    const SYNTHETIC_COOKIE: &str = "synthetic-session-value";
    const SYNTHETIC_TOKEN: &str = "synthetic-query-token";
    const SYNTHETIC_BODY: &str = "synthetic-private-response";
    type MockServer = JoinHandle<(String, TcpListener)>;

    fn test_http(timeout: Duration) -> Client {
        let mut headers = HeaderMap::new();
        let mut cookie =
            HeaderValue::from_str(&format!("test_session={SYNTHETIC_COOKIE}")).unwrap();
        cookie.set_sensitive(true);
        headers.insert(COOKIE, cookie);
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .no_proxy()
            .default_headers(headers)
            .build()
            .unwrap()
    }

    fn local_client(http: Client, address: std::net::SocketAddr) -> WebClassClient {
        assert!(address.ip().is_loopback());
        let mut client = WebClassClient::from_http(http);
        // Test-only access to the private field keeps origin substitution out of the API.
        client.base_url = Url::parse(&format!("http://{address}")).unwrap();
        client
    }

    fn read_request_headers(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
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

    fn mock_response(status: &str, headers: &str, body: &[u8]) -> (WebClassClient, MockServer) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = mock_response_at(listener, status, headers, body);
        (
            local_client(test_http(Duration::from_secs(2)), address),
            server,
        )
    }

    fn mock_response_at(
        listener: TcpListener,
        status: &str,
        headers: &str,
        body: &[u8],
    ) -> MockServer {
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let body = body.to_vec();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            (request, listener)
        })
    }

    fn finish(server: MockServer) -> String {
        let (request, listener) = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        // No second connection is allowed, including an automatically followed redirect.
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        request
    }

    fn assert_secret_free(error: WebClassHttpError) {
        let rendered = format!("{error:?} {error}");
        for secret in [
            SYNTHETIC_COOKIE,
            SYNTHETIC_TOKEN,
            SYNTHETIC_BODY,
            "group_id=",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(error.source().is_none());
    }

    #[test]
    fn production_constructor_sets_fixed_endpoints_without_sending_requests() {
        let client = WebClassClient::from_http(test_http(Duration::from_secs(15)));
        assert_eq!(client.base_url.as_str(), "https://lms2025.hosei.ac.jp/");
        assert_eq!(client.endpoint(COURSES_PATH).path(), COURSES_PATH);
        assert_eq!(client.endpoint(CONTENTS_PATH).path(), CONTENTS_PATH);
        assert_eq!(client.endpoint(DASHBOARD_PATH).path(), DASHBOARD_PATH);
    }

    #[test]
    fn courses_are_returned_unchanged_and_use_existing_parser() {
        let (client, server) = mock_response(
            "200 OK",
            "Content-Type: application/json; charset=UTF-8\r\n",
            COURSES.as_bytes(),
        );
        let raw = client.fetch_courses_json().unwrap();
        let request = finish(server);
        assert!(request.starts_with(&format!("GET {COURSES_PATH} HTTP/1.1\r\n")));
        assert_eq!(raw, COURSES);
        let courses = parse_courses(&raw).unwrap();
        assert_eq!(courses.len(), 3);
        assert_eq!(courses[0].group_id, "fixture-course-001");
    }

    #[test]
    fn contents_encode_group_id_and_use_existing_parser() {
        let group_id = "course &role=admin+日本/%?#";
        let (client, server) = mock_response(
            "200 OK",
            "Content-Type: application/json\r\n",
            CONTENTS.as_bytes(),
        );
        let raw = client.fetch_contents_json(group_id).unwrap();
        let request = finish(server);
        let target = request.split_whitespace().nth(1).unwrap();
        assert!(target.starts_with(&format!("{CONTENTS_PATH}?group_id=")));
        assert!(!target.contains('#'));
        let url = client.base_url.join(target).unwrap();
        assert_eq!(url.path(), CONTENTS_PATH);
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("group_id".into(), group_id.into())]
        );
        assert_eq!(raw, CONTENTS);
        let contents = parse_contents(&raw).unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0].contents_id, "fixture-content-001");
    }

    #[test]
    fn empty_group_id_is_passed_through() {
        let (client, server) = mock_response("200 OK", "Content-Type: application/json\r\n", b"[]");
        assert_eq!(client.fetch_contents_json("").unwrap(), "[]");
        assert!(finish(server).starts_with(&format!("GET {CONTENTS_PATH}?group_id= HTTP/1.1\r\n")));
    }

    #[test]
    fn both_endpoints_send_required_headers_and_preserve_supplied_client() {
        for contents in [false, true] {
            let (client, server) =
                mock_response("200 OK", "Content-Type: application/json\r\n", b"[]");
            let referer = client.endpoint(DASHBOARD_PATH);
            if contents {
                client.fetch_contents_json("fixture-course-002").unwrap();
            } else {
                client.fetch_courses_json().unwrap();
            }
            let request = finish(server).to_ascii_lowercase();
            assert!(request.contains("\r\naccept: application/json\r\n"));
            assert!(request.contains(&format!("\r\nreferer: {referer}\r\n")));
            assert!(request.contains(&format!("\r\nuser-agent: {APPLICATION_USER_AGENT}\r\n")));
            assert!(request.contains(&format!("\r\ncookie: test_session={SYNTHETIC_COOKIE}\r\n")));
        }
    }

    #[test]
    fn http_failure_takes_precedence_over_login_html() {
        let (client, server) = mock_response(
            "500 Internal Server Error",
            "Content-Type: text/html\r\n",
            format!("{LOGIN_PATH} {SYNTHETIC_BODY}").as_bytes(),
        );
        let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
        finish(server);
        assert_eq!(
            error,
            WebClassHttpError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert_secret_free(error);
    }

    #[test]
    fn login_redirect_expires_auth_without_being_followed() {
        for status in [
            "301 Moved Permanently",
            "302 Found",
            "303 See Other",
            "307 Temporary Redirect",
            "308 Permanent Redirect",
        ] {
            for location in [
                "/webclass/login.php",
                "../../../login.php?token=synthetic-query-token",
            ] {
                let (client, server) =
                    mock_response(status, &format!("Location: {location}\r\n"), b"");
                let error = client.fetch_courses_json().unwrap_err();
                finish(server);
                assert_eq!(error, WebClassHttpError::AuthExpired);
                assert_secret_free(error);
            }
        }
    }

    #[test]
    fn absolute_same_origin_login_redirect_expires_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{address}{LOGIN_PATH}?token={SYNTHETIC_TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            (request, listener)
        });
        let client = local_client(test_http(Duration::from_secs(2)), address);
        let error = client.fetch_courses_json().unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::AuthExpired);
        assert_secret_free(error);
    }

    #[test]
    fn unknown_redirects_are_http_errors_and_are_not_followed() {
        for headers in [
            "Location: /maintenance\r\n",
            "Location: /webclass/login.php.other\r\n",
            "Location: http://127.0.0.1:1/webclass/login.php\r\n",
            "",
        ] {
            let (client, server) = mock_response("302 Found", headers, SYNTHETIC_BODY.as_bytes());
            let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
            finish(server);
            assert_eq!(error, WebClassHttpError::HttpStatus(StatusCode::FOUND));
            assert_secret_free(error);
        }
    }

    #[test]
    fn confirmed_login_html_expires_auth() {
        let html = format!(
            "<body><script>window.top.location.href=\"{LOGIN_PATH}\";</script>{SYNTHETIC_BODY}</body>"
        );
        let (client, server) = mock_response(
            "200 OK",
            "Content-Type: text/html; charset=UTF-8\r\n",
            html.as_bytes(),
        );
        let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::AuthExpired);
        assert_secret_free(error);
    }

    #[test]
    fn unknown_html_is_unexpected_including_unhandled_js_escaping() {
        for html in [
            "<html>maintenance</html>",
            r#"<script>window.top.location.href="\/webclass\/login.php";</script>"#,
        ] {
            let (client, server) =
                mock_response("200 OK", "Content-Type: text/html\r\n", html.as_bytes());
            let error = client.fetch_courses_json().unwrap_err();
            finish(server);
            assert_eq!(error, WebClassHttpError::UnexpectedResponse);
            assert_secret_free(error);
        }
    }

    #[test]
    fn missing_or_unexpected_content_type_is_unexpected_even_with_login_text() {
        for headers in [
            "",
            "Content-Type: text/plain\r\n",
            "Content-Type: application/json-other\r\n",
        ] {
            let (client, server) = mock_response("200 OK", headers, LOGIN_PATH.as_bytes());
            let error = client.fetch_courses_json().unwrap_err();
            finish(server);
            assert_eq!(error, WebClassHttpError::UnexpectedResponse);
        }
    }

    #[test]
    fn other_success_status_is_unexpected() {
        let (client, server) =
            mock_response("204 No Content", "Content-Type: application/json\r\n", b"");
        let error = client.fetch_courses_json().unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::UnexpectedResponse);
    }

    #[test]
    fn malformed_json_is_successful_http_but_fails_both_parsers() {
        for contents in [false, true] {
            let (client, server) =
                mock_response("200 OK", "Content-Type: application/json\r\n", b"not-json");
            let raw = if contents {
                client.fetch_contents_json("fixture-course-002").unwrap()
            } else {
                client.fetch_courses_json().unwrap()
            };
            finish(server);
            assert_eq!(raw, "not-json");
            assert!(parse_courses(&raw).is_err());
            assert!(parse_contents(&raw).is_err());
        }
    }

    #[test]
    fn json_content_type_does_not_interpret_login_text() {
        let (client, server) = mock_response(
            "200 OK",
            "Content-Type: Application/JSON; charset=UTF-8\r\n",
            LOGIN_PATH.as_bytes(),
        );
        assert_eq!(client.fetch_courses_json().unwrap(), LOGIN_PATH);
        finish(server);
    }

    #[test]
    fn invalid_utf8_is_body_decode_error() {
        let (client, server) =
            mock_response("200 OK", "Content-Type: application/json\r\n", &[0xff]);
        let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::BodyDecode);
        assert_secret_free(error);
    }

    #[test]
    fn truncated_body_is_body_read_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort").unwrap();
            (request, listener)
        });
        let client = local_client(test_http(Duration::from_secs(2)), address);
        let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::BodyRead);
        assert_secret_free(error);
    }

    #[test]
    fn request_and_body_timeouts_are_reported_separately_from_network_errors() {
        for send_headers in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request_headers(&mut stream);
                if send_headers {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n").unwrap();
                }
                thread::sleep(Duration::from_millis(300));
                (request, listener)
            });
            let client = local_client(test_http(Duration::from_millis(100)), address);
            let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
            finish(server);
            assert_eq!(error, WebClassHttpError::Timeout);
            assert_secret_free(error);
        }
    }

    #[test]
    fn network_failure_is_request_error_without_url_or_cookie() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Keep the listener bound to avoid a port reuse race, but close without a response.
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            (request, listener)
        });
        let client = local_client(test_http(Duration::from_secs(2)), address);
        let error = client.fetch_contents_json(SYNTHETIC_TOKEN).unwrap_err();
        finish(server);
        assert_eq!(error, WebClassHttpError::Request);
        assert_secret_free(error);
    }

    #[test]
    fn every_error_variant_has_secret_free_debug_display_and_source() {
        for error in [
            WebClassHttpError::AuthExpired,
            WebClassHttpError::Timeout,
            WebClassHttpError::Request,
            WebClassHttpError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR),
            WebClassHttpError::BodyRead,
            WebClassHttpError::BodyDecode,
            WebClassHttpError::UnexpectedResponse,
        ] {
            assert_secret_free(error);
        }
    }

    // Exercise application sync through the existing private authenticated mock
    // client and HTTP helpers, without changing production constructors/policies.
    mod sync_tests {
        use std::fs;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        use chrono::{DateTime, Utc};
        use rusqlite::{Connection, params, types::Value};
        use serde_json::{Value as JsonValue, json};

        use super::*;
        use crate::domain::{
            Assignment, AssignmentId, CompletionState, Deadline, SubmissionTarget,
        };
        use crate::import::{ExternalRecordKey, OriginKind};
        use crate::persistence::{AssignmentImportSummary, SqliteStore};
        use crate::sync::{WebClassSyncError, sync_webclass_group_once};

        const GROUP: &str = "fixture-course-002";
        const PRIVATE: &str = "SYNTHETIC_PRIVATE_SYNC_VALUE_7B1";
        const JSON_HEADERS: &str = "Content-Type: application/json\r\n";
        const EMPTY: &str = include_str!("../fixtures/webclass/contents_empty.json");

        struct TestDatabase {
            directory: PathBuf,
        }

        impl TestDatabase {
            fn new() -> Self {
                static SEQUENCE: AtomicU64 = AtomicU64::new(0);
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let directory = std::env::temp_dir().join(format!(
                    "assignment-manager-webclass-sync-{}-{nonce}-{sequence}",
                    std::process::id()
                ));
                fs::create_dir(&directory).unwrap();
                Self { directory }
            }

            fn path(&self) -> PathBuf {
                self.directory.join("test.sqlite3")
            }

            fn open(&self) -> (SqliteStore, Connection) {
                let store = SqliteStore::open(self.path()).unwrap();
                let observer = Connection::open(self.path()).unwrap();
                (store, observer)
            }
        }

        impl Drop for TestDatabase {
            fn drop(&mut self) {
                for name in [
                    "test.sqlite3",
                    "test.sqlite3-journal",
                    "test.sqlite3-wal",
                    "test.sqlite3-shm",
                ] {
                    let _ = fs::remove_file(self.directory.join(name));
                }
                let _ = fs::remove_dir(&self.directory);
            }
        }

        struct SyncMock {
            client: WebClassClient,
            listener: TcpListener,
        }

        impl SyncMock {
            fn new() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let client = local_client(
                    test_http(Duration::from_secs(2)),
                    listener.local_addr().unwrap(),
                );
                Self { client, listener }
            }

            fn assert_request(&self, request: &str, group: &str) {
                assert!(request.starts_with("GET "));
                let target = request.split_whitespace().nth(1).unwrap();
                let url = self.client.base_url.join(target).unwrap();
                assert_eq!(url.path(), CONTENTS_PATH);
                assert_eq!(
                    url.query_pairs().collect::<Vec<_>>(),
                    vec![("group_id".into(), group.into())]
                );
                let headers = request.to_ascii_lowercase();
                assert!(headers.contains("\r\naccept: application/json\r\n"));
                assert!(
                    headers.contains(&format!("\r\ncookie: test_session={SYNTHETIC_COOKIE}\r\n"))
                );
            }

            fn sync(
                &self,
                store: &mut SqliteStore,
                group: &str,
                status: &str,
                headers: &str,
                body: &str,
            ) -> Result<AssignmentImportSummary, WebClassSyncError> {
                let server = mock_response_at(
                    self.listener.try_clone().unwrap(),
                    status,
                    headers,
                    body.as_bytes(),
                );
                let result = sync_webclass_group_once(&self.client, group, store);
                // Also rejects a redirect follow-up, retry or course-list request.
                let request = finish(server);
                self.listener.set_nonblocking(false).unwrap();
                self.assert_request(&request, group);
                result
            }

            fn sync_json(
                &self,
                store: &mut SqliteStore,
                group: &str,
                body: &str,
            ) -> Result<AssignmentImportSummary, WebClassSyncError> {
                self.sync(store, group, "200 OK", JSON_HEADERS, body)
            }
        }

        fn records() -> Vec<JsonValue> {
            serde_json::from_str(CONTENTS).unwrap()
        }

        fn key(group: &str, id: &str) -> ExternalRecordKey {
            ExternalRecordKey {
                origin: OriginKind::WebClass,
                scope: group.into(),
                id: id.into(),
            }
        }

        fn stored(store: &SqliteStore, group: &str, id: &str) -> Assignment {
            store
                .find_assignment_by_external_key(&key(group, id))
                .unwrap()
                .unwrap()
        }

        // Includes all rows, bindings, metadata and courses, not just domain fields.
        fn snapshot(conn: &Connection) -> Vec<Vec<Vec<Value>>> {
            [
                "assignments",
                "assignment_sources",
                "courses",
                "course_sources",
                "course_schedule",
            ]
            .into_iter()
            .map(|table| {
                let mut statement = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
                    .unwrap();
                let columns = statement.column_count();
                statement
                    .query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap()
            })
            .collect()
        }

        fn metadata(conn: &Connection, id: &AssignmentId) -> (String, String) {
            conn.query_row(
                "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        }

        fn age_updated_at(conn: &Connection, id: &AssignmentId) {
            conn.execute(
                "UPDATE assignments SET updated_at = ?1 WHERE id = ?2",
                params!["2000-01-01T00:00:00.000000001Z", id.as_str()],
            )
            .unwrap();
        }

        fn utc(value: &str) -> DateTime<Utc> {
            DateTime::parse_from_rfc3339(value)
                .unwrap()
                .with_timezone(&Utc)
        }

        fn assert_safe(error: WebClassSyncError, db: &TestDatabase) {
            let rendered = format!("{error} {error:?}");
            for forbidden in [
                PRIVATE,
                SYNTHETIC_COOKIE,
                SYNTHETIC_TOKEN,
                SYNTHETIC_BODY,
                GROUP,
                "fixture-content-001",
                "fixture-content-003",
                "演習課題A",
                "group_id",
                "contents_name",
                "127.0.0.1",
                ORIGIN,
                LOGIN_PATH,
                CONTENTS_PATH,
                "<html>",
                "SELECT",
                "RAISE",
                "assignment_sources",
            ] {
                assert!(!rendered.contains(forbidden));
            }
            assert!(!rendered.contains(db.path().to_str().unwrap()));
            assert!(error.source().is_none());
        }

        #[test]
        fn sync_imports_and_resyncs_with_and_without_course_binding() {
            for bound in [false, true] {
                let db = TestDatabase::new();
                let (mut store, observer) = db.open();
                let course_id = if bound {
                    Some(
                        store
                            .create_course_with_source(
                                "Synthetic course",
                                vec![],
                                OriginKind::WebClass,
                                GROUP,
                            )
                            .unwrap()
                            .id,
                    )
                } else {
                    None
                };
                let mock = SyncMock::new();
                assert_eq!(
                    mock.sync_json(&mut store, GROUP, CONTENTS).unwrap(),
                    AssignmentImportSummary {
                        created: 3,
                        ..Default::default()
                    }
                );
                let first = stored(&store, GROUP, "fixture-content-001");
                assert_eq!(first.title, "演習課題A");
                assert_eq!(
                    first.deadline,
                    Some(Deadline::DateTime(utc("2026-10-31T14:59:59Z")))
                );
                for id in [
                    "fixture-content-001",
                    "fixture-content-002",
                    "fixture-content-003",
                ] {
                    let assignment = stored(&store, GROUP, id);
                    assert!(assignment.id.as_str().starts_with("a_"));
                    assert_eq!(assignment.course_id, course_id);
                    assert_eq!(assignment.submission, SubmissionTarget::WebClass);
                    assert_eq!(assignment.submission_url, None);
                    assert_eq!(assignment.completion, CompletionState::Incomplete);
                    let (created, updated) = metadata(&observer, &assignment.id);
                    assert_eq!(created, updated);
                }
                let initial = snapshot(&observer);
                assert_eq!(initial[0].len(), 3);
                assert_eq!(initial[1].len(), 3);
                assert_eq!(initial[2].len(), usize::from(bound));
                assert_eq!(initial[3].len(), usize::from(bound));
                assert_eq!(
                    mock.sync_json(&mut store, GROUP, CONTENTS).unwrap(),
                    AssignmentImportSummary {
                        unchanged: 3,
                        ..Default::default()
                    }
                );
                assert_eq!(snapshot(&observer), initial);

                store
                    .set_completion(&first.id, CompletionState::Completed)
                    .unwrap();
                let completed = snapshot(&observer);
                assert_eq!(
                    mock.sync_json(&mut store, GROUP, CONTENTS).unwrap(),
                    AssignmentImportSummary {
                        unchanged: 3,
                        ..Default::default()
                    }
                );
                assert_eq!(snapshot(&observer), completed);
                let mut answered = records();
                for record in &mut answered {
                    record["scores"] =
                        json!([{"max_score": 0, "answer_datetime": "2026-10-04 12:00:00"}]);
                }
                assert_eq!(
                    mock.sync_json(
                        &mut store,
                        GROUP,
                        &serde_json::to_string(&answered).unwrap()
                    )
                    .unwrap(),
                    AssignmentImportSummary {
                        unchanged: 3,
                        ..Default::default()
                    }
                );
                assert_eq!(snapshot(&observer), completed);
                let resynced = stored(&store, GROUP, "fixture-content-001");
                assert_eq!(resynced.id, first.id);
                assert_eq!(resynced.completion, CompletionState::Completed);
                assert_eq!(
                    stored(&store, GROUP, "fixture-content-002").completion,
                    CompletionState::Incomplete
                );
            }
        }

        #[test]
        fn sync_updates_title_and_deadline_preserving_identity_completion_and_created_at() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            mock.sync_json(&mut store, GROUP, CONTENTS).unwrap();
            let original = stored(&store, GROUP, "fixture-content-001");
            store
                .set_completion(&original.id, CompletionState::Completed)
                .unwrap();
            age_updated_at(&observer, &original.id);
            let previous = metadata(&observer, &original.id);
            let mut changed = records();
            changed[0]["contents_name"] = json!(PRIVATE);
            changed[0]["end_date"] = json!("2026-11-10 09:00:00");
            let before = Utc::now();
            assert_eq!(
                mock.sync_json(&mut store, GROUP, &serde_json::to_string(&changed).unwrap())
                    .unwrap(),
                AssignmentImportSummary {
                    updated: 1,
                    unchanged: 2,
                    created: 0
                }
            );
            let after = Utc::now();
            let updated = stored(&store, GROUP, "fixture-content-001");
            assert_eq!(updated.id, original.id);
            assert_eq!(updated.title, PRIVATE);
            assert_eq!(
                updated.deadline,
                Some(Deadline::DateTime(utc("2026-11-10T00:00:00Z")))
            );
            assert_eq!(updated.completion, CompletionState::Completed);
            let current = metadata(&observer, &updated.id);
            assert_eq!(current.0, previous.0);
            assert_ne!(current.1, previous.1);
            assert!(before <= utc(&current.1) && utc(&current.1) <= after);
            assert_eq!(snapshot(&observer)[0].len(), 3);
        }

        #[test]
        fn sync_reuses_authenticated_client_across_exact_opaque_groups() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            mock.sync_json(&mut store, GROUP, CONTENTS).unwrap();
            let other_group = " group &role=admin+日本/%?#0002 ";
            let mut other = records();
            for record in &mut other {
                record["group_id"] = json!(other_group);
            }
            let body = serde_json::to_string(&other).unwrap();
            assert_eq!(
                mock.sync_json(&mut store, other_group, &body).unwrap(),
                AssignmentImportSummary {
                    created: 3,
                    ..Default::default()
                }
            );
            assert_ne!(
                stored(&store, GROUP, "fixture-content-001").id,
                stored(&store, other_group, "fixture-content-001").id
            );
            let before = snapshot(&observer);
            assert_eq!(
                mock.sync_json(&mut store, other_group, &body).unwrap(),
                AssignmentImportSummary {
                    unchanged: 3,
                    ..Default::default()
                }
            );
            assert_eq!(snapshot(&observer), before);
            assert!(before[2].is_empty());
        }

        #[test]
        fn sync_absent_hidden_unsupported_and_empty_candidates_preserve_database() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            mock.sync_json(&mut store, GROUP, CONTENTS).unwrap();
            let first = stored(&store, GROUP, "fixture-content-001");
            store
                .set_completion(&first.id, CompletionState::Completed)
                .unwrap();
            let before = snapshot(&observer);
            let mut filtered = records();
            filtered[0]["hidden_content"] = json!(true);
            filtered[1]["contents_kind"] = json!("Other");
            filtered.pop();
            for body in [EMPTY.to_owned(), serde_json::to_string(&filtered).unwrap()] {
                assert_eq!(
                    mock.sync_json(&mut store, GROUP, &body).unwrap(),
                    AssignmentImportSummary::default()
                );
                assert_eq!(snapshot(&observer), before);
            }
            filtered[0]["hidden_content"] = json!(false);
            assert_eq!(
                mock.sync_json(
                    &mut store,
                    GROUP,
                    &serde_json::to_string(&filtered).unwrap()
                )
                .unwrap(),
                AssignmentImportSummary {
                    unchanged: 1,
                    ..Default::default()
                }
            );
            assert_eq!(snapshot(&observer), before);
        }

        #[test]
        fn sync_distinguishes_fetch_failure_and_auth_expiry_without_database_changes_or_retry() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            mock.sync_json(&mut store, GROUP, CONTENTS).unwrap();
            let before = snapshot(&observer);
            let login_html = format!(
                "<html>{SYNTHETIC_BODY}<a href='{LOGIN_PATH}?token={SYNTHETIC_TOKEN}'>{PRIVATE}</a></html>"
            );
            let login_redirect = format!("Location: {LOGIN_PATH}?token={SYNTHETIC_TOKEN}\r\n");
            for (status, headers, body, expected) in [
                (
                    "500 Internal Server Error",
                    "Content-Type: text/html\r\n",
                    login_html.as_str(),
                    WebClassSyncError::Fetch,
                ),
                (
                    "302 Found",
                    login_redirect.as_str(),
                    "",
                    WebClassSyncError::AuthExpired,
                ),
                (
                    "200 OK",
                    "Content-Type: text/html\r\n",
                    login_html.as_str(),
                    WebClassSyncError::AuthExpired,
                ),
            ] {
                let error = mock
                    .sync(&mut store, GROUP, status, headers, body)
                    .unwrap_err();
                assert_eq!(error, expected);
                assert_safe(error, &db);
                assert_eq!(snapshot(&observer), before);
            }
        }

        #[test]
        fn sync_parse_mapping_group_and_candidate_validation_failures_prevent_partial_import() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            mock.sync_json(&mut store, GROUP, CONTENTS).unwrap();
            let first = stored(&store, GROUP, "fixture-content-001");
            store
                .set_completion(&first.id, CompletionState::Completed)
                .unwrap();
            let before = snapshot(&observer);
            let mut changed = records();
            changed[0]["contents_name"] = json!(PRIVATE);
            let malformed = format!("{} {PRIVATE}", serde_json::to_string(&changed).unwrap());
            let mut invalid_date = changed.clone();
            invalid_date[2]["end_date"] = json!(PRIVATE);
            let mut hidden_mismatch = changed.clone();
            hidden_mismatch[2]["group_id"] = json!(PRIVATE);
            hidden_mismatch[2]["hidden_content"] = json!(true);
            let mut unsupported_mismatch = changed.clone();
            unsupported_mismatch[2]["group_id"] = json!(PRIVATE);
            unsupported_mismatch[2]["contents_kind"] = json!("Other");
            let mut duplicate = changed.clone();
            duplicate[0]["contents_id"] = json!(PRIVATE);
            duplicate[2]["contents_id"] = json!(PRIVATE);
            let mut blank_id = changed.clone();
            blank_id[2]["contents_id"] = json!(" \t ");
            let mut blank_group = changed;
            for record in &mut blank_group {
                record["group_id"] = json!(" \t ");
            }
            for (group, body, expected) in [
                (GROUP, malformed, WebClassSyncError::Parse),
                (
                    GROUP,
                    serde_json::to_string(&invalid_date).unwrap(),
                    WebClassSyncError::Map,
                ),
                (
                    GROUP,
                    serde_json::to_string(&hidden_mismatch).unwrap(),
                    WebClassSyncError::Validation,
                ),
                (
                    GROUP,
                    serde_json::to_string(&unsupported_mismatch).unwrap(),
                    WebClassSyncError::Validation,
                ),
                (
                    GROUP,
                    serde_json::to_string(&duplicate).unwrap(),
                    WebClassSyncError::Validation,
                ),
                (
                    GROUP,
                    serde_json::to_string(&blank_id).unwrap(),
                    WebClassSyncError::Validation,
                ),
                (
                    " \t ",
                    serde_json::to_string(&blank_group).unwrap(),
                    WebClassSyncError::Validation,
                ),
            ] {
                let error = mock.sync_json(&mut store, group, &body).unwrap_err();
                assert_eq!(error, expected);
                assert_safe(error, &db);
                assert_eq!(snapshot(&observer), before);
            }
        }

        #[test]
        fn sync_persistence_failure_rolls_back_prior_update_insert_and_source_bindings() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let mut changed = records();
            mock.sync_json(
                &mut store,
                GROUP,
                &serde_json::to_string(&changed[..1]).unwrap(),
            )
            .unwrap();
            let original = stored(&store, GROUP, "fixture-content-001");
            store
                .set_completion(&original.id, CompletionState::Completed)
                .unwrap();
            age_updated_at(&observer, &original.id);
            changed[0]["contents_name"] = json!(PRIVATE);
            changed[2]["contents_id"] = json!(PRIVATE);
            // Reject the third record's binding after an update and another new
            // assignment/binding have succeeded inside the same transaction.
            observer
                .execute_batch(&format!(
                    "CREATE TRIGGER reject_sync_binding BEFORE INSERT ON assignment_sources
                 WHEN NEW.external_id = '{PRIVATE}'
                 BEGIN SELECT RAISE(ABORT, NEW.scope || NEW.external_id); END;"
                ))
                .unwrap();
            let before = snapshot(&observer);
            let body = serde_json::to_string(&changed).unwrap();
            let error = mock.sync_json(&mut store, GROUP, &body).unwrap_err();
            assert_eq!(error, WebClassSyncError::Persistence);
            assert_safe(error, &db);
            assert_eq!(snapshot(&observer), before);
            for id in ["fixture-content-002", PRIVATE] {
                assert_eq!(
                    store
                        .find_assignment_by_external_key(&key(GROUP, id))
                        .unwrap(),
                    None
                );
            }
            observer
                .execute_batch("DROP TRIGGER reject_sync_binding")
                .unwrap();
            assert_eq!(
                mock.sync_json(&mut store, GROUP, &body).unwrap(),
                AssignmentImportSummary {
                    created: 2,
                    updated: 1,
                    unchanged: 0
                }
            );
            let recovered = stored(&store, GROUP, "fixture-content-001");
            assert_eq!(recovered.id, original.id);
            assert_eq!(recovered.completion, CompletionState::Completed);
        }

        #[test]
        fn sync_does_not_hold_a_sqlite_transaction_during_http_fetch() {
            let db = TestDatabase::new();
            let (mut store, _observer) = db.open();
            let path = db.path();
            let mock = SyncMock::new();
            let listener = mock.listener.try_clone().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request_headers(&mut stream);
                let writer = Connection::open(path).unwrap();
                writer.busy_timeout(Duration::ZERO).unwrap();
                // HTTP is waiting for a response; another connection must be
                // able to acquire the write lock without waiting for sync.
                writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").unwrap();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\n{JSON_HEADERS}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    CONTENTS.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                stream.write_all(CONTENTS.as_bytes()).unwrap();
                (request, listener)
            });
            assert_eq!(
                sync_webclass_group_once(&mock.client, GROUP, &mut store).unwrap(),
                AssignmentImportSummary {
                    created: 3,
                    ..Default::default()
                }
            );
            mock.assert_request(&finish(server), GROUP);
        }
    }
}
