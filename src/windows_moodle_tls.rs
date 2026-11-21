//! Windows Moodle revocation evidence. All CryptoAPI stores here are in memory.

use std::{
    collections::HashSet,
    ffi::CStr,
    fmt,
    fs::File,
    io::Read,
    mem::size_of,
    path::Path,
    ptr::{null, null_mut},
    sync::Arc,
};

use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::{
        Resumption,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use windows_sys::Win32::{Foundation::FILETIME, Security::Cryptography::*};

const MAX_CERT_BYTES: usize = 64 * 1024;
const MAX_CRL_BYTES: usize = 8 * 1024 * 1024;
const ENCODING: u32 = X509_ASN_ENCODING;
const OFFLINE_FLAGS: u32 = CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL
    | CERT_CHAIN_DISABLE_AIA
    | CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE
    | CERT_CHAIN_REVOCATION_CHECK_CACHE_ONLY
    | CERT_CHAIN_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT;
const TIME_ERRORS: u32 = CERT_TRUST_IS_NOT_TIME_VALID | CERT_TRUST_IS_NOT_TIME_NESTED;
const REVOCATION_ERRORS: u32 =
    CERT_TRUST_REVOCATION_STATUS_UNKNOWN | CERT_TRUST_IS_OFFLINE_REVOCATION;
const CERTIFICATE_ISSUER: &CStr = c"2.5.29.29";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoodleCrlError {
    Evidence,
    Issuer,
    DistributionPoint,
    Signature,
    CrlTime,
    Revoked,
    CertificateChanged,
    CertificateName,
    CertificateTime,
    CertificateChain,
    RevocationUnconfirmed,
}

impl MoodleCrlError {
    pub fn diagnostic(self) -> &'static str {
        match self {
            Self::Revoked => "tls_certificate_revoked",
            Self::CertificateChanged => "tls_certificate_changed",
            Self::CertificateName => "tls_hostname",
            Self::CertificateTime => "tls_certificate_time",
            Self::CertificateChain => "tls_certificate_chain",
            _ => "tls_revocation_unconfirmed",
        }
    }

    fn tls_error(self) -> Error {
        Error::InvalidCertificate(match self {
            Self::Revoked => CertificateError::Revoked,
            Self::CertificateChanged => CertificateError::Other(rustls::OtherError(Arc::new(self))),
            Self::CertificateName => CertificateError::NotValidForName,
            Self::CertificateTime => CertificateError::Expired,
            Self::CertificateChain => CertificateError::UnknownIssuer,
            Self::CrlTime => CertificateError::ExpiredRevocationList,
            _ => CertificateError::UnknownRevocationStatus,
        })
    }
}

impl fmt::Display for MoodleCrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.diagnostic())
    }
}

impl std::error::Error for MoodleCrlError {}

#[derive(Clone)]
pub(crate) struct CrlEvidence {
    host: String,
    leaf: CertificateDer<'static>,
    issuer: CertificateDer<'static>,
    crl: Vec<u8>,
    #[cfg(test)]
    test_trust: Option<tests::TestTrust>,
}

impl CrlEvidence {
    pub(crate) fn load(
        host: &str,
        leaf: &Path,
        issuer: &Path,
        crl: &Path,
    ) -> Result<Self, MoodleCrlError> {
        let evidence = Self {
            host: host.to_owned(),
            leaf: CertificateDer::from(read_bounded(leaf, MAX_CERT_BYTES)?),
            issuer: CertificateDer::from(read_bounded(issuer, MAX_CERT_BYTES)?),
            crl: read_bounded(crl, MAX_CRL_BYTES)?,
            #[cfg(test)]
            test_trust: None,
        };
        let name = ServerName::try_from(host).map_err(|_| MoodleCrlError::CertificateName)?;
        evidence.check_material(&name, UnixTime::now())?;
        Ok(evidence)
    }

