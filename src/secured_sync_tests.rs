//! Core-side secured E2E: actual TLS/SSO, parser, observation and SQLite v3.
//! Nested in the existing protocol fixture module so no CA/DNS seam is exported.
use super::*;
use crate::{
    domain::{CompletionState, Deadline},
    import::{AssignmentCandidate, ExternalRecordKey, OriginKind},
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{self, DeferredReason, NotionSnapshot, PlanAction, SyncInput},
    observation::SyncScope,
    persistence::SqliteStore,
    secured_sync::SecuredSyncClients,
    sync::{
        self, MoodleSyncError, SourceCourse, WebClassCredentialProvider, WebClassReauthSyncError,
        WebClassSyncError,
    },
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
const COURSE_ICS: &str = include_str!("../fixtures/moodle/calendar_course_related.ics");
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
        result.push(contents_response(
            &group,
            &CONTENT_DATA.replace("fixture-course-002", &group),
        ));
    }
    result
}
fn contents_response(group: &str, body: &str) -> Exchange {
    Exchange::new("GET", &format!("{CONTENTS}?group_id={group}"), 200, body)
        .json_response()
        .cookie("wc_test", "synthetic-cookie")
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

fn assert_import_matches(
    store: &SqliteStore,
    candidate: &AssignmentCandidate,
    course: &SourceCourse,
) {
    let assignment = store
        .find_assignment_by_external_key(&candidate.external_key)
        .unwrap()
        .unwrap();
    assert_eq!(assignment.title, candidate.title);
    assert_eq!(assignment.deadline, candidate.deadline);
    assert_eq!(assignment.submission, candidate.submission);
    assert_eq!(assignment.submission_url, None);
    assert_eq!(candidate.submission_url, None);
    let key = &course.external_key;
    let stored_course = store
        .find_course_by_source(key.origin, &key.scope, &key.id)
        .unwrap()
        .unwrap();
    assert_eq!(assignment.course_id.as_ref(), Some(&stored_course.id));
    assert_eq!(stored_course.name, course.name);
}

#[test]
fn acquisition_without_a_store_matches_both_legacy_imports() {
    let mut f = flow();
    f.exchanges[2].extend(full_sync());
    f.exchanges[2].extend(full_sync());
    let calendar_responses = (0..2)
        .map(|pass| {
            let mut response = moodle_response(200);
            let lf = COURSE_ICS.replace("\r\n", "\n");
            response.body = if pass == 0 {
                lf
            } else {
                lf.replace('\n', "\r\n")
            };
            response
        })
        .collect();
    let (mut clients, servers, _) = start(f, calendar_responses);
    clients.begin_run();
    let feed = feed();
    let moodle_client = clients.moodle().unwrap();
    let moodle = sync::acquire_moodle(&moodle_client, &feed).unwrap();
    let session = clients
        .authenticator()
        .login(HoseiCredentials::new(STUDENT, PASSWORD))
        .unwrap();
    let webclass = sync::acquire_webclass_once(&session);
    assert_eq!(webclass.result, Ok(()));

    let moodle_candidates = moodle.assignments.batch().as_slice();
    assert_eq!(moodle_candidates.len(), 2);
    assert_eq!(moodle.assignments.observation().keys().len(), 3);
    let first = &moodle_candidates[0];
    assert_eq!(first.external_key.origin, OriginKind::Moodle);
    assert_eq!(first.external_key.scope, feed.scope());
    assert_eq!(first.external_key.id, "synthetic-db-end@example.invalid");
    assert_eq!(first.title, "Synthetic database quiz");
    assert_eq!(
        first.deadline,
        Some(Deadline::DateTime("2026-10-05T00:00:00Z".parse().unwrap()))
    );
    let course = moodle.course_for(&first.external_key).unwrap();
    assert_eq!(course.external_key.origin, OriginKind::Moodle);
    assert_eq!(course.external_key.scope, feed.scope());
    assert_eq!(course.external_key.id, "2026-DB");

    let courses = &webclass.course_list.as_ref().unwrap().courses;
    assert_eq!(courses.len(), 3);
    assert_eq!(webclass.groups.len(), courses.len());
    assert_eq!(courses[0].external_key.scope, "default");
    assert_eq!(courses[0].external_key.id, "fixture-course-001");
    let first = &webclass.groups[0].batch().as_slice()[0];
    assert_eq!(first.external_key, key("fixture-course-001"));
    assert_eq!(first.title, "演習課題A");
    assert_eq!(
        first.deadline,
        Some(Deadline::DateTime("2026-10-31T14:59:59Z".parse().unwrap()))
    );
    let keys: std::collections::HashSet<_> = webclass
        .groups
        .iter()
        .flat_map(|group| {
            group
                .batch()
                .as_slice()
                .iter()
                .map(|item| &item.external_key)
        })
        .collect();
    assert_eq!(keys.len(), 9); // The same three contents IDs occur in each group.

    let output = format!("{moodle:?} {webclass:?}");
    for marker in SECRETS.iter().copied().chain([
        "synthetic-secret",
        "synthetic-cookie",
        "fixture-author-001",
        "synthetic-db-end@example.invalid",
        "fixture-course-001",
        "Synthetic database quiz",
        "演習課題A",
    ]) {
        assert!(!output.contains(marker));
    }

    // The independent fetches above complete before SQLite is opened.
    let mut store = SqliteStore::open(":memory:").unwrap();
    let moodle_summary = sync::sync_moodle_once(&moodle_client, &feed, &mut store).unwrap();
    assert_eq!(moodle_summary.created, moodle_candidates.len());
    for candidate in moodle_candidates {
        assert_import_matches(
            &store,
            candidate,
            moodle.course_for(&candidate.external_key).unwrap(),
        );
    }
    let webclass_summary = sync::sync_webclass_all_once(&session, &mut store).unwrap();
    assert_eq!(webclass_summary.assignments_created, keys.len());
    for (course, group) in courses.iter().zip(webclass.groups) {
        assert_eq!(
            group.observation().scope(),
            &SyncScope::WebClassContents(course.external_key.id.clone())
        );
        for candidate in group.batch().as_slice() {
            assert_import_matches(&store, candidate, course);
        }
    }
    finish_all(servers);
}

#[test]
fn acquisition_validates_filtered_records_and_accepts_empty_batches() {
    let mut empty_calendar = moodle_response(200);
    empty_calendar.body = include_str!("../fixtures/moodle/calendar_empty.ics").into();
    let mut duplicate_calendar = moodle_response(200);
    duplicate_calendar.body = COURSE_ICS.replace(
        "UID:synthetic-ai-start@example.invalid",
        "UID:synthetic-db-end@example.invalid",
    );

    const GROUP: &str = "fixture-course-001";
    let mut records: Vec<serde_json::Value> = serde_json::from_str(CONTENT_DATA).unwrap();
    let mut hidden = records.remove(0);
    hidden["hidden_content"] = true.into();
    hidden["group_id"] = GROUP.into();
    let hidden_only = serde_json::json!([hidden]).to_string();
    let duplicate = serde_json::json!([hidden, hidden]).to_string();
    let mut wrong_group = hidden.clone();
    wrong_group["group_id"] = "fixture-course-002".into();
    hidden["end_date"] = "malformed-private-date".into();
    let mut f = flow();
    f.exchanges[2].extend([
        contents_response(GROUP, &hidden_only),
        contents_response(GROUP, &duplicate),
        contents_response(GROUP, &serde_json::json!([wrong_group]).to_string()),
        contents_response(GROUP, &serde_json::json!([hidden]).to_string()),
    ]);
    let (mut clients, servers, _) = start(f, vec![empty_calendar, duplicate_calendar]);
    clients.begin_run();

    let moodle_client = clients.moodle().unwrap();
    let empty = sync::acquire_moodle(&moodle_client, &feed()).unwrap();
    assert!(empty.assignments.batch().as_slice().is_empty());
    assert!(empty.assignments.observation().keys().is_empty());
    assert_eq!(
        sync::acquire_moodle(&moodle_client, &feed()).unwrap_err(),
        MoodleSyncError::Validation
    );

    let session = clients
        .authenticator()
        .login(HoseiCredentials::new(STUDENT, PASSWORD))
        .unwrap();
    let empty = sync::acquire_webclass_group(&session, GROUP).unwrap();
    assert!(empty.batch().as_slice().is_empty());
    assert_eq!(empty.observation().keys(), &[key(GROUP)]);
    for expected in [
        WebClassSyncError::Validation,
        WebClassSyncError::Validation,
        WebClassSyncError::Map,
    ] {
        let error = sync::acquire_webclass_group(&session, GROUP).unwrap_err();
        assert_eq!(error, expected);
        assert!(error.source().is_none());
        assert!(!format!("{error:?} {error}").contains("malformed-private-date"));
    }
    finish_all(servers);
}

#[test]
fn acquisition_reports_a_partial_failure_with_validated_groups() {
    let mut f = flow();
    let mut exchanges = full_sync();
    exchanges.truncate(3);
    exchanges[2].status = 503;
    exchanges[2].body = "synthetic-private-body".into();
    f.exchanges[2].extend(exchanges);
    let (mut clients, servers, _) = start(f, vec![]);
    clients.begin_run();
    let session = clients
        .authenticator()
        .login(HoseiCredentials::new(STUDENT, PASSWORD))
        .unwrap();

    let report = sync::acquire_webclass_once(&session);
    assert_eq!(report.result, Err(WebClassSyncError::Fetch));
    let output = format!("{report:?}");
    assert!(!output.contains("synthetic-private-body"));
    assert_eq!(report.course_list.unwrap().courses.len(), 3);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(
        report.groups[0].batch().as_slice()[0].external_key,
        key("fixture-course-001")
    );
    finish_all(servers);
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

fn flow_with_expiry_and_retry(second_expiry: bool) -> FullFlow {
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
    f
}

#[test]
fn acquisition_retains_both_attempts_and_reauthenticates_only_once() {
    for second_expiry in [false, true] {
        let (mut clients, servers, _) = start(flow_with_expiry_and_retry(second_expiry), vec![]);
        clients.begin_run();
        let authenticator = clients.authenticator();
        let mut session = authenticator
            .login(HoseiCredentials::new(STUDENT, PASSWORD))
            .unwrap();
        let mut provider = Provider(0);
        let report =
            sync::acquire_webclass_with_reauth(&mut session, &authenticator, &mut provider);
        assert_eq!(provider.0, 1);
        assert!(clients.owns_session(&session));
        assert_eq!(report.attempts().len(), 2);
        let first = &report.attempts()[0];
        assert_eq!(first.result, Err(WebClassSyncError::AuthExpired));
        assert_eq!(first.groups.len(), 1);
        assert_eq!(
            first.groups[0].batch().as_slice()[0].external_key,
            key("fixture-course-001")
        );
        let retry = &report.attempts()[1];
        if second_expiry {
            assert_eq!(
                report.result(),
                Err(WebClassReauthSyncError::Sync(
                    WebClassSyncError::AuthExpired
                ))
            );
            assert_eq!(retry.result, Err(WebClassSyncError::AuthExpired));
            assert!(retry.course_list.is_none());
            assert!(retry.groups.is_empty());
        } else {
            assert_eq!(report.result(), Ok(()));
            assert_eq!(retry.result, Ok(()));
            assert_eq!(retry.groups.len(), 3);
            assert_eq!(
                first.groups[0].batch().as_slice(),
                retry.groups[0].batch().as_slice()
            );
        }
        let input = SyncInput::from_acquisitions(None, Some(&report)).unwrap();
        let plan = notion::plan_sync(&input, NotionSnapshot::Complete(&[]), &[]);
        if second_expiry {
            assert_eq!(plan.len(), 3);
            assert!(plan.iter().all(|item| item.action == PlanAction::Deferred(
                DeferredReason::EarlierAttempt
            )));
        } else {
            assert_eq!(plan.len(), 9);
            assert!(
                plan.iter()
                    .all(|item| matches!(item.action, PlanAction::Create { .. }))
            );
        }
        finish_all(servers);
    }
}

#[test]
fn secured_partial_commit_and_auth_expired_retry_once_keeps_policy_and_cookie_partition() {
    for second_expiry in [false, true] {
        let (mut clients, servers, _) = start(flow_with_expiry_and_retry(second_expiry), vec![]);
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
