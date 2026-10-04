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

    // Keep orchestration integration tests beside the existing private HTTP mock
    // helpers, so no HTTP feed constructor or raw store access becomes public.
    mod sync_tests {
        use std::fs;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        use chrono::{DateTime, Utc};
        use rusqlite::{Connection, params, types::Value};

        use super::*;
        use crate::domain::{
            Assignment, AssignmentId, CompletionState, Deadline, SubmissionTarget,
        };
        use crate::import::ExternalRecordKey;
        use crate::persistence::{AssignmentImportSummary, SqliteStore};
        use crate::sync::{MoodleSyncError, sync_moodle_once};

        const MULTIPLE: &str = include_str!("../fixtures/moodle/calendar_multiple.ics");
        const EMPTY: &str = include_str!("../fixtures/moodle/calendar_empty.ics");
        const PRIVATE: &str = "SYNTHETIC_PRIVATE_SYNC_VALUE_7A";

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
                    "assignment-manager-moodle-sync-{}-{nonce}-{sequence}",
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
            feed: MoodleFeed,
            listener: TcpListener,
        }

        impl SyncMock {
            fn new() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let feed = local_feed(listener.local_addr().unwrap());
                Self { feed, listener }
            }

            fn sync(
                &self,
                client: &MoodleClient,
                store: &mut SqliteStore,
                status: &'static str,
                body: &str,
            ) -> Result<AssignmentImportSummary, MoodleSyncError> {
                let (_, server) = mock_response_at(
                    self.listener.try_clone().unwrap(),
                    status,
                    "",
                    body.as_bytes().to_vec(),
                );
                let result = sync_moodle_once(client, &self.feed, store);
                finish(server);
                result
            }
        }

        fn key(id: &str) -> ExternalRecordKey {
            ExternalRecordKey {
                origin: OriginKind::Moodle,
                scope: SCOPE.into(),
                id: id.into(),
            }
        }

        fn stored(store: &SqliteStore, id: &str) -> Assignment {
            store
                .find_assignment_by_external_key(&key(id))
                .unwrap()
                .unwrap()
        }

        // Compare all persisted rows, including timestamps, bindings and courses.
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

        fn assert_safe(error: MoodleSyncError, db: &TestDatabase) {
            assert_secret_free(&error);
            let rendered = format!("{error} {error:?}");
            for forbidden in [
                PRIVATE,
                SCOPE,
                "1001@cms.example.edu",
                "2002@cms.example.edu",
                "2003@cms.example.edu",
                "課題（DFS/BFS）",
                "演習課題01",
                "BEGIN:VCALENDAR",
                "authtoken",
                "calendar/export",
                "127.0.0.1",
                "SELECT",
                "RAISE",
                "assignment_sources",
            ] {
                assert!(!rendered.contains(forbidden));
            }
            assert!(!rendered.contains(db.path().to_str().unwrap()));
        }

        #[test]
        fn sync_imports_then_resyncs_same_feed_and_preserves_local_completion() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", MULTIPLE).unwrap(),
                AssignmentImportSummary {
                    created: 2,
                    ..Default::default()
                }
            );
            let end = stored(&store, "2002@cms.example.edu");
            let deadline = stored(&store, "2003@cms.example.edu");
            assert_eq!(end.title, "課題（DFS/BFS）");
            assert_eq!(
                end.deadline,
                Some(Deadline::DateTime(utc("2026-10-08T01:50:00Z")))
            );
            assert_eq!(deadline.title, "CF-L01課題");
            assert_eq!(
                deadline.deadline,
                Some(Deadline::DateTime(utc("2026-10-04T14:59:00Z")))
            );
            for assignment in [&end, &deadline] {
                assert!(assignment.id.as_str().starts_with("a_"));
                assert_eq!(assignment.course_id, None);
                assert_eq!(assignment.submission, SubmissionTarget::Unknown);
                assert_eq!(assignment.submission_url, None);
                assert_eq!(assignment.completion, CompletionState::Incomplete);
                let (created, updated) = metadata(&observer, &assignment.id);
                assert_eq!(created, updated);
            }
            assert_eq!(
                store
                    .find_assignment_by_external_key(&key("2001@cms.example.edu"))
                    .unwrap(),
                None
            );
            let initial = snapshot(&observer);
            assert!(initial[2].is_empty());
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", MULTIPLE).unwrap(),
                AssignmentImportSummary {
                    unchanged: 2,
                    ..Default::default()
                }
            );
            assert_eq!(snapshot(&observer), initial);

            store
                .set_completion(&deadline.id, CompletionState::Completed)
                .unwrap();
            let completed = snapshot(&observer);
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", MULTIPLE).unwrap(),
                AssignmentImportSummary {
                    unchanged: 2,
                    ..Default::default()
                }
            );
            assert_eq!(snapshot(&observer), completed);
            let resynced = stored(&store, "2003@cms.example.edu");
            assert_eq!(resynced.id, deadline.id);
            assert_eq!(resynced.completion, CompletionState::Completed);
        }

        #[test]
        fn sync_updates_source_fields_preserving_id_completion_and_created_at() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            mock.sync(&client, &mut store, "200 OK", BASIC).unwrap();
            let original = stored(&store, "1001@cms.example.edu");
            store
                .set_completion(&original.id, CompletionState::Completed)
                .unwrap();
            age_updated_at(&observer, &original.id);
            let previous = metadata(&observer, &original.id);
            let changed = BASIC
                .replace("演習課題01", PRIVATE)
                .replace("20260927T145900Z", "20261010T090000Z");
            let before = Utc::now();
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", &changed).unwrap(),
                AssignmentImportSummary {
                    updated: 1,
                    ..Default::default()
                }
            );
            let after = Utc::now();
            let updated = stored(&store, "1001@cms.example.edu");
            assert_eq!(updated.id, original.id);
            assert_eq!(updated.title, PRIVATE);
            assert_eq!(
                updated.deadline,
                Some(Deadline::DateTime(utc("2026-10-10T09:00:00Z")))
            );
            assert_eq!(updated.completion, CompletionState::Completed);
            let current = metadata(&observer, &updated.id);
            assert_eq!(current.0, previous.0);
            assert_ne!(current.1, previous.1);
            assert!(before <= utc(&current.1) && utc(&current.1) <= after);
            assert_eq!(snapshot(&observer)[0].len(), 1);
        }

        #[test]
        fn sync_candidate_absence_preserves_all_existing_rows() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            mock.sync(&client, &mut store, "200 OK", MULTIPLE).unwrap();
            let absent = stored(&store, "2003@cms.example.edu");
            store
                .set_completion(&absent.id, CompletionState::Completed)
                .unwrap();
            let before = snapshot(&observer);
            let (prefix, _) = MULTIPLE.rsplit_once("BEGIN:VEVENT").unwrap();
            let reduced = format!("{prefix}END:VCALENDAR\n");
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", &reduced).unwrap(),
                AssignmentImportSummary {
                    unchanged: 1,
                    ..Default::default()
                }
            );
            assert_eq!(snapshot(&observer), before);
            assert_eq!(stored(&store, "2003@cms.example.edu").id, absent.id);
        }

        #[test]
        fn sync_empty_and_fully_filtered_calendars_return_zero_counts_without_changes() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            mock.sync(&client, &mut store, "200 OK", BASIC).unwrap();
            let before = snapshot(&observer);
            let filtered = BASIC.replace("の提出期限", " の受験可能期間の開始");
            for body in [EMPTY, filtered.as_str()] {
                assert_eq!(
                    mock.sync(&client, &mut store, "200 OK", body).unwrap(),
                    AssignmentImportSummary::default()
                );
                assert_eq!(snapshot(&observer), before);
            }
        }

        #[test]
        fn sync_fetch_parse_and_validation_failures_leave_database_unchanged() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            mock.sync(&client, &mut store, "200 OK", MULTIPLE).unwrap();
            let existing = stored(&store, "2002@cms.example.edu");
            store
                .set_completion(&existing.id, CompletionState::Completed)
                .unwrap();
            let before = snapshot(&observer);
            let changed = MULTIPLE.replace("課題（DFS/BFS）", PRIVATE);
            // A later malformed event must prevent an earlier valid update.
            let malformed = changed
                .replace("DTSTART:20261004T145900Z\n", "")
                .replace("DESCRIPTION:", &format!("DESCRIPTION:{PRIVATE}"));
            let duplicate = changed
                .replace("2002@cms.example.edu", PRIVATE)
                .replace("2003@cms.example.edu", PRIVATE);
            for (status, body, expected) in [
                ("500 Internal Server Error", PRIVATE, MoodleSyncError::Fetch),
                ("200 OK", malformed.as_str(), MoodleSyncError::Parse),
                ("200 OK", duplicate.as_str(), MoodleSyncError::Validation),
            ] {
                let error = mock.sync(&client, &mut store, status, body).unwrap_err();
                assert_eq!(error, expected);
                assert_safe(error, &db);
                assert_eq!(snapshot(&observer), before);
            }
        }

        #[test]
        fn sync_persistence_failure_rolls_back_prior_update_insert_and_binding() {
            let db = TestDatabase::new();
            let (mut store, observer) = db.open();
            let mock = SyncMock::new();
            let client = MoodleClient::new().unwrap();
            let (prefix, _) = MULTIPLE.rsplit_once("BEGIN:VEVENT").unwrap();
            let initial = format!("{prefix}END:VCALENDAR\n");
            mock.sync(&client, &mut store, "200 OK", &initial).unwrap();
            let existing = stored(&store, "2002@cms.example.edu");
            store
                .set_completion(&existing.id, CompletionState::Completed)
                .unwrap();
            age_updated_at(&observer, &existing.id);
            // An ordinary SQLite trigger rejects the later binding with a raw
            // synthetic identity in its cause, after the earlier source update.
            observer
                .execute_batch(
                    "CREATE TRIGGER reject_sync_binding BEFORE INSERT ON assignment_sources
                 BEGIN SELECT RAISE(ABORT, NEW.scope || NEW.external_id); END;",
                )
                .unwrap();
            let before = snapshot(&observer);
            let changed = MULTIPLE
                .replace("課題（DFS/BFS）", PRIVATE)
                .replace("2003@cms.example.edu", PRIVATE);
            let error = mock
                .sync(&client, &mut store, "200 OK", &changed)
                .unwrap_err();
            assert_eq!(error, MoodleSyncError::Persistence);
            assert_safe(error, &db);
            assert_eq!(snapshot(&observer), before);
            assert_eq!(
                store
                    .find_assignment_by_external_key(&key(PRIVATE))
                    .unwrap(),
                None
            );

            observer
                .execute_batch("DROP TRIGGER reject_sync_binding")
                .unwrap();
            assert_eq!(
                mock.sync(&client, &mut store, "200 OK", &changed).unwrap(),
                AssignmentImportSummary {
                    created: 1,
                    updated: 1,
                    unchanged: 0
                }
            );
            assert_eq!(stored(&store, "2002@cms.example.edu").id, existing.id);
            assert_eq!(
                stored(&store, "2002@cms.example.edu").completion,
                CompletionState::Completed
            );
        }

        #[test]
        fn sync_does_not_hold_a_sqlite_transaction_while_waiting_for_http() {
            let db = TestDatabase::new();
            let (mut store, _observer) = db.open();
            let path = db.path();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let feed = local_feed(listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request_headers(&mut stream);
                let writer = Connection::open(path).unwrap();
                writer.busy_timeout(Duration::ZERO).unwrap();
                // The fetch is still waiting for this response. A write lock
                // must be immediately available from another SQLite connection.
                writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").unwrap();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    BASIC.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                stream.write_all(BASIC.as_bytes()).unwrap();
                (request, listener)
            });
            assert_eq!(
                sync_moodle_once(&MoodleClient::new().unwrap(), &feed, &mut store).unwrap(),
                AssignmentImportSummary {
                    created: 1,
                    ..Default::default()
                }
            );
            finish(server);
        }
    }
}
