//! No public DNS queries or university requests. All connections use loopback.
use super::*;
use crate::moodle_http::{MoodleClient, MoodleExportKind, MoodleFeed, MoodleHttpError};
use std::{
    collections::VecDeque,
    io::{self, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Instant,
};

const HOST: &str = "moodle.synthetic.invalid";
const PATH: &str = "/calendar/export_execute.php";
const URL: &str =
    "https://moodle.synthetic.invalid/calendar/export_execute.php?token=SYNTHETIC_4B1_SECRET";
const CA: &[u8] = include_bytes!("../fixtures/network/ca.der");

fn policy() -> NetworkPolicy {
    NetworkPolicy::default()
        .with_moodle_calendar(HOST, PATH)
        .unwrap()
}
fn feed(url: &str) -> MoodleFeed {
    MoodleFeed::new(
        "default.course-related",
        MoodleExportKind::CourseRelated,
        url,
    )
    .unwrap()
}
fn secret_safe(error: &dyn Error) {
    let text = format!("{error:?} {error}");
    for marker in [
        "SYNTHETIC_4B1_SECRET",
        "synthetic-password",
        "SAMLResponse",
        "Set-Cookie",
        "?token=",
        "https://",
    ] {
        assert!(!text.contains(marker), "secret in error");
    }
    assert!(error.source().is_none());
}

#[test]
fn moodle_exact_url_and_export_scope_contract() {
    for url in [
        URL,
        "https://moodle.synthetic.invalid:443/calendar/export_execute.php?token=rotated",
    ] {
        policy().validate_moodle_url(url).unwrap();
        assert_eq!(feed(url).scope(), "default.course-related");
        assert_eq!(feed(url).export_kind(), MoodleExportKind::CourseRelated);
    }
    assert_eq!(
        NetworkPolicy::default().validate_moodle_url(URL),
        Err(NetworkError::Policy)
    );
    assert!(
        NetworkPolicy::default()
            .with_moodle_calendar("*.synthetic.invalid", PATH)
            .is_err()
    );
    assert!(
        NetworkPolicy::default()
            .with_moodle_calendar(HOST, "/")
            .is_err()
    );
    assert!(
        NetworkPolicy::default()
            .with_moodle_calendar(HOST, "/calendar/../export")
            .is_err()
    );
}

#[test]
fn strict_url_policy_rejects_disguises_and_special_ip_literals() {
    for url in [
        "https://outside.synthetic.invalid/calendar/export_execute.php",
        "https://moodle.synthetic.invalid.evil.invalid/calendar/export_execute.php",
        "http://moodle.synthetic.invalid/calendar/export_execute.php",
        "https://moodle.synthetic.invalid:444/calendar/export_execute.php",
        "https://student:synthetic-password@moodle.synthetic.invalid/calendar/export_execute.php",
        "https://@moodle.synthetic.invalid/calendar/export_execute.php",
        "https://moodle.synthetic.invalid/calendar/export_execute.php#secret",
        "https://moodle.synthetic.invalid/calendar/export_execute.php/extra",
        "https://moodle.synthetic.invalid/calendar/%65xport_execute.php",
        "https://moodle.synthetic.invalid/calendar/other/../export_execute.php",
        "https://moodle.synthetic.invalid/calendar/%2e%2e/calendar/export_execute.php",
        "https://moodle.synthetic.invalid//calendar/export_execute.php",
        "https://moodle.synthetic.invalid./calendar/export_execute.php",
        "https://%6doodle.synthetic.invalid/calendar/export_execute.php",
        "https://MOODLE.synthetic.invalid/calendar/export_execute.php",
        " https://moodle.synthetic.invalid/calendar/export_execute.php",
        "https://moodle.synthetic.invalid\\calendar/export_execute.php",
        "https://localhost/calendar/export_execute.php",
        "https://127.0.0.1/calendar/export_execute.php",
        "https://2130706433/calendar/export_execute.php",
        "https://0x7f000001/calendar/export_execute.php",
        "https://0177.0.0.1/calendar/export_execute.php",
        "https://10.1.2.3/calendar/export_execute.php",
        "https://169.254.169.254/calendar/export_execute.php",
        "https://100.100.100.200/calendar/export_execute.php",
        "https://[::1]/calendar/export_execute.php",
        "https://[::ffff:127.0.0.1]/calendar/export_execute.php",
        "https://[fe80::1%25en0]/calendar/export_execute.php",
        "https://[fc00::1]/calendar/export_execute.php",
        "https://[2606:4700:4700::1111]/calendar/export_execute.php",
    ] {
        let error = policy().validate_moodle_url(url).unwrap_err();
        secret_safe(&error);
    }
}

#[test]
fn original_url_disguise_cannot_be_lost_by_feed_parser() {
    let client = MoodleClient::for_web(policy()).unwrap();
    for raw in [
        "https://moodle.synthetic.invalid/calendar/x/../export_execute.php",
        "https://@moodle.synthetic.invalid/calendar/export_execute.php",
        "https://%6doodle.synthetic.invalid/calendar/export_execute.php",
    ] {
        assert_eq!(
            client.fetch_calendar(&feed(raw)),
            Err(MoodleHttpError::Request)
        );
    }
}

struct FakeLookup {
    answers: Mutex<VecDeque<Vec<SocketAddr>>>,
    calls: Arc<AtomicUsize>,
}
impl FakeLookup {
    fn new(answers: Vec<Vec<SocketAddr>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Resolve for FakeLookup {
    fn resolve(&self, _: Name) -> Resolving {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            result
                .map(|a| Box::new(a.into_iter()) as Addrs)
                .ok_or_else(|| NetworkError::Policy.into())
        })
    }
}
fn addr(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 0)
}
fn resolve(lookup: Arc<dyn Resolve>, host: &str) -> Result<Vec<SocketAddr>, NetworkError> {
    let resolver = GuardedResolver {
        #[cfg(feature = "synthetic-https-fixture")]
        fixture_peers: Vec::new(),
        hosts: vec![HOST.into()],
        lookup,
        peer: None,
        loopback_origins: false,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(resolver.resolve(host.parse().unwrap()))
        .map(|i| i.collect())
        .map_err(|_| NetworkError::Policy)
}

#[test]
fn dns_returns_only_the_entire_validated_a_aaaa_set_and_no_port_override() {
    let mut addresses = vec![
        addr("8.8.8.8"),
        addr("1.1.1.1"),
        addr("2606:4700:4700::1111"),
    ];
    addresses[0].set_port(9000);
    let lookup = Arc::new(FakeLookup::new(vec![addresses.clone()]));
    let calls = lookup.calls.clone();
    addresses[0].set_port(0);
    assert_eq!(resolve(lookup, HOST).unwrap(), addresses);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn dns_rejects_any_private_reserved_or_mapped_answer_in_a_mixed_set() {
    for ip in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "100.64.0.1",
        "100.100.100.200",
        "127.0.0.1",
        "169.254.1.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "192.0.0.1",
        "192.0.2.1",
        "192.88.99.1",
        "192.31.196.1",
        "192.52.193.1",
        "192.175.48.1",
        "168.63.129.16",
        "198.18.0.1",
        "198.19.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:8.8.8.8",
        "::ffff:127.0.0.1",
        "::8.8.8.8",
        "64:ff9b::808:808",
        "64:ff9b:1::1",
        "100::1",
        "2001::1",
        "2001:20::1",
        "2001:db8::1",
        "2002:808:808::1",
        "2620:4f:8000::1",
        "3ffe::1",
        "3fff::1",
        "fc00::1",
        "fd00:ec2::254",
        "fe80::1",
        "fec0::1",
        "ff02::1",
    ] {
        assert_eq!(
            resolve(
                Arc::new(FakeLookup::new(vec![vec![addr("8.8.8.8"), addr(ip)]])),
                HOST
            ),
            Err(NetworkError::Policy),
            "{ip}"
        );
    }
}

#[test]
fn dns_empty_failed_excessive_and_unknown_hosts_fail_closed_without_fallback() {
    for answers in [
        vec![],
        vec![vec![]],
        vec![vec![addr("8.8.8.8"); MAX_DNS_ANSWERS + 1]],
    ] {
        assert_eq!(
            resolve(Arc::new(FakeLookup::new(answers)), HOST),
            Err(NetworkError::Policy)
        );
    }
    let lookup = Arc::new(FakeLookup::new(vec![vec![addr("8.8.8.8")]]));
    assert_eq!(
        resolve(lookup.clone(), "outside.invalid"),
        Err(NetworkError::Policy)
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 0);
}

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < until => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) => panic!("synthetic server accept failed: {e}"),
        }
    }
}
fn read_headers(stream: &mut impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        assert!(bytes.len() < 16384);
    }
    Ok(String::from_utf8(bytes).unwrap())
}
fn tls_server(
    raw: Vec<u8>,
) -> (
    SocketAddr,
    thread::JoinHandle<(Option<String>, TcpListener)>,
) {
    tls_server_delayed(raw, Duration::ZERO)
}
fn tls_server_delayed(
    raw: Vec<u8>,
    delay: Duration,
) -> (
    SocketAddr,
    thread::JoinHandle<(Option<String>, TcpListener)>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
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
        let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(conn, accept(&listener));
        let request = read_headers(&mut stream).ok();
        if let Some(request) = &request {
            let sni = stream.conn.server_name().expect("TLS client must send SNI");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("host: {sni}\r\n"))
            );
            thread::sleep(delay);
            let _ = stream.write_all(&raw);
            let _ = stream.flush();
        }
        (request, listener)
    });
    (peer, server)
}
fn response(body: &[u8], headers: &str) -> Vec<u8> {
    let mut raw = format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    raw.extend_from_slice(body);
    raw
}
fn tls_policy(peer: SocketAddr, lookup: Arc<dyn Resolve>) -> NetworkPolicy {
    let mut policy = policy();
    policy.test.peer = Some(peer);
    policy.test.lookup = Some(lookup);
    policy.test.root = Some(reqwest::Certificate::from_der(CA).unwrap());
    policy
}
fn assert_no_connection(listener: &TcpListener) {
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn secured_moodle_uses_pinned_dns_tls_sni_host_and_no_referer() {
    let (peer, server) = tls_server(response(b"synthetic calendar", "Content-Length: 18\r\n"));
    let lookup = Arc::new(FakeLookup::new(vec![vec![peer], vec![addr("127.0.0.2")]]));
    let client = MoodleClient::for_web(tls_policy(peer, lookup.clone())).unwrap();
    assert_eq!(
        client.fetch_calendar(&feed(URL)).unwrap(),
        "synthetic calendar"
    );
    let (request, listener) = server.join().unwrap();
    let request = request.unwrap().to_ascii_lowercase();
    assert!(request.contains("host: moodle.synthetic.invalid\r\n"));
    assert!(request.contains("accept-encoding: identity\r\n"));
    assert!(!request.contains("referer:"));
    // The next DNS answer is private. Never connect to it or fall back to the
    // previously checked peer; the first response explicitly closed its socket.
    assert_eq!(
        client.fetch_calendar(&feed(URL)),
        Err(MoodleHttpError::Request)
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 2);
    assert_no_connection(&listener);
}

#[test]
fn tls_untrusted_certificate_and_wrong_hostname_are_rejected() {
    for trust in [false, true] {
        let (peer, server) = tls_server(response(b"ok", "Content-Length: 2\r\n"));
        let mut policy = tls_policy(peer, Arc::new(FakeLookup::new(vec![vec![peer]])));
        let target = if trust {
            policy = policy
                .with_moodle_calendar("wrong.synthetic.invalid", PATH)
                .unwrap();
            URL.replace(HOST, "wrong.synthetic.invalid")
        } else {
            policy.test.root = None;
            URL.into()
        };
        let error = MoodleClient::for_web(policy)
            .unwrap()
            .fetch_calendar(&feed(&target))
            .unwrap_err();
        assert_eq!(error, MoodleHttpError::Request);
        secret_safe(&error);
        assert!(server.join().unwrap().0.is_none());
    }
}

#[test]
fn moodle_same_and_cross_origin_redirects_are_never_followed() {
    for location in [URL, "https://outside.invalid/?token=SYNTHETIC_4B1_SECRET"] {
        let raw = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes();
        let (peer, server) = tls_server(raw);
        let lookup = Arc::new(FakeLookup::new(vec![vec![peer]]));
        let error = MoodleClient::for_web(tls_policy(peer, lookup.clone()))
            .unwrap()
            .fetch_calendar(&feed(URL))
            .unwrap_err();
        assert_eq!(
            error,
            MoodleHttpError::HttpStatus(reqwest::StatusCode::FOUND)
        );
        secret_safe(&error);
        let (_, listener) = server.join().unwrap();
        assert_no_connection(&listener);
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    }
}

fn bounded_raw(raw: Vec<u8>, kind: BodyKind) -> Result<String, NetworkError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_headers(&mut stream).unwrap();
        let _ = stream.write_all(&raw);
    });
    let transport = Transport::desktop(
        Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .unwrap(),
    );
    let result = read_text(transport.send(transport.get(url)).unwrap(), kind);
    server.join().unwrap();
    result
}

