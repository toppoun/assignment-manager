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
    /// Accept the authenticated client supplied by the future authentication module.
    ///
    /// The caller must build it with `reqwest::redirect::Policy::none()`,
    /// `reqwest::retry::never()`, and a finite timeout (for example, 15 seconds).
    /// reqwest cannot inspect or change these policies on an already built client.
    /// This constructor trusts that crate-internal contract; it creates no HTTP client.
    #[cfg_attr(not(test), allow(dead_code))] // The production caller arrives in Phase 5C.
    pub(crate) fn from_http(http: Client) -> Self {
        Self {
            http,
            base_url: Url::parse(ORIGIN).expect("the fixed WebClass origin is a valid URL"),
        }
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
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let body = body.to_vec();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            (request, listener)
        });
        (
            local_client(test_http(Duration::from_secs(2)), address),
            server,
        )
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
}
