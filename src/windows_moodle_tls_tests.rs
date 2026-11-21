use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    thread,
    time::Duration,
};

use aws_lc_rs::{
    digest::{SHA256, digest},
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::{
    Url,
    dns::{Name, Resolve, Resolving},
};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};

use crate::university_network::{NetworkPolicy, RequestFailure, Source, Transport};

const HOST: &str = "moodle-crl.synthetic.invalid";
const CALENDAR: &str = "/calendar/export_execute.php";

pub(super) struct OwnedTestEngine {
    pub(super) engine: HCERTCHAINENGINE,
    _roots: MemoryStore,
}

impl Drop for OwnedTestEngine {
    fn drop(&mut self) {
        unsafe {
            CertFreeCertificateChainEngine(self.engine);
        }
    }
}

#[derive(Clone)]
pub(super) struct TestTrust {
    root: Vec<u8>,
    issuer_crl: Option<Vec<u8>>,
}

impl TestTrust {
    pub(super) fn engine(&self, store: &MemoryStore) -> Result<OwnedTestEngine, MoodleCrlError> {
        let roots = MemoryStore::new()?;
        roots.add_certificate(&self.root, false)?;
        if let Some(crl) = &self.issuer_crl {
            store.add_crl(crl)?;
        }
        let config = CERT_CHAIN_ENGINE_CONFIG {
            cbSize: size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32,
            dwFlags: CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
            hExclusiveRoot: roots.0,
            ..Default::default()
        };
        let mut engine = null_mut();
        if unsafe { CertCreateCertificateChainEngine(&config, &mut engine) } == 0
            || engine.is_null()
        {
            return Err(MoodleCrlError::Evidence);
        }
        Ok(OwnedTestEngine {
            engine,
            _roots: roots,
        })
    }
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if content.len() < 128 {
        out.push(content.len() as u8);
    } else {
        let width = content.len().to_be_bytes();
        let start = width.iter().position(|byte| *byte != 0).unwrap();
        out.push(0x80 | (width.len() - start) as u8);
        out.extend_from_slice(&width[start..]);
    }
    out.extend_from_slice(content);
    out
}

fn fields(tag: u8, values: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    der(tag, &values.into_iter().flatten().collect::<Vec<_>>())
}

fn seq(values: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    fields(0x30, values)
}

fn oid(bytes: &[u8]) -> Vec<u8> {
    der(0x06, bytes)
}
fn integer(byte: u8) -> Vec<u8> {
    der(0x02, &[byte])
}

fn bits(unused: u8, bytes: &[u8]) -> Vec<u8> {
    der(0x03, &[&[unused], bytes].concat())
}

fn signing_algorithm() -> Vec<u8> {
    seq([oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])])
}

fn timestamp(value: DateTime<Utc>) -> Vec<u8> {
    der(0x18, value.format("%Y%m%d%H%M%SZ").to_string().as_bytes())
}

fn extension_value(id: u8, critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(&[0x55, 0x1d, id])];
    if critical {
        parts.push(der(0x01, &[0xff]));
    }
    parts.push(der(0x04, &value));
    seq(parts)
}

fn distribution(url: &str) -> Vec<u8> {
    der(0xa0, &der(0xa0, &der(0x86, url.as_bytes())))
}

struct Key {
    signing: EcdsaKeyPair,
    pkcs8: Vec<u8>,
    id: Vec<u8>,
    subject: Vec<u8>,
}

impl Key {
    fn new(label: &str) -> Self {
        let material =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
                .unwrap();
        let pkcs8 = material.as_ref().to_vec();
        let signing = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8).unwrap();
        let id = digest(&SHA256, signing.public_key().as_ref()).as_ref()[..20].to_vec();
        let cn = format!(
            "{label}-{:02x}{:02x}{:02x}{:02x}",
            id[0], id[1], id[2], id[3]
        );
        let subject = seq([fields(
            0x31,
            [seq([oid(&[0x55, 0x04, 3]), der(0x0c, cn.as_bytes())])],
        )]);
        Self {
            signing,
            pkcs8,
            id,
            subject,
        }
    }

    fn signed(&self, body: Vec<u8>) -> Vec<u8> {
        let signature = self.signing.sign(&SystemRandom::new(), &body).unwrap();
        seq([body, signing_algorithm(), bits(0, signature.as_ref())])
    }

    fn authority_key(&self) -> Vec<u8> {
        extension_value(35, false, seq([der(0x80, &self.id)]))
    }
}