#[test]
fn all_body_kinds_accept_small_and_exact_limit_bodies_and_reject_oversized_without_length() {
    for kind in [
        BodyKind::MoodleIcs,
        BodyKind::WebClassJson,
        BodyKind::AuthHtml,
        BodyKind::OpenAmJson,
    ] {
        assert_eq!(bounded_raw(response(b"ok", ""), kind).unwrap(), "ok");
        assert_eq!(
            bounded_raw(response(&vec![b'x'; kind.limit()], ""), kind)
                .unwrap()
                .len(),
            kind.limit()
        );
        assert_eq!(
            bounded_raw(response(&vec![b'x'; kind.limit() + 1], ""), kind),
            Err(NetworkError::BodyTooLarge)
        );
    }
}

#[test]
fn oversized_content_length_chunked_and_forged_length_are_safe_failures() {
    assert_eq!(
        bounded_raw(
            response(b"", "Content-Length: 999999999\r\n"),
            BodyKind::AuthHtml
        ),
        Err(NetworkError::BodyTooLarge)
    );
    let body = vec![b'x'; BodyKind::OpenAmJson.limit() + 1];
    let mut chunks = format!("{:x}\r\n", body.len()).into_bytes();
    chunks.extend(body);
    chunks.extend(b"\r\n0\r\n\r\n");
    assert_eq!(
        bounded_raw(
            response(&chunks, "Transfer-Encoding: chunked\r\n"),
            BodyKind::OpenAmJson
        ),
        Err(NetworkError::BodyTooLarge)
    );
    // HTTP framing defines a small Content-Length as the end of this response;
    // surplus bytes cannot become parser input. Never claim to count them as body.
    assert_eq!(
        bounded_raw(
            response(b"okSURPLUS", "Content-Length: 2\r\n"),
            BodyKind::AuthHtml
        )
        .unwrap(),
        "ok"
    );
    assert_eq!(
        bounded_raw(
            response(b"short", "Content-Length: 30\r\n"),
            BodyKind::AuthHtml
        ),
        Err(NetworkError::BodyRead)
    );
}