    pub(crate) fn client_config(self: Arc<Self>) -> Result<ClientConfig, rustls::Error> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Arc::new(Verifier {
            evidence: self,
            provider: provider.clone(),
        });
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.resumption = Resumption::disabled();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }

    fn check_material(&self, name: &ServerName<'_>, now: UnixTime) -> Result<(), MoodleCrlError> {
        let ServerName::DnsName(dns) = name else {
            return Err(MoodleCrlError::CertificateName);
        };
        if dns.as_ref() != self.host {
            return Err(MoodleCrlError::CertificateName);
        }
        let parsed = rustls::server::ParsedCertificate::try_from(&self.leaf)
            .map_err(|_| MoodleCrlError::CertificateChain)?;
        rustls::client::verify_server_name(&parsed, name)
            .map_err(|_| MoodleCrlError::CertificateName)?;
        let leaf = Certificate::decode(&self.leaf)?;
        let issuer = Certificate::decode(&self.issuer)?;
        let time = filetime(now);
        if !leaf.valid_at(&time) || !issuer.valid_at(&time) {
            return Err(MoodleCrlError::CertificateTime);
        }
        let leaf_info = leaf.info()?;
        let issuer_info = issuer.info()?;
        let leaf_ext = extensions(leaf_info.rgExtension, leaf_info.cExtension, leaf_info)?;
        let issuer_ext = extensions(issuer_info.rgExtension, issuer_info.cExtension, issuer_info)?;
        let issuer_key = extension(issuer_ext, c"2.5.29.14")
            .and_then(|ext| blob(&ext.Value))
            .and_then(|value| one_der(value, 0x04))
            .filter(|key| !key.is_empty())
            .ok_or(MoodleCrlError::Issuer)?;
        let mut usage = 0;
        if blob(&leaf_info.Issuer) != blob(&issuer_info.Subject)
            || authority_key(leaf_ext) != Some(issuer_key)
            || is_ca(leaf_ext) != Some(false)
            || is_ca(issuer_ext) != Some(true)
            || extension(issuer_ext, c"2.5.29.15").is_none()
            || unsafe { CertGetIntendedKeyUsage(ENCODING, issuer_info, &mut usage, 1) } == 0
            || u32::from(usage) & CERT_CRL_SIGN_KEY_USAGE == 0
            || !signature_valid(CRYPT_VERIFY_CERT_SIGN_SUBJECT_CERT, leaf.0.cast(), &issuer)
        {
            return Err(MoodleCrlError::Issuer);
        }
        let distribution = leaf_distribution(leaf_ext).ok_or(MoodleCrlError::DistributionPoint)?;
        Crl::decode(&self.crl)?.check(&issuer, issuer_info, issuer_key, &leaf, distribution, &time)
    }

    fn check_peer(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        now: UnixTime,
    ) -> Result<(), MoodleCrlError> {
        if end.as_ref() != self.leaf.as_ref() {
            return Err(MoodleCrlError::CertificateChanged);
        }
        // Read and verify actual DER data again at handshake time, including freshness.
        self.check_material(name, now)?;
        if intermediates.len() > 32 {
            return Err(MoodleCrlError::CertificateChain);
        }
        let store = MemoryStore::new()?;
        let leaf = store
            .add_certificate(end, true)?
            .ok_or(MoodleCrlError::CertificateChain)?;
        for item in intermediates {
            store.add_certificate(item, false)?;
        }
        store.add_crl(&self.crl)?;
        #[cfg(test)]
        let test_engine = self
            .test_trust
            .as_ref()
            .map(|trust| trust.engine(&store))
            .transpose()?;
        #[cfg(test)]
        let engine = test_engine
            .as_ref()
            .map_or(null_mut(), |owned| owned.engine);
        #[cfg(not(test))]
        let engine = null_mut();
        let chain = Chain::build(engine, &store, &leaf, &filetime(now))?;
        chain.check(&self.host, &self.issuer)
    }
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, MoodleCrlError> {
    let file = File::open(path).map_err(|_| MoodleCrlError::Evidence)?;
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= max as u64)
    {
        return Err(MoodleCrlError::Evidence);
    }
    let mut data = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| MoodleCrlError::Evidence)?;
    if data.is_empty() || data.len() > max {
        return Err(MoodleCrlError::Evidence);
    }
    Ok(data)
}