fn certificate(
    key: &Key,
    issuer: &Key,
    serial: u8,
    now: DateTime<Utc>,
    crl: Option<&str>,
    ca: bool,
) -> Vec<u8> {
    let mut exts = vec![
        extension_value(
            19,
            true,
            if ca {
                seq([der(0x01, &[0xff])])
            } else {
                seq([])
            },
        ),
        extension_value(
            15,
            true,
            if ca {
                bits(1, &[0x06])
            } else {
                bits(7, &[0x80])
            },
        ),
        extension_value(14, false, der(0x04, &key.id)),
        issuer.authority_key(),
    ];
    if !ca {
        exts.push(extension_value(
            17,
            false,
            seq([der(0x82, HOST.as_bytes())]),
        ));
        exts.push(extension_value(
            37,
            false,
            seq([oid(&[0x2b, 6, 1, 5, 5, 7, 3, 1])]),
        ));
    }
    if let Some(crl) = crl {
        exts.push(extension_value(31, false, seq([seq([distribution(crl)])])));
    }
    let spki = seq([
        seq([
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        bits(0, key.signing.public_key().as_ref()),
    ]);
    issuer.signed(seq([
        der(0xa0, &integer(2)),
        integer(serial),
        signing_algorithm(),
        issuer.subject.clone(),
        seq([
            timestamp(now - TimeDelta::days(2)),
            timestamp(now + TimeDelta::days(3)),
        ]),
        key.subject.clone(),
        spki,
        der(0xa3, &seq(exts)),
    ]))
}

fn crl(
    key: &Key,
    url: &str,
    issuer_certs: bool,
    now: DateTime<Utc>,
    expired: bool,
    revoked: Option<u8>,
) -> Vec<u8> {
    let from = now - TimeDelta::days(2);
    let until = if expired {
        now - TimeDelta::hours(1)
    } else {
        now + TimeDelta::days(1)
    };
    let mut values = vec![
        integer(1),
        signing_algorithm(),
        key.subject.clone(),
        timestamp(from),
        timestamp(until),
    ];
    if let Some(serial) = revoked {
        values.push(seq([seq([integer(serial), timestamp(from)])]));
    }
    let idp = seq([
        distribution(url),
        der(if issuer_certs { 0x82 } else { 0x81 }, &[0xff]),
    ]);
    values.push(der(
        0xa0,
        &seq([
            key.authority_key(),
            extension_value(20, false, integer(1)),
            extension_value(28, true, idp),
        ]),
    ));
    key.signed(seq(values))
}

struct Fixture {
    now: DateTime<Utc>,
    key: Key,
    root: Key,
    other: Key,
    leaf: Vec<u8>,
    leaf_key: Key,
    issuer: Vec<u8>,
    trusted_root: Vec<u8>,
    leaf_dp: String,
    issuer_dp: String,
}

impl Fixture {
    fn new(origin: &str) -> Self {
        let now = Utc::now();
        let root = Key::new("offline root");
        let key = Key::new("offline issuer");
        let other = Key::new("different issuer");
        let leaf_key = Key::new("offline leaf");
        let leaf_dp = format!("{origin}/42.crl");
        let issuer_dp = format!("{origin}/issuer.crl");
        let leaf = certificate(&leaf_key, &key, 3, now, Some(&leaf_dp), false);
        let issuer = certificate(&key, &root, 2, now, Some(&issuer_dp), true);
        let trusted_root = certificate(&root, &root, 1, now, None, true);
        Self {
            now,
            key,
            root,
            other,
            leaf,
            leaf_key,
            issuer,
            trusted_root,
            leaf_dp,
            issuer_dp,
        }
    }

    fn evidence(&self, blob: Vec<u8>) -> CrlEvidence {
        CrlEvidence {
            host: HOST.into(),
            leaf: self.leaf.clone().into(),
            issuer: self.issuer.clone().into(),
            crl: blob,
            test_trust: Some(TestTrust {
                root: self.trusted_root.clone(),
                issuer_crl: Some(crl(
                    &self.root,
                    &self.issuer_dp,
                    true,
                    self.now,
                    false,
                    None,
                )),
            }),
        }
    }

    fn good(&self) -> Vec<u8> {
        crl(&self.key, &self.leaf_dp, false, self.now, false, None)
    }

    fn original_server(&self) -> (Vec<u8>, Vec<u8>) {
        (self.leaf.clone(), self.leaf_key.pkcs8.clone())
    }

    fn rotated_server(&self) -> (Vec<u8>, Vec<u8>) {
        let key = Key::new("rotated leaf");
        (
            certificate(&key, &self.key, 6, self.now, Some(&self.leaf_dp), false),
            key.pkcs8,
        )
    }
}

struct LocalPeer(SocketAddr);
impl Resolve for LocalPeer {
    fn resolve(&self, _name: Name) -> Resolving {
        let peer = self.0;
        Box::pin(async move { Ok(Box::new([peer].into_iter()) as reqwest::dns::Addrs) })
    }
}

fn round_trip(
    fixture: &Fixture,
    evidence: CrlEvidence,
    pair: (Vec<u8>, Vec<u8>),
    hostname: &str,
    omit_issuer: bool,
) -> (Result<u16, RequestFailure>, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();
    let mut chain = vec![pair.0.into()];
    if !omit_issuer {
        chain.push(fixture.issuer.clone().into());
    }
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pair.1));
    let server = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(Arc::new(server)).unwrap(), socket);
        let mut header = Vec::new();
        if BufReader::new(&mut tls)
            .read_until(b'\n', &mut header)
            .is_ok_and(|size| size > 0)
        {
            assert!(header.starts_with(b"GET /calendar/export_execute.php "));
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            tls.flush().unwrap();
            1
        } else {
            0
        }
    });

    let origin = Url::parse(&format!("https://{hostname}:{}", local.port())).unwrap();
    let target = origin.join(CALENDAR).unwrap();
    let mut policy = NetworkPolicy::default()
        .with_moodle_calendar(hostname, CALENDAR)
        .unwrap();
    policy.test.origins = vec![origin];
    policy.test.peer = Some(local);
    policy.test.lookup = Some(Arc::new(LocalPeer(local)));
    policy.moodle_windows_crl = Some(Arc::new(evidence));
    let transport = Transport::secured(policy, Source::Moodle, None).unwrap();
    let result = transport
        .send_with_detail(transport.get(target))
        .map(|response| response.status().as_u16())
        .map_err(|(_, failure)| failure);
    (result, handle.join().unwrap())
}