#[test]
fn compressed_expansion_is_rejected_before_any_decompression() {
    // A gzip stream with high expansion potential. Encoding itself is forbidden;
    // auto-decoders stay disabled even if Cargo features are unified by a consumer.
    for encoding in ["gzip", "br", "deflate", "zstd", "gzip, br", "unknown"] {
        let error = bounded_raw(
            response(
                include_bytes!("../fixtures/network/expansion.gz"),
                &format!("Content-Encoding: {encoding}\r\n"),
            ),
            BodyKind::AuthHtml,
        )
        .unwrap_err();
        assert_eq!(error, NetworkError::Encoding);
        secret_safe(&error);
    }
}

#[test]
fn invalid_utf8_and_truncated_chunked_body_have_no_secret_cause() {
    for (raw, expected) in [
        (
            response(&[0xff], "Content-Length: 1\r\n"),
            NetworkError::Decode,
        ),
        (
            response(b"10\r\nshort", "Transfer-Encoding: chunked\r\n"),
            NetworkError::BodyRead,
        ),
    ] {
        let error = bounded_raw(raw, BodyKind::AuthHtml).unwrap_err();
        assert_eq!(error, expected);
        secret_safe(&error);
    }
}

#[test]
fn webclass_origin_policy_and_cookie_partition_reject_cross_origin_leakage() {
    let policy = NetworkPolicy::default();
    let jar = SessionCookies::new(true);
    let idp = Url::parse("https://idp.hosei.ac.jp/openam/").unwrap();
    jar.add_cookie_str(
        "synthetic=SYNTHETIC_4B1_SECRET; Domain=.hosei.ac.jp; Secure; Path=/",
        &idp,
    );
    assert!(jar.cookies(&idp).unwrap().is_sensitive());
    for origin in [HOPPII_ORIGIN, WEBCLASS_ORIGIN] {
        let url = Url::parse(&format!("{origin}/")).unwrap();
        policy.validate(&url, Source::WebClass).unwrap();
        assert!(jar.cookies(&url).is_none());
    }
    for raw in [
        "https://unknown.hosei.ac.jp/",
        "https://idp.hosei.ac.jp.evil.invalid/",
        "http://idp.hosei.ac.jp/",
        "https://idp.hosei.ac.jp:444/",
        "https://idp.hosei.ac.jp/#fragment",
    ] {
        assert_eq!(
            policy.validate(&Url::parse(raw).unwrap(), Source::WebClass),
            Err(NetworkError::Policy)
        );
        let url = Url::parse(raw).unwrap();
        if url.origin() != idp.origin() {
            assert!(jar.cookies(&url).is_none());
        }
    }
}