struct Verifier {
    evidence: Arc<CrlEvidence>,
    provider: Arc<CryptoProvider>,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MoodleCrlVerifier")
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.evidence
            .check_peer(end, intermediates, name, now)
            .map_err(MoodleCrlError::tls_error)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct Certificate(*mut CERT_CONTEXT);

impl Certificate {
    fn decode(der: &[u8]) -> Result<Self, MoodleCrlError> {
        one_der(der, 0x30).ok_or(MoodleCrlError::Evidence)?;
        // The caller has already bounded these DER files.
        let ptr = unsafe { CertCreateCertificateContext(ENCODING, der.as_ptr(), der.len() as u32) };
        if ptr.is_null() {
            Err(MoodleCrlError::Evidence)
        } else {
            Ok(Self(ptr))
        }
    }

    fn info(&self) -> Result<&CERT_INFO, MoodleCrlError> {
        // The info buffer belongs to this live certificate context.
        unsafe { (*self.0).pCertInfo.as_ref() }.ok_or(MoodleCrlError::Evidence)
    }

    fn valid_at(&self, time: &FILETIME) -> bool {
        self.info()
            .is_ok_and(|info| unsafe { CertVerifyTimeValidity(time, info) } == 0)
    }
}

impl Drop for Certificate {
    fn drop(&mut self) {
        unsafe {
            CertFreeCertificateContext(self.0);
        }
    }
}

fn signature_valid(
    subject_kind: u32,
    subject: *const std::ffi::c_void,
    issuer: &Certificate,
) -> bool {
    // A subject context and its issuer context are both explicit. This API verifies a signature
    // with the given public key without building a chain or fetching additional objects.
    unsafe {
        CryptVerifyCertificateSignatureEx(
            0,
            ENCODING,
            subject_kind,
            subject,
            CRYPT_VERIFY_CERT_SIGN_ISSUER_CERT,
            issuer.0.cast(),
            0,
            null_mut(),
        ) != 0
    }
}

struct Crl(*mut CRL_CONTEXT);

impl Crl {
    fn decode(der: &[u8]) -> Result<Self, MoodleCrlError> {
        let mut signed = one_der(der, 0x30).ok_or(MoodleCrlError::Evidence)?;
        let mut body = take_der(&mut signed, 0x30).ok_or(MoodleCrlError::Evidence)?;
        let outer = take_der(&mut signed, 0x30).ok_or(MoodleCrlError::Evidence)?;
        let signature = one_der(signed, 0x03).ok_or(MoodleCrlError::Evidence)?;
        if !matches!(signature, [0, _, ..]) || take_der(&mut body, 0x02) != Some(&[1]) {
            return Err(MoodleCrlError::Evidence);
        }
        let inner = take_der(&mut body, 0x30).ok_or(MoodleCrlError::Evidence)?;
        let mut algorithm = outer;
        let oid = take_der(&mut algorithm, 0x06).ok_or(MoodleCrlError::Signature)?;
        if inner != outer
            || !matches!(
                oid,
                [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 0x0b..=0x0d]
                    | [0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2..=4]
            )
        {
            return Err(MoodleCrlError::Signature);
        }
        let ptr = unsafe { CertCreateCRLContext(ENCODING, der.as_ptr(), der.len() as u32) };
        if ptr.is_null() {
            Err(MoodleCrlError::Evidence)
        } else {
            Ok(Self(ptr))
        }
    }

