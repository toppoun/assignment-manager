//! Fixed offline fixture. No caller-supplied host, peer, resolver or CA exists.
//! All four peers are listeners allocated here; unknown names never use OS DNS.
use super::*;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};
include!("webclass_auth_fixture.rs");

pub const MOODLE_URL: &str =
    "https://moodle.synthetic.invalid/calendar/export_execute.php?token=synthetic-secret";
pub const UNIVERSITY_ID: &str = STUDENT;
pub const UNIVERSITY_PASSWORD: &str = PASSWORD;
const ICS: &str = include_str!("../fixtures/moodle/calendar_basic.ics");
const COURSE_DATA: &str = include_str!("../fixtures/webclass/courses_basic.json");
const CONTENT_DATA: &str = include_str!("../fixtures/webclass/contents_basic.json");
const CONTENTS: &str = "/webclass/ip_mods.php/plugin/score_summary_table/contents";

#[derive(Clone, Copy, Default)]
pub enum Scenario {
    #[default]
    BothSuccess,
    MoodleFailure,
    WebClassFailure,
    BothFailure,
    AuthExpiredOnce,
    AuthExpiredTwice,
    BodyLimit,
    Paced,
}
fn origins() -> [Url; 3] {
    ["hoppii", "idp", "webclass"]
        .map(|h| Url::parse(&format!("https://{h}.synthetic.invalid")).unwrap())
}
pub(super) fn auth_endpoints() -> AuthEndpoints {
    let [hoppii, idp, webclass] = origins();
    AuthEndpoints {
        hoppii,
        idp,
        webclass,
        timeout: TIMEOUT,
    }
}
pub(crate) fn hosts(source: Source) -> Vec<String> {
    match source {
        Source::Moodle => vec!["moodle.synthetic.invalid".into()],
        Source::WebClass => origins()
            .iter()
            .map(|u| u.host_str().unwrap().to_owned())
            .collect(),
    }
}
pub(crate) fn validate(url: &Url, source: Source) -> Result<(), NetworkError> {
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !hosts(source)
            .iter()
            .any(|h| Some(h.as_str()) == url.host_str())
        || (matches!(source, Source::Moodle) && url.as_str() != MOODLE_URL)
    {
        return Err(NetworkError::Policy);
    }
    Ok(())
}
pub(crate) struct FixedNetwork(HashMap<String, SocketAddr>);
impl FixedNetwork {
    pub(crate) fn peers(&self) -> Vec<SocketAddr> {
        self.0.values().copied().collect()
    }
}
impl Resolve for FixedNetwork {
    fn resolve(&self, name: Name) -> Resolving {
        let peer = self.0.get(name.as_str()).copied();
        Box::pin(async move {
            let peer = peer.ok_or(NetworkError::Policy)?;
            Ok(Box::new(std::iter::once(peer)) as Addrs)
        })
    }
}
#[derive(Default)]
struct Control {
    stop: AtomicBool,
    logins: AtomicUsize,
    requests: AtomicUsize,
    expiries: AtomicUsize,
    scenario: Mutex<Scenario>,
}
/// Owns only temporary loopback TLS listeners. Drop after the Web runtime drains.
pub struct SyntheticUniversity {
    control: Arc<Control>,
    servers: Vec<JoinHandle<()>>,
    network: Arc<FixedNetwork>,
}
impl SyntheticUniversity {
    pub fn start(scenario: Scenario) -> Self {
        let mut flow = FullFlow::with_origins(true, Some(origins()));
        flow.exchanges[2].last_mut().unwrap().body = COURSE_DATA.into();
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
        let moodle = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peers = HashMap::from([(
            "moodle.synthetic.invalid".into(),
            moodle.local_addr().unwrap(),
        )]);
        for (url, listener) in origins().iter().zip(&flow.listeners) {
            peers.insert(
                url.host_str().unwrap().into(),
                listener.local_addr().unwrap(),
            );
        }
        let control = Arc::new(Control {
            scenario: Mutex::new(scenario),
            ..Default::default()
        });
        let mut servers = Vec::new();
        for (role, (listener, template)) in
            flow.listeners.into_iter().zip(flow.exchanges).enumerate()
        {
            servers.push(serve(listener, role, template, control.clone()));
        }
        servers.push(serve(moodle, 3, Vec::new(), control.clone()));
        Self {
            control,
            servers,
            network: Arc::new(FixedNetwork(peers)),
        }
    }
    /// A fixed fixture policy, with no route to university hosts or system DNS.
    pub fn policy(&self) -> NetworkPolicy {
        let mut policy = NetworkPolicy::default();
        policy.synthetic = Some(self.network.clone());
        policy
    }
    /// In-process test control, never exposed through HTTP/environment settings.
    pub fn set_scenario(&self, scenario: Scenario) {
        *self.control.scenario.lock().unwrap() = scenario;
        self.control.expiries.store(0, Ordering::SeqCst);
    }
    pub fn logins(&self) -> usize {
        self.control.logins.load(Ordering::SeqCst)
    }
    pub fn requests(&self) -> usize {
        self.control.requests.load(Ordering::SeqCst)
    }
}
impl Drop for SyntheticUniversity {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::SeqCst);
        for server in self.servers.drain(..) {
            server.join().expect("synthetic TLS fixture failed");
        }
    }
}
fn serve(
    listener: TcpListener,
    role: usize,
    template: Vec<Exchange>,
    control: Arc<Control>,
) -> JoinHandle<()> {
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
        let mut queue = VecDeque::new();
        while !control.stop.load(Ordering::SeqCst) {
            let (tcp, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => panic!("synthetic TLS accept failed"),
            };
            tcp.set_nonblocking(false).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let request = read_request(&mut stream);
            assert_eq!(request.headers["host"], stream.conn.server_name().unwrap());
            control.requests.fetch_add(1, Ordering::SeqCst);
            let scenario = *control.scenario.lock().unwrap();
            // Reuse the existing complete SSO + consent fixture for every login.
            let starts_login = (role == 0 && request.target == USER_PAGE && queue.is_empty())
                || (role == 1 && request.target.starts_with("/idp/sso?") && queue.is_empty())
                || (role == 2 && request.target == "/webclass/singlesignon.php");
            if starts_login {
                queue = template.clone().into();
                if role == 0 {
                    control.logins.fetch_add(1, Ordering::SeqCst);
                }
            }
            let exchange = if let Some(exchange) = queue.pop_front() {
                exchange
            } else if role == 3 {
                if matches!(scenario, Scenario::Paced) {
                    thread::sleep(Duration::from_secs(2));
                }
                let status = if matches!(scenario, Scenario::MoodleFailure | Scenario::BothFailure)
                {
                    503
                } else {
                    200
                };
                let body = if matches!(scenario, Scenario::BodyLimit) {
                    "x".repeat(8 * 1024 * 1024 + 1)
                } else {
                    ICS.into()
                };
                Exchange::new(
                    "GET",
                    "/calendar/export_execute.php?token=synthetic-secret",
                    status,
                    &body,
                )
                .without_cookies()
                .without_referer()
            } else if role == 2 {
                let expires = matches!(
                    scenario,
                    Scenario::AuthExpiredOnce | Scenario::AuthExpiredTwice
                ) && request.target == COURSES
                    && control.expiries.fetch_add(1, Ordering::SeqCst)
                        < if matches!(scenario, Scenario::AuthExpiredOnce) {
                            1
                        } else {
                            2
                        };
                let mut exchange = if expires {
                    Exchange::redirect("GET", COURSES, "/webclass/login.php")
                } else if matches!(scenario, Scenario::WebClassFailure | Scenario::BothFailure) {
                    Exchange::new("GET", &request.target, 503, "synthetic-failure")
                } else if request.target == COURSES {
                    Exchange::new("GET", COURSES, 200, COURSE_DATA).json_response()
                } else {
                    let group = request
                        .target
                        .strip_prefix(&format!("{CONTENTS}?group_id="))
                        .expect("unexpected fixture path");
                    assert!(
                        [
                            "fixture-course-001",
                            "fixture-course-002",
                            "fixture-course-003"
                        ]
                        .contains(&group)
                    );
                    Exchange::new(
                        "GET",
                        &request.target,
                        200,
                        &CONTENT_DATA.replace("fixture-course-002", group),
                    )
                    .json_response()
                };
                exchange = exchange.cookie("wc_test", "synthetic-cookie");
                exchange
            } else {
                panic!("unexpected synthetic SSO request");
            };
            exchange.assert_request(&request);
            // Assert exact-origin Cookie isolation, including parent Domain cookies.
            if role != 1 {
                assert!(
                    !request
                        .headers
                        .get("cookie")
                        .is_some_and(|s| s.contains("idp_parent="))
                );
            }
            let mut headers = format!(
                "HTTP/1.1 {} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n",
                exchange.status,
                exchange.body.len()
            );
            for (name, value) in exchange.response_headers {
                headers.push_str(&format!("{name}: {value}\r\n"));
            }
            if role == 1 {
                headers.push_str(
                    "Set-Cookie: idp_parent=synthetic-parent; Domain=synthetic.invalid; Path=/\r\n",
                );
            }
            headers.push_str("\r\n");
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.write_all(exchange.body.as_bytes());
            let _ = stream.flush();
        }
    })
}