#[test]
fn transport_checks_built_destination_before_resolver_or_cookie_password_send() {
    let lookup = Arc::new(FakeLookup::new(vec![]));
    let mut policy = policy();
    policy.test.lookup = Some(lookup.clone());
    let http = Transport::secured(
        policy,
        Source::WebClass,
        Some(Arc::new(SessionCookies::new(true))),
    )
    .unwrap();
    for raw in [
        URL,
        "https://outside.invalid/",
        "https://127.0.0.1/",
        "http://idp.hosei.ac.jp/",
        "https://idp.hosei.ac.jp:444/",
    ] {
        let error = http
            .send(
                http.post(Url::parse(raw).unwrap())
                    .form(&[("password", "synthetic-password")]),
            )
            .unwrap_err();
        assert_eq!(error, NetworkError::Policy);
        secret_safe(&error);
    }
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn web_referer_query_and_fragment_are_removed_before_sending() {
    let (peer, server) = tls_server(response(b"ok", "Content-Length: 2\r\n"));
    let http = Transport::secured(
        tls_policy(peer, Arc::new(FakeLookup::new(vec![vec![peer]]))),
        Source::Moodle,
        None,
    )
    .unwrap();
    let response = http
        .send(http.get(Url::parse(URL).unwrap()).header(
            REFERER,
            "https://moodle.synthetic.invalid/page?token=SYNTHETIC_4B1_SECRET#secret",
        ))
        .unwrap();
    assert_eq!(read_text(response, BodyKind::AuthHtml).unwrap(), "ok");
    let request = server.join().unwrap().0.unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("referer: https://moodle.synthetic.invalid/page\r\n")
    );
}