    fn check(
        &self,
        issuer: &Certificate,
        issuer_info: &CERT_INFO,
        issuer_key: &[u8],
        leaf: &Certificate,
        distribution: &[u8],
        time: &FILETIME,
    ) -> Result<(), MoodleCrlError> {
        let info = unsafe { (*self.0).pCrlInfo.as_ref() }.ok_or(MoodleCrlError::Evidence)?;
        let exts = extensions(info.rgExtension, info.cExtension, info)?;
        if blob(&info.Issuer) != blob(&issuer_info.Subject)
            || authority_key(exts) != Some(issuer_key)
        {
            return Err(MoodleCrlError::Issuer);
        }
        if !signature_valid(CRYPT_VERIFY_CERT_SIGN_SUBJECT_CRL, self.0.cast(), issuer) {
            return Err(MoodleCrlError::Signature);
        }
        if extension(exts, c"2.5.29.27").is_some()
            || extension(exts, c"2.5.29.46").is_some()
            || extension(exts, c"2.5.29.20")
                .and_then(|ext| blob(&ext.Value))
                .and_then(|value| one_der(value, 0x02))
                .is_none_or(|value| value.is_empty() || value.len() > 20 || value[0] >= 128)
            || exts
                .iter()
                .any(|ext| ext.fCritical != 0 && extension_oid(ext) != c"2.5.29.28")
        {
            return Err(MoodleCrlError::Evidence);
        }
        if crl_distribution(exts) != Some(distribution) {
            return Err(MoodleCrlError::DistributionPoint);
        }
        if ticks(&info.ThisUpdate) >= ticks(&info.NextUpdate)
            || ticks(time) >= ticks(&info.NextUpdate)
            || unsafe { CertVerifyCRLTimeValidity(time, info) } != 0
        {
            return Err(MoodleCrlError::CrlTime);
        }
        // This probe supports complete, direct CRLs with no reason partition. Refuse entries
        // whose critical extensions or issuer override would change serial lookup semantics.
        if info.cCRLEntry > 0 {
            if info.rgCRLEntry.is_null() {
                return Err(MoodleCrlError::Evidence);
            }
            let entries =
                unsafe { std::slice::from_raw_parts(info.rgCRLEntry, info.cCRLEntry as usize) };
            for entry in entries {
                let reasons = extensions(entry.rgExtension, entry.cExtension, entry)?;
                if reasons
                    .iter()
                    .any(|ext| ext.fCritical != 0 || extension_oid(ext) == CERTIFICATE_ISSUER)
                    || extension(reasons, c"2.5.29.21").is_some_and(|ext| {
                        !matches!(
                            blob(&ext.Value).and_then(|value| one_der(value, 0x0a)),
                            Some([0..=6 | 9 | 10])
                        )
                    })
                {
                    return Err(MoodleCrlError::Evidence);
                }
            }
        }
        let mut entry = null_mut();
        if unsafe { CertFindCertificateInCRL(leaf.0, self.0, 0, null(), &mut entry) } == 0 {
            return Err(MoodleCrlError::RevocationUnconfirmed);
        }
        if !entry.is_null() {
            return Err(MoodleCrlError::Revoked);
        }
        Ok(())
    }
}

impl Drop for Crl {
    fn drop(&mut self) {
        unsafe {
            CertFreeCRLContext(self.0);
        }
    }
}

fn extensions<T>(
    ptr: *const CERT_EXTENSION,
    size: u32,
    _owner: &T,
) -> Result<&[CERT_EXTENSION], MoodleCrlError> {
    if size == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() || size > 64 {
        return Err(MoodleCrlError::Evidence);
    }
    // CryptoAPI owns these arrays for the lifetime of the owning context/entry.
    let values = unsafe { std::slice::from_raw_parts(ptr, size as usize) };
    let mut seen = HashSet::new();
    if values
        .iter()
        .any(|ext| ext.pszObjId.is_null() || !seen.insert(extension_oid(ext)))
    {
        return Err(MoodleCrlError::Evidence);
    }
    Ok(values)
}

fn extension_oid(ext: &CERT_EXTENSION) -> &CStr {
    // Only called on the parsed extension arrays checked by extensions().
    unsafe { CStr::from_ptr(ext.pszObjId.cast()) }
}

fn extension<'a>(exts: &'a [CERT_EXTENSION], oid: &CStr) -> Option<&'a CERT_EXTENSION> {
    exts.iter().find(|ext| extension_oid(ext) == oid)
}

fn blob(value: &CRYPT_INTEGER_BLOB) -> Option<&[u8]> {
    if value.cbData == 0 {
        return Some(&[]);
    }
    if value.pbData.is_null() {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(value.pbData, value.cbData as usize) })
}

fn authority_key(exts: &[CERT_EXTENSION]) -> Option<&[u8]> {
    let seq = one_der(blob(&extension(exts, c"2.5.29.35")?.Value)?, 0x30)?;
    one_der(seq, 0x80)
}

fn is_ca(exts: &[CERT_EXTENSION]) -> Option<bool> {
    let Some(ext) = extension(exts, c"2.5.29.19") else {
        return Some(false);
    };
    let mut value = one_der(blob(&ext.Value)?, 0x30)?;
    if value.is_empty() {
        Some(false)
    } else {
        Some(take_der(&mut value, 0x01)? == [0xff])
    }
}

