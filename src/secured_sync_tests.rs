//! Core-side secured E2E: actual TLS/SSO, parser, observation and SQLite v3.
//! Nested in the existing protocol fixture module so no CA/DNS seam is exported.
use super::*;
use crate::{
    domain::CompletionState,
    import::{ExternalRecordKey, OriginKind},
    moodle_http::{MoodleExportKind, MoodleFeed},
    observation::SyncScope,
    persistence::SqliteStore,
    secured_sync::SecuredSyncClients,
    sync::{self, WebClassCredentialProvider, WebClassReauthSyncError, WebClassSyncError},
    university_network::NetworkError,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
const ICS: &str = include_str!("../fixtures/moodle/calendar_basic.ics");
const COURSE_DATA: &str = include_str!("../fixtures/webclass/courses_basic.json");
const CONTENT_DATA: &str = include_str!("../fixtures/webclass/contents_basic.json");
const MOODLE: &str =
    "https://moodle.synthetic.invalid/calendar/export_execute.php?token=synthetic-secret";
const CONTENTS: &str = "/webclass/ip_mods.php/plugin/score_summary_table/contents";

struct Lookup(HashMap<String, SocketAddr>, Arc<AtomicUsize>);
impl Resolve for Lookup {
    fn resolve(&self, name: Name) -> Resolving {
        self.1.fetch_add(1, Ordering::SeqCst);
        let peer = self.0.get(name.as_str()).copied();
        Box::pin(async move {
            let peer = peer.ok_or(NetworkError::Policy)?;
            Ok(Box::new(std::iter::once(peer)) as Addrs)
        })
    }
}
fn tls_script(listener: TcpListener, exchanges: Vec<Exchange>) -> JoinHandle<TcpListener> {
    thread::spawn(move || {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                include_bytes!("../fixtures/network/server.der").to_vec(),
            )],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                include_bytes!("../fixtures/network/server-key.der").to_vec(),
            )),
        )
        .unwrap();
        let config = Arc::new(config);
        listener.set_nonblocking(true).unwrap();
        for exchange in exchanges {
            let until = Instant::now() + Duration::from_secs(10);
            let tcp = loop {
                match listener.accept() {
                    Ok((tcp, _)) => break tcp,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("synthetic TLS request missing: {e}"),
                }
            };
            tcp.set_nonblocking(false).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let request = read_request(&mut stream);
            assert_eq!(request.headers["host"], stream.conn.server_name().unwrap());
            exchange.assert_request(&request);
            let bytes = exchange
                .response_bytes
                .as_deref()
                .unwrap_or(exchange.body.as_bytes());
            let mut headers = format!(
                "HTTP/1.1 {} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n",
                exchange.status,
                bytes.len()
            );
            for (name, value) in exchange.response_headers {
                headers.push_str(&format!("{name}: {value}\r\n"));
            }
            headers.push_str("\r\n");
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(bytes);
            let _ = stream.flush();
        }
        listener
    })
}
fn origins() -> [Url; 3] {
    ["hoppii", "idp", "webclass"]
        .map(|h| Url::parse(&format!("https://{h}.synthetic.invalid")).unwrap())
}
fn flow() -> FullFlow {
    let mut flow = FullFlow::with_origins(true, Some(origins()));
    flow.exchanges[2].last_mut().unwrap().body = COURSE_DATA.into();
    // Web Referers intentionally strip the query, including the fixed PARM URL.
    for exchanges in &mut flow.exchanges {
        for exchange in exchanges {
            for (name, value) in &mut exchange.headers {
                if *name == "referer" {
                    let mut url = Url::parse(value).unwrap();
                    url.set_query(None);
                    url.set_fragment(None);
                    *value = url.into();
                }
            }
        }
    }
    flow
}
fn course_response() -> Exchange {
    Exchange::new("GET", COURSES, 200, COURSE_DATA)
        .json_response()
        .cookie("wc_test", "synthetic-cookie")
}
fn full_sync() -> Vec<Exchange> {
    let mut result = vec![course_response()];
    for n in 1..=3 {
        let group = format!("fixture-course-{n:03}");
        result.push(
            Exchange::new(
                "GET",
                &format!("{CONTENTS}?group_id={group}"),
                200,
                &CONTENT_DATA.replace("fixture-course-002", &group),
            )
            .json_response()
            .cookie("wc_test", "synthetic-cookie"),
        );
    }
    result
}
fn start(
    flow: FullFlow,
    moodle: Vec<Exchange>,
) -> (
    SecuredSyncClients,
    Vec<JoinHandle<TcpListener>>,
    Arc<AtomicUsize>,
) {
    let ml = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peers = HashMap::new();
    peers.insert("moodle.synthetic.invalid".into(), ml.local_addr().unwrap());
    for (url, listener) in origins().iter().zip(&flow.listeners) {
        peers.insert(
            url.host_str().unwrap().into(),
            listener.local_addr().unwrap(),
        );
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut policy = NetworkPolicy::default()
        .with_moodle_calendar("moodle.synthetic.invalid", "/calendar/export_execute.php")
        .unwrap();
    policy.test.origins = origins().to_vec();
    policy.test.auth = Some(origins());
    policy.test.lookup = Some(Arc::new(Lookup(peers, calls.clone())));
    policy.test.root =
        Some(reqwest::Certificate::from_der(include_bytes!("../fixtures/network/ca.der")).unwrap());
    let mut servers: Vec<_> = flow
        .listeners
        .into_iter()
        .zip(flow.exchanges)
        .map(|(l, e)| tls_script(l, e))
        .collect();
    servers.push(tls_script(ml, moodle));
    (SecuredSyncClients::new(policy), servers, calls)
}
fn finish_all(servers: Vec<JoinHandle<TcpListener>>) {
    for server in servers {
        finish(server);
    }
}
fn moodle_response(status: u16) -> Exchange {
    Exchange::new(
        "GET",
        "/calendar/export_execute.php?token=synthetic-secret",
        status,
        ICS,
    )
    .without_cookies()
    .without_referer()
}
fn feed() -> MoodleFeed {
    MoodleFeed::new(
        "default.course-related",
        MoodleExportKind::CourseRelated,
        MOODLE,
    )
    .unwrap()
}
struct Provider(usize);
impl WebClassCredentialProvider for Provider {
    fn with_credentials<R>(
        &mut self,
        use_credentials: impl FnOnce(HoseiCredentials<'_>) -> R,
    ) -> Option<R> {
        self.0 += 1;
        Some(use_credentials(HoseiCredentials::new(STUDENT, PASSWORD)))
    }
}
fn key(group: &str) -> ExternalRecordKey {
    ExternalRecordKey {
        origin: OriginKind::WebClass,
        scope: group.into(),
        id: "fixture-content-001".into(),
    }
}

#[test]
fn secured_sources_matrix_tls_sso_to_sqlite_and_safe_failures() {
    for (moodle_ok, webclass_ok) in [(true, true), (false, true), (true, false), (false, false)] {
        let mut f = flow();
        if webclass_ok {
            f.exchanges[2].extend(full_sync());
        } else {
            f.exchanges[2].push(Exchange::new("GET", COURSES, 503, "synthetic-private-body"));
        }
        let (mut clients, servers, _) =
            start(f, vec![moodle_response(if moodle_ok { 200 } else { 503 })]);
        clients.begin_run();
        let mut store = SqliteStore::open(":memory:").unwrap();
        let moodle = sync::sync_moodle_once(&clients.moodle().unwrap(), &feed(), &mut store);
        assert_eq!(moodle.is_ok(), moodle_ok);
        let mut session = clients
            .authenticator()
            .login(HoseiCredentials::new(STUDENT, PASSWORD))
            .unwrap();
        let result = sync::sync_webclass_all_with_reauth(
            &mut session,
            &clients.authenticator(),
            &mut Provider(0),
            &mut store,
        );
        assert_eq!(result.is_ok(), webclass_ok);
        if let Err(error) = result {
            assert!(error.source().is_none());
            for secret in SECRETS {
                assert!(!format!("{error:?} {error}").contains(secret));
            }
        }
        assert_eq!(
            store
                .find_assignment_by_external_key(&key("fixture-course-001"))
                .unwrap()
                .is_some(),
            webclass_ok
        );
        finish_all(servers);
    }
}

#[test]
fn secured_session_reuse_budget_reset_duplicates_completion_and_positive_observation() {
    let mut f = flow();
    f.exchanges[2].extend(full_sync());
    f.exchanges[2].extend(full_sync());
    // Third run sees an empty course list: absence must not remove data/observations.
    f.exchanges[2].push(
        Exchange::new("GET", COURSES, 200, "[]")
            .json_response()
            .cookie("wc_test", "synthetic-cookie"),
    );
    let (mut clients, servers, _) = start(f, vec![moodle_response(200), moodle_response(200)]);
    let mut store = SqliteStore::open(":memory:").unwrap();
    clients.begin_run();
    let mc = clients.moodle().unwrap();
    assert_eq!(
        sync::sync_moodle_once(&mc, &feed(), &mut store)
            .unwrap()
            .created,
        1
    );
    let mut session = clients
        .authenticator()
        .login(HoseiCredentials::new(STUDENT, PASSWORD))
        .unwrap();
    assert!(clients.owns_session(&session));
    assert!(!SecuredSyncClients::new(NetworkPolicy::default()).owns_session(&session));
    let mut provider = Provider(0);
    let first = sync::sync_webclass_all_with_reauth(
        &mut session,
        &clients.authenticator(),
        &mut provider,
        &mut store,
    )
    .unwrap();
    assert_eq!(first.assignments_created, 9);
    let list = crate::query::load_assignment_list(&store).unwrap();
    assert_eq!(list.len(), 10);
    // Moodle source identity does not establish a submission target.
    // Preserve the existing Unknown mapping rather than inferring one here.
    assert_eq!(
        list.iter()
            .filter(|a| a.submission == crate::domain::SubmissionTarget::Unknown)
            .count(),
        1
    );
    assert_eq!(
        list.iter()
            .filter(|a| a.submission == crate::domain::SubmissionTarget::WebClass)
            .count(),
        9
    );
    eprintln!("Synthetic secured smoke: SQLite v3 list = 10 (Moodle 1, WebClass 9)");
    let item = store
        .find_assignment_by_external_key(&key("fixture-course-001"))
        .unwrap()
        .unwrap();
    store
        .set_completion(&item.id, CompletionState::Completed)
        .unwrap();
    clients.begin_run(); // same transport and jar, new resource accounting only
    assert_eq!(
        sync::sync_moodle_once(&mc, &feed(), &mut store)
            .unwrap()
            .unchanged,
        1
    );
    let second = sync::sync_webclass_all_with_reauth(
        &mut session,
        &clients.authenticator(),
        &mut provider,
        &mut store,
    )
    .unwrap();
    assert_eq!(second.assignments_created, 0);
    assert_eq!(second.assignments_unchanged, 9);
    assert_eq!(provider.0, 0);
    assert_eq!(
        store.get_assignment(&item.id).unwrap().unwrap().completion,
        CompletionState::Completed
    );
    let seen = store
        .get_assignment_last_seen_at(&key("fixture-course-001"))
        .unwrap();
    clients.begin_run();
    sync::sync_webclass_all_with_reauth(
        &mut session,
        &clients.authenticator(),
        &mut provider,
        &mut store,
    )
    .unwrap();
    assert_eq!(
        store
            .get_assignment_last_seen_at(&key("fixture-course-001"))
            .unwrap(),
        seen
    );
    assert!(
        store
            .get_last_synced_at(&SyncScope::WebClassCourses)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .find_assignment_by_external_key(&key("fixture-course-001"))
            .unwrap()
            .is_some()
    );
    eprintln!(
        "Synthetic secured smoke: repeated sync = 0 created; completion and positive observations retained; one SSO session reused"
    );
    finish_all(servers);
}

#[test]
fn secured_partial_commit_and_auth_expired_retry_once_keeps_policy_and_cookie_partition() {
    for second_expiry in [false, true] {
        let mut f = flow();
        let mut first = full_sync();
        first.truncate(3);
        first[2] = Exchange::redirect(
            "GET",
            &format!("{CONTENTS}?group_id=fixture-course-002"),
            "/webclass/login.php",
        );
        f.exchanges[2].extend(first);
        let retry_login = flow();
        for (target, more) in f.exchanges.iter_mut().zip(retry_login.exchanges) {
            target.extend(more);
        }
        if second_expiry {
            f.exchanges[2].push(Exchange::redirect("GET", COURSES, "/webclass/login.php"));
        } else {
            f.exchanges[2].extend(full_sync());
        }
        let (mut clients, servers, _) = start(f, vec![]);
        clients.begin_run();
        let mut store = SqliteStore::open(":memory:").unwrap();
        let mut session = clients
            .authenticator()
            .login(HoseiCredentials::new(STUDENT, PASSWORD))
            .unwrap();
        let mut provider = Provider(0);
        let result = sync::sync_webclass_all_with_reauth(
            &mut session,
            &clients.authenticator(),
            &mut provider,
            &mut store,
        );
        assert_eq!(provider.0, 1);
        assert!(clients.owns_session(&session));
        if second_expiry {
            assert_eq!(
                result,
                Err(WebClassReauthSyncError::Sync(
                    WebClassSyncError::AuthExpired
                ))
            );
        } else {
            let counts = result.unwrap();
            assert_eq!(counts.assignments_created, 6);
            assert_eq!(counts.assignments_unchanged, 3);
        }
        assert!(
            store
                .find_assignment_by_external_key(&key("fixture-course-001"))
                .unwrap()
                .is_some()
        );
        finish_all(servers);
    }
}

#[test]
fn secured_moodle_body_limit_and_policy_missing_stop_safely() {
    // Empty scripts are tripwires: any unexpected request remains on the bound listener.
    let mut f = flow();
    for e in &mut f.exchanges {
        e.clear();
    }
    let mut oversized = moodle_response(200);
    oversized.response_bytes = Some(vec![b'x'; 8 * 1024 * 1024 + 1]);
    let (mut clients, servers, calls) = start(f, vec![oversized]);
    let mut store = SqliteStore::open(":memory:").unwrap();
    clients.begin_run();
    assert_eq!(
        clients.validate_moodle_url("https://127.0.0.1/private"),
        Err(NetworkError::Policy)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sync::sync_moodle_once(&clients.moodle().unwrap(), &feed(), &mut store),
        Err(sync::MoodleSyncError::Fetch)
    );
    finish_all(servers);
    let mut missing = SecuredSyncClients::new(NetworkPolicy::default());
    missing.begin_run();
    assert_eq!(
        missing.validate_moodle_url(MOODLE),
        Err(NetworkError::Policy)
    );
    assert_eq!(
        sync::sync_moodle_once(&missing.moodle().unwrap(), &feed(), &mut store),
        Err(sync::MoodleSyncError::Fetch)
    );
}

#[test]
fn secured_course_and_import_batch_limits_reject_before_source_writes() {
    for oversized_courses in [true, false] {
        let mut f = flow();
        if oversized_courses {
            let base: Vec<Value> = serde_json::from_str(COURSE_DATA).unwrap();
            let data: Vec<_> = (0..129)
                .map(|n| {
                    let mut c = base[0].clone();
                    c["group_id"] = json!(format!("group-{n}"));
                    c
                })
                .collect();
            f.exchanges[2].push(
                Exchange::new("GET", COURSES, 200, &serde_json::to_string(&data).unwrap())
                    .json_response(),
            );
        } else {
            let base: Vec<Value> = serde_json::from_str(CONTENT_DATA).unwrap();
            let data: Vec<_> = (0..4097)
                .map(|n| {
                    let mut c = base[0].clone();
                    c["group_id"] = json!("fixture-course-001");
                    c["contents_id"] = json!(format!("content-{n}"));
                    c
                })
                .collect();
            f.exchanges[2].push(course_response());
            f.exchanges[2].push(
                Exchange::new(
                    "GET",
                    &format!("{CONTENTS}?group_id=fixture-course-001"),
                    200,
                    &serde_json::to_string(&data).unwrap(),
                )
                .json_response(),
            );
        }
        let (mut clients, servers, _) = start(f, vec![moodle_response(200)]);
        clients.begin_run();
        let mut store = SqliteStore::open(":memory:").unwrap();
        sync::sync_moodle_once(&clients.moodle().unwrap(), &feed(), &mut store).unwrap();
        let mut session = clients
            .authenticator()
            .login(HoseiCredentials::new(STUDENT, PASSWORD))
            .unwrap();
        let result = sync::sync_webclass_all_with_reauth(
            &mut session,
            &clients.authenticator(),
            &mut Provider(0),
            &mut store,
        );
        let expected = if oversized_courses {
            WebClassSyncError::CourseValidation
        } else {
            WebClassSyncError::Validation
        };
        assert_eq!(result, Err(WebClassReauthSyncError::Sync(expected)));
        assert!(
            store
                .find_assignment_by_external_key(&key("fixture-course-001"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .find_webclass_course_id("fixture-course-001")
                .unwrap()
                .is_none(),
            oversized_courses
        );
        let moodle_key = ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: "default.course-related".into(),
            id: "1001@cms.example.edu".into(),
        };
        assert!(
            store
                .find_assignment_by_external_key(&moodle_key)
                .unwrap()
                .is_some()
        );
        finish_all(servers);
    }
}