#[test]
fn environment_proxy_cannot_bypass_dns_policy() {
    const CHILD: &str = "AM_SYNTHETIC_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let lookup = Arc::new(FakeLookup::new(vec![vec![addr("127.0.0.1")]]));
        let mut policy = policy();
        policy.test.lookup = Some(lookup.clone());
        let error = MoodleClient::for_web(policy)
            .unwrap()
            .fetch_calendar(&feed(URL))
            .unwrap_err();
        assert_eq!(error, MoodleHttpError::Request);
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
        return;
    }
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", trap.local_addr().unwrap());
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "university_network::tests::environment_proxy_cannot_bypass_dns_policy",
        ])
        .env(CHILD, "1")
        .env("HTTP_PROXY", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("https_proxy", &proxy)
        .env("all_proxy", &proxy)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_no_connection(&trap);
}

#[test]
fn dns_deadline_fails_closed() {
    struct PendingLookup;
    impl Resolve for PendingLookup {
        fn resolve(&self, _: Name) -> Resolving {
            Box::pin(std::future::pending())
        }
    }
    let started = Instant::now();
    assert_eq!(
        resolve(Arc::new(PendingLookup), HOST),
        Err(NetworkError::Policy)
    );
    assert!(started.elapsed() < Duration::from_secs(12));
}