fn leaf_distribution(exts: &[CERT_EXTENSION]) -> Option<&[u8]> {
    let points = one_der(blob(&extension(exts, c"2.5.29.31")?.Value)?, 0x30)?;
    let point = one_der(points, 0x30)?;
    distribution_name(one_der(point, 0xa0)?)
}

fn crl_distribution(exts: &[CERT_EXTENSION]) -> Option<&[u8]> {
    let ext = extension(exts, c"2.5.29.28")?;
    if ext.fCritical == 0 {
        return None;
    }
    let mut point = one_der(blob(&ext.Value)?, 0x30)?;
    let name = distribution_name(take_der(&mut point, 0xa0)?)?;
    // Support only an optional onlyContainsUserCerts=TRUE field after the name.
    if !point.is_empty() && one_der(point, 0x81)? != [0xff] {
        return None;
    }
    Some(name)
}

fn distribution_name(name: &[u8]) -> Option<&[u8]> {
    let url = one_der(one_der(name, 0xa0)?, 0x86)?;
    if url.is_empty() || url.len() > 2048 || !url.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return None;
    }
    Some(url)
}

fn one_der(input: &[u8], tag: u8) -> Option<&[u8]> {
    let mut rest = input;
    let value = take_der(&mut rest, tag)?;
    rest.is_empty().then_some(value)
}

fn take_der<'a>(input: &mut &'a [u8], tag: u8) -> Option<&'a [u8]> {
    let (&actual, rest) = input.split_first()?;
    let (&size, rest) = rest.split_first()?;
    if actual != tag {
        return None;
    }
    let (size, rest) = if size < 128 {
        (size.into(), rest)
    } else {
        let width = usize::from(size & 127);
        if !(1..=4).contains(&width) {
            return None;
        }
        let (length, rest) = rest.split_at_checked(width)?;
        let size = length
            .iter()
            .fold(0usize, |out, byte| (out << 8) | usize::from(*byte));
        if length[0] == 0 || size < 128 {
            return None;
        }
        (size, rest)
    };
    let (value, tail) = rest.split_at_checked(size)?;
    *input = tail;
    Some(value)
}

fn filetime(time: UnixTime) -> FILETIME {
    let time = (time.as_secs() + 11_644_473_600) * 10_000_000;
    FILETIME {
        dwLowDateTime: time as u32,
        dwHighDateTime: (time >> 32) as u32,
    }
}

fn ticks(time: &FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

struct MemoryStore(HCERTSTORE);

impl MemoryStore {
    fn new() -> Result<Self, MoodleCrlError> {
        let store = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null()) };
        if store.is_null() {
            Err(MoodleCrlError::Evidence)
        } else {
            Ok(Self(store))
        }
    }

    fn add_certificate(
        &self,
        der: &[u8],
        keep: bool,
    ) -> Result<Option<Certificate>, MoodleCrlError> {
        let mut cert = null_mut();
        let out = if keep { &mut cert } else { null_mut() };
        if unsafe {
            CertAddEncodedCertificateToStore(
                self.0,
                ENCODING,
                der.as_ptr(),
                der.len() as u32,
                CERT_STORE_ADD_ALWAYS,
                out,
            )
        } == 0
        {
            return Err(MoodleCrlError::CertificateChain);
        }
        Ok(if cert.is_null() {
            None
        } else {
            Some(Certificate(cert))
        })
    }

    fn add_crl(&self, der: &[u8]) -> Result<(), MoodleCrlError> {
        if unsafe {
            CertAddEncodedCRLToStore(
                self.0,
                ENCODING,
                der.as_ptr(),
                der.len() as u32,
                CERT_STORE_ADD_ALWAYS,
                null_mut(),
            )
        } == 0
        {
            Err(MoodleCrlError::Evidence)
        } else {
            Ok(())
        }
    }
}

impl Drop for MemoryStore {
    fn drop(&mut self) {
        unsafe {
            CertCloseStore(self.0, 0);
        }
    }
}

struct Chain(*mut CERT_CHAIN_CONTEXT);