#[test]
fn windows_crl_tls_gate_only_sends_http_after_all_checks() {
    // CRL URLs point only to this local tripwire, including on error paths.
    let tripwire = TcpListener::bind("127.0.0.1:0").unwrap();
    let control = TcpStream::connect(tripwire.local_addr().unwrap()).unwrap();
    drop((control, tripwire.accept().unwrap()));
    tripwire.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", tripwire.local_addr().unwrap());
    let fixture = Fixture::new(&origin);
    let mut corrupt = fixture.good();
    *corrupt.last_mut().unwrap() ^= 1;
    let mut trailing = fixture.good();
    trailing.push(0);
    let cases = [
        ("valid", fixture.good(), Ok(()), Ok(200), 1),
        (
            "expired",
            crl(
                &fixture.key,
                &fixture.leaf_dp,
                false,
                fixture.now,
                true,
                None,
            ),
            Err(MoodleCrlError::CrlTime),
            Err(RequestFailure::TlsRevocation),
            0,
        ),
        (
            "issuer",
            crl(
                &fixture.other,
                &fixture.leaf_dp,
                false,
                fixture.now,
                false,
                None,
            ),
            Err(MoodleCrlError::Issuer),
            Err(RequestFailure::TlsRevocation),
            0,
        ),
        (
            "partition",
            crl(
                &fixture.key,
                &format!("{origin}/11.crl"),
                false,
                fixture.now,
                false,
                None,
            ),
            Err(MoodleCrlError::DistributionPoint),
            Err(RequestFailure::TlsRevocation),
            0,
        ),
        (
            "signature",
            corrupt,
            Err(MoodleCrlError::Signature),
            Err(RequestFailure::TlsRevocation),
            0,
        ),
        (
            "trailing",
            trailing,
            Err(MoodleCrlError::Evidence),
            Err(RequestFailure::TlsRevocation),
            0,
        ),
        (
            "revoked",
            crl(
                &fixture.key,
                &fixture.leaf_dp,
                false,
                fixture.now,
                false,
                Some(3),
            ),
            Err(MoodleCrlError::Revoked),
            Err(RequestFailure::TlsRevoked),
            0,
        ),
    ];
    for (case, blob, validation, result, requests) in cases {
        let name = ServerName::try_from(HOST).unwrap();
        let evidence = fixture.evidence(blob);
        assert_eq!(
            evidence.check_material(&name, UnixTime::now()),
            validation,
            "{case}"
        );
        assert_eq!(
            round_trip(&fixture, evidence, fixture.original_server(), HOST, false),
            (result, requests),
            "{case}"
        );
    }
    assert_eq!(
        round_trip(
            &fixture,
            fixture.evidence(fixture.good()),
            fixture.rotated_server(),
            HOST,
            false
        ),
        (Err(RequestFailure::TlsCertificateChanged), 0)
    );
    assert_eq!(
        round_trip(
            &fixture,
            fixture.evidence(fixture.good()),
            fixture.original_server(),
            "wrong.synthetic.invalid",
            false
        ),
        (Err(RequestFailure::TlsHostname), 0)
    );
    // A fresh PKI prevents a cached issuer from satisfying the deliberately incomplete chain.
    let incomplete = Fixture::new(&origin);
    assert_eq!(
        round_trip(
            &incomplete,
            incomplete.evidence(incomplete.good()),
            incomplete.original_server(),
            HOST,
            true
        ),
        (Err(RequestFailure::TlsCertificateChain), 0)
    );
    let cache_missing = Fixture::new(&origin);
    let mut evidence = cache_missing.evidence(cache_missing.good());
    evidence.test_trust.as_mut().unwrap().issuer_crl = None;
    assert_eq!(
        round_trip(
            &cache_missing,
            evidence,
            cache_missing.original_server(),
            HOST,
            false
        ),
        (Err(RequestFailure::TlsRevocation), 0)
    );
    assert!(tripwire.accept().unwrap_err().kind() == std::io::ErrorKind::WouldBlock);
}

#[test]
#[ignore = "read explicitly selected local evidence; no network or system store writes"]
fn windows_saved_moodle_crl_offline() {
    let path = |name| PathBuf::from(std::env::var_os(name).expect("set the public evidence path"));
    let host = "cms.cis.k.hosei.ac.jp";
    let evidence = CrlEvidence::load(
        host,
        &path("AM_MOODLE_CRL_TEST_LEAF"),
        &path("AM_MOODLE_CRL_TEST_ISSUER"),
        &path("AM_MOODLE_CRL_TEST_CRL"),
    )
    .unwrap();
    println!("windows_saved_pair_crl=verified");
    let peer = evidence.check_peer(
        &evidence.leaf,
        std::slice::from_ref(&evidence.issuer),
        &ServerName::try_from(host).unwrap(),
        UnixTime::now(),
    );
    match peer {
        Ok(()) => println!("windows_saved_cached_chain=verified"),
        Err(MoodleCrlError::CertificateChain | MoodleCrlError::RevocationUnconfirmed) => println!(
            "windows_saved_cached_chain={}",
            peer.unwrap_err().diagnostic()
        ),
        Err(error) => panic!("unexpected fixed diagnostic: {error}"),
    }
}