#[test]
fn secured_connect_tls_deadline_expires_without_sending_http_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = listener.local_addr().unwrap();
    let (release, waiting) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut prefix = [0; 5];
        stream.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix[0], 0x16); // TLS ClientHello, never an HTTP credential request
        waiting.recv_timeout(Duration::from_secs(20)).unwrap();
    });
    let client = MoodleClient::for_web(tls_policy(
        peer,
        Arc::new(FakeLookup::new(vec![vec![peer]])),
    ))
    .unwrap();
    let started = Instant::now();
    let error = client.fetch_calendar(&feed(URL)).unwrap_err();
    release.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(error, MoodleHttpError::Timeout);
    secret_safe(&error);
    assert!(started.elapsed() < Duration::from_secs(12));
}

#[test]
fn secured_header_and_body_reads_have_finite_timeouts_and_safe_errors() {
    for body_timeout in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (release, waiting) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            read_headers(&mut stream).unwrap();
            if body_timeout {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 30\r\n\r\n")
                    .unwrap();
            }
            waiting.recv_timeout(Duration::from_secs(20)).unwrap();
        });
        let mut policy = NetworkPolicy::default();
        policy.test.origins = vec![url.clone()];
        let http = Transport::secured(policy, Source::WebClass, None).unwrap();
        let started = Instant::now();
        let result = http.send(http.get(url));
        let error = if body_timeout {
            read_text(result.unwrap(), BodyKind::AuthHtml).unwrap_err()
        } else {
            result.unwrap_err()
        };
        release.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(error, NetworkError::Timeout);
        secret_safe(&error);
        assert!(started.elapsed() < Duration::from_secs(12));
    }
}

#[test]
fn secured_total_deadline_stops_a_body_that_keeps_sending_small_chunks() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (release, waiting) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_headers(&mut stream).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
            .unwrap();
        for _ in 0..6 {
            if waiting.recv_timeout(Duration::from_secs(3)).is_ok() {
                return;
            }
            if stream.write_all(b"x").is_err() {
                return;
            }
        }
    });
    let mut policy = NetworkPolicy::default();
    policy.test.origins = vec![url.clone()];
    let http = Transport::secured(policy, Source::WebClass, None).unwrap();
    let started = Instant::now();
    let error = read_text(http.send(http.get(url)).unwrap(), BodyKind::AuthHtml).unwrap_err();
    let _ = release.send(());
    server.join().unwrap();
    assert_eq!(error, NetworkError::Timeout);
    secret_safe(&error);
    assert!(started.elapsed() >= Duration::from_secs(12));
    assert!(started.elapsed() < Duration::from_secs(22));
}

#[test]
fn whole_run_request_concurrency_group_record_deadline_and_byte_limits_fail_closed() {
    let run = Arc::new(RunBudget::default());
    assert!(run.acquire().is_err()); // no admitted run
    run.begin();
    let first = run.acquire().unwrap();
    assert!(run.acquire().is_err()); // response lease owns the only slot
    drop(first);
    for _ in 1..RUN_REQUESTS {
        drop(run.acquire().unwrap());
    }
    assert!(run.acquire().is_err());
    run.begin();
    run.checkpoint(MAX_COURSES, MAX_COURSES, false).unwrap();
    assert!(run.checkpoint(MAX_COURSES + 1, MAX_COURSES, false).is_err());
    run.checkpoint(MAX_RECORDS, MAX_RECORDS, false).unwrap();
    assert!(run.checkpoint(MAX_RECORDS + 1, MAX_RECORDS, false).is_err());
    for _ in 0..RUN_GROUPS {
        run.checkpoint(0, 0, true).unwrap();
    }
    assert!(run.checkpoint(0, 0, true).is_err());
    run.begin();
    run.0.lock().unwrap().bytes = RUN_BYTES;
    assert!(run.acquire().is_err());
    run.begin();
    run.0.lock().unwrap().started = Some(Instant::now() - RUN_TIME);
    assert!(run.acquire().is_err());
    assert!(run.checkpoint(0, 0, false).is_err());
    run.begin();
    drop(run.acquire().unwrap());
}