impl Chain {
    fn build(
        engine: HCERTCHAINENGINE,
        store: &MemoryStore,
        leaf: &Certificate,
        time: &FILETIME,
    ) -> Result<Self, MoodleCrlError> {
        let mut usage = szOID_PKIX_KP_SERVER_AUTH.cast_mut();
        let mut params = CERT_CHAIN_PARA {
            cbSize: size_of::<CERT_CHAIN_PARA>() as u32,
            ..Default::default()
        };
        params.RequestedUsage.Usage.cUsageIdentifier = 1;
        params.RequestedUsage.Usage.rgpszUsageIdentifier = &mut usage;
        let mut chain = null_mut();
        if unsafe {
            CertGetCertificateChain(
                engine,
                leaf.0,
                time,
                store.0,
                &params,
                OFFLINE_FLAGS,
                null(),
                &mut chain,
            )
        } == 0
            || chain.is_null()
        {
            Err(MoodleCrlError::CertificateChain)
        } else {
            Ok(Self(chain))
        }
    }

    fn check(&self, host: &str, issuer: &[u8]) -> Result<(), MoodleCrlError> {
        let chain = unsafe { &*self.0 };
        let flags = chain.TrustStatus.dwErrorStatus;
        if flags & CERT_TRUST_IS_REVOKED != 0 {
            return Err(MoodleCrlError::Revoked);
        }
        if flags & TIME_ERRORS != 0 {
            return Err(MoodleCrlError::CertificateTime);
        }
        if flags & !REVOCATION_ERRORS != 0 {
            return Err(MoodleCrlError::CertificateChain);
        }
        if flags != 0 {
            return Err(MoodleCrlError::RevocationUnconfirmed);
        }
        if chain.cChain != 1 || chain.rgpChain.is_null() {
            return Err(MoodleCrlError::CertificateChain);
        }
        let path = unsafe { (*chain.rgpChain).as_ref() }.ok_or(MoodleCrlError::CertificateChain)?;
        if path.cElement < 2 || path.cElement > 34 || path.rgpElement.is_null() {
            return Err(MoodleCrlError::CertificateChain);
        }
        let members =
            unsafe { std::slice::from_raw_parts(path.rgpElement, path.cElement as usize) };
        let parent = unsafe {
            members[1]
                .as_ref()
                .and_then(|element| element.pCertContext.as_ref())
        }
        .ok_or(MoodleCrlError::CertificateChain)?;
        if parent.pbCertEncoded.is_null()
            || unsafe {
                std::slice::from_raw_parts(parent.pbCertEncoded, parent.cbCertEncoded as usize)
            } != issuer
        {
            return Err(MoodleCrlError::CertificateChanged);
        }
        for &member in &members[..members.len() - 1] {
            let info = unsafe {
                member
                    .as_ref()
                    .and_then(|member| member.pRevocationInfo.as_ref())
            };
            if !info.is_some_and(|result| result.dwRevocationResult == 0) {
                return Err(MoodleCrlError::RevocationUnconfirmed);
            }
        }
        if self.ssl_policy(host) {
            Ok(())
        } else {
            Err(MoodleCrlError::CertificateChain)
        }
    }

    fn ssl_policy(&self, host: &str) -> bool {
        let mut host: Vec<u16> = host.encode_utf16().chain([0]).collect();
        let mut ssl = HTTPSPolicyCallbackData {
            Anonymous: HTTPSPolicyCallbackData_0 {
                cbSize: size_of::<HTTPSPolicyCallbackData>() as u32,
            },
            dwAuthType: AUTHTYPE_SERVER,
            fdwChecks: 0,
            pwszServerName: host.as_mut_ptr(),
        };
        let params = CERT_CHAIN_POLICY_PARA {
            cbSize: size_of::<CERT_CHAIN_POLICY_PARA>() as u32,
            dwFlags: 0,
            pvExtraPolicyPara: std::ptr::from_mut(&mut ssl).cast(),
        };
        let mut result = CERT_CHAIN_POLICY_STATUS {
            cbSize: size_of::<CERT_CHAIN_POLICY_STATUS>() as u32,
            ..Default::default()
        };
        let valid = unsafe {
            CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, self.0, &params, &mut result)
        };
        valid != 0 && result.dwError == 0
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        unsafe {
            CertFreeCertificateChain(self.0);
        }
    }
}

#[cfg(test)]
#[path = "windows_moodle_tls_tests.rs"]
mod tests;