#[test]
fn cumulative_bytes_and_whole_run_deadline_bound_actual_tls_body_reads() {
    for deadline in [false, true] {
        let (peer, server) = tls_server(response(b"12345", "Content-Length: 5\r\n"));
        let policy = tls_policy(peer, Arc::new(FakeLookup::new(vec![vec![peer]])));
        let run = Arc::new(RunBudget::default());
        run.begin();
        let mut http = Transport::secured(policy, Source::Moodle, None).unwrap();
        http.set_run_budget(Some(run.clone()));
        let response = http.send(http.get(Url::parse(URL).unwrap())).unwrap();
        if deadline {
            run.0.lock().unwrap().started = Some(Instant::now() - RUN_TIME);
        } else {
            run.0.lock().unwrap().bytes = RUN_BYTES - 2;
        }
        assert!(read_text(response, BodyKind::MoodleIcs).is_err());
        server.join().unwrap();
        assert!(!run.0.lock().unwrap().active);
    }
}

#[test]
fn web_cookie_budget_never_evicts_isolation_or_grows_unbounded() {
    let url = Url::parse(IDP_ORIGIN).unwrap();
    for mode in 0..4 {
        let cookies = SessionCookies::new(true);
        match mode {
            0 => {
                for n in 0..1025 {
                    cookies.add_cookie_str(&format!("c{n}=synthetic; Path=/"), &url);
                }
            }
            1 => cookies.add_cookie_str(&format!("a={}", "x".repeat(4096)), &url),
            2 => {
                for n in 0..100 {
                    cookies.add_cookie_str(&format!("c{n}={}", "x".repeat(4090)), &url);
                }
            }
            _ => {
                for n in 0..4 {
                    cookies.add_cookie_str(
                        "a=b",
                        &Url::parse(&format!("https://origin{n}.synthetic.invalid")).unwrap(),
                    );
                }
            }
        }
        assert!(cookies.exhausted());
        let usage = *cookies.usage.lock().unwrap();
        assert!(usage.0 <= 1024 && usage.1 <= 256 * 1024);
        assert!(cookies.origins.lock().unwrap().len() <= 3);
        cookies.add_cookie_str("late=secret", &url);
        assert_eq!(usage, *cookies.usage.lock().unwrap());
    }
}

#[test]
fn remaining_run_time_clamps_a_real_tls_request_timeout() {
    let (peer, server) = tls_server_delayed(
        response(b"ok", "Content-Length: 2\r\n"),
        Duration::from_millis(600),
    );
    let policy = tls_policy(peer, Arc::new(FakeLookup::new(vec![vec![peer]])));
    let run = Arc::new(RunBudget::default());
    let mut http = Transport::secured(policy, Source::Moodle, None).unwrap();
    http.set_run_budget(Some(run.clone()));
    run.begin();
    run.0.lock().unwrap().started = Some(Instant::now() - (RUN_TIME - Duration::from_millis(300)));
    let start = Instant::now();
    assert_eq!(
        http.send(http.get(Url::parse(URL).unwrap())).unwrap_err(),
        NetworkError::Timeout
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert!(!run.0.lock().unwrap().active);
}

#[test]
fn secured_sync_private_dns_rejection_never_connects_to_fixture_tripwire() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    let lookup = Arc::new(FakeLookup::new(vec![vec![trap.local_addr().unwrap()]]));
    let mut policy = policy();
    policy.test.lookup = Some(lookup.clone());
    let mut clients = crate::secured_sync::SecuredSyncClients::new(policy);
    clients.begin_run();
    let mut store = crate::persistence::SqliteStore::open(":memory:").unwrap();
    assert_eq!(
        crate::sync::sync_moodle_once(&clients.moodle().unwrap(), &feed(URL), &mut store),
        Err(crate::sync::MoodleSyncError::Fetch)
    );
    assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
    assert_no_connection(&trap);
}
