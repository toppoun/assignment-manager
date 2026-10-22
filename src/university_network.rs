//! Web egress policy and shared bounded HTTP transport. No server/backend wiring.
//! The reqwest resolver returns only the addresses it has just validated: there
//! is no separate validation lookup followed by an unguarded connection lookup.
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    io::Read,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(test)]
use std::net::SocketAddr;

use reqwest::{
    Url,
    blocking::{Client, RequestBuilder, Response},
    cookie::{CookieStore, Jar},
    dns::{Addrs, Name, Resolve, Resolving},
    header::{ACCEPT_ENCODING, CONTENT_ENCODING, HeaderValue, REFERER},
};

pub(crate) const HOPPII_ORIGIN: &str = "https://hoppii2025.hosei.ac.jp";
pub(crate) const IDP_ORIGIN: &str = "https://idp.hosei.ac.jp";
pub(crate) const WEBCLASS_ORIGIN: &str = "https://lms2025.hosei.ac.jp";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DNS_ANSWERS: usize = 64;

/// Operator-owned, immutable configuration. Default Moodle policy is unset.
/// This type deliberately has no Deserialize or environment configuration API.
#[derive(Clone, Default)]
pub struct NetworkPolicy {
    moodle: Option<(String, String)>,
    #[cfg(test)]
    pub(crate) test: TestNetwork,
}

impl NetworkPolicy {
    /// Set an operator-confirmed exact host and calendar export path. This does
    /// not discover/approve a university, change a feed scope, or enable a backend.
    pub fn with_moodle_calendar(mut self, host: &str, path: &str) -> Result<Self, NetworkError> {
        let url = strict_url(&format!("https://{host}{path}"))?;
        if url.host_str() != Some(host)
            || url.path() != path
            || url.query().is_some()
            || path == "/"
            || path.contains('%')
            || path.contains("//")
        {
            return Err(NetworkError::Policy);
        }
        self.moodle = Some((host.into(), path.into()));
        Ok(self)
    }

    /// Validate the original secret text before URL canonicalization loses syntax.
    /// Never return or format the URL on failure.
    pub fn validate_moodle_url(&self, secret: &str) -> Result<(), NetworkError> {
        self.validate(&strict_url(secret)?, Source::Moodle)
    }

    fn validate(&self, url: &Url, source: Source) -> Result<(), NetworkError> {
        #[cfg(test)]
        if self.test.origins.iter().any(|o| o.origin() == url.origin()) {
            return if url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
            {
                Ok(())
            } else {
                Err(NetworkError::Policy)
            };
        }
        strict_url(url.as_str())?;
        match source {
            Source::Moodle => match &self.moodle {
                Some((host, path)) if url.host_str() == Some(host) && url.path() == path => Ok(()),
                _ => Err(NetworkError::Policy),
            },
            Source::WebClass => {
                if [HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN]
                    .iter()
                    .any(|origin| url.origin().ascii_serialization() == *origin)
                {
                    Ok(())
                } else {
                    Err(NetworkError::Policy)
                }
            }
        }
    }

    fn hosts(&self, source: Source) -> Vec<String> {
        #[cfg(test)]
        if !self.test.origins.is_empty() {
            return self
                .test
                .origins
                .iter()
                .filter_map(|o| o.host_str().map(String::from))
                .collect();
        }
        match source {
            Source::Moodle => self.moodle.iter().map(|(host, _)| host.clone()).collect(),
            Source::WebClass => [HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN]
                .iter()
                .map(|o| Url::parse(o).unwrap().host_str().unwrap().to_owned())
                .collect(),
        }
    }
}

/// Require a canonical ASCII DNS authority; reject parser-normalized disguises,
/// empty userinfo, alternate IP notation, trailing dots, escapes and dot segments.
pub(crate) fn strict_url(raw: &str) -> Result<Url, NetworkError> {
    if !raw.is_ascii()
        || raw
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || raw.contains('\\')
    {
        return Err(NetworkError::Policy);
    }
    let url = Url::parse(raw).map_err(|_| NetworkError::Policy)?;
    let host = url
        .host_str()
        .filter(|host| valid_hostname(host) && host.parse::<IpAddr>().is_err())
        .ok_or(NetworkError::Policy)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(NetworkError::Policy);
    }
    let rest = raw.strip_prefix("https://").ok_or(NetworkError::Policy)?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or(NetworkError::Policy)?;
    if authority != host && authority != format!("{host}:443") {
        return Err(NetworkError::Policy);
    }
    let raw_path = rest[authority.len()..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    if raw_path != url.path() || url.path().contains("//") {
        return Err(NetworkError::Policy);
    }
    Ok(url)
}

fn valid_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && !host.ends_with('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Fixed classifications only; no request, response, resolver or TLS cause chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    Policy,
    Initialization,
    Timeout,
    Request,
    BodyRead,
    BodyTooLarge,
    Encoding,
    Decode,
}
impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Policy => "university network policy rejected request",
            Self::Initialization => "university transport initialization failed",
            Self::Timeout => "university request timed out",
            Self::Request => "university request failed",
            Self::BodyRead => "university response read failed",
            Self::BodyTooLarge => "university response exceeded size limit",
            Self::Encoding => "university response encoding rejected",
            Self::Decode => "university response is not UTF-8",
        })
    }
}
impl Error for NetworkError {}

#[derive(Clone, Copy)]
pub(crate) enum Source {
    Moodle,
    WebClass,
}

struct SystemLookup;
impl Resolve for SystemLookup {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let result = tokio::net::lookup_host((host, 0))
                .await
                .map_err(|_| NetworkError::Request)?;
            Ok(Box::new(result) as Addrs)
        })
    }
}

struct GuardedResolver {
    hosts: Vec<String>,
    lookup: Arc<dyn Resolve>,
    #[cfg(test)]
    peer: Option<SocketAddr>,
    #[cfg(test)]
    loopback_origins: bool,
}
impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        if !self.hosts.iter().any(|h| h == name.as_str()) {
            return Box::pin(async { Err(NetworkError::Policy.into()) });
        }
        let lookup = self.lookup.resolve(name);
        #[cfg(test)]
        let peer = self.peer;
        #[cfg(test)]
        let loopback_origins = self.loopback_origins;
        Box::pin(async move {
            let answers = tokio::time::timeout(CONNECT_TIMEOUT, lookup)
                .await
                .map_err(|_| NetworkError::Timeout)?
                .map_err(|_| NetworkError::Policy)?;
            let mut checked = Vec::new();
            for mut addr in answers.take(MAX_DNS_ANSWERS + 1) {
                #[cfg(test)]
                let test_peer = peer == Some(addr) || (loopback_origins && addr.ip().is_loopback());
                #[cfg(not(test))]
                let test_peer = false;
                if checked.len() == MAX_DNS_ANSWERS || (!test_peer && !public_ip(addr.ip())) {
                    return Err(NetworkError::Policy.into());
                }
                // DNS supplies addresses, never a service/port override.
                if !test_peer {
                    addr.set_port(0);
                }
                checked.push(addr);
            }
            if checked.is_empty() {
                return Err(NetworkError::Policy.into());
            }
            Ok(Box::new(checked.into_iter()) as Addrs)
        })
    }
}

/// Conservative IANA special-purpose deny policy. IPv6 accepts global unicast
/// 2000::/3 only, excluding protocol assignments, documentation and 6to4.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !matches!(a, 0 | 10 | 127 | 224..=255)
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 169 && b == 254)
                && !(a == 172 && (16..=31).contains(&b))
                && !(a == 192
                    && (b == 168
                        || matches!(
                            (b, c),
                            (0, 0 | 2) | (31, 196) | (52, 193) | (88, 99) | (175, 48)
                        )))
                && !(a == 198 && (matches!(b, 18 | 19) || b == 51 && c == 100))
                && !(a == 203 && b == 0 && c == 113)
                && [a, b, c, d] != [168, 63, 129, 16] // Azure platform virtual address
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && s[1] < 0x200)
                && !(s[0] == 0x2001 && s[1] == 0xdb8)
                && s[0] != 0x2002
                && s[0] != 0x3ffe // retired 6bone space
                && !(s[0] == 0x2620 && s[1] == 0x4f && s[2] == 0x8000)
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

/// Private client: consumers cannot extract it or override resolver/proxy/TLS.
#[derive(Clone)]
pub(crate) struct Transport {
    client: Client,
    policy: Option<(NetworkPolicy, Source)>,
}
impl Transport {
    pub(crate) fn desktop(client: Client) -> Self {
        Self {
            client,
            policy: None,
        }
    }
    pub(crate) fn secured(
        policy: NetworkPolicy,
        source: Source,
        cookies: Option<Arc<SessionCookies>>,
    ) -> Result<Self, NetworkError> {
        let lookup: Arc<dyn Resolve> = Arc::new(SystemLookup);
        #[cfg(test)]
        let lookup = policy.test.lookup.clone().unwrap_or(lookup);
        let resolver = GuardedResolver {
            hosts: policy.hosts(source),
            lookup,
            #[cfg(test)]
            peer: policy.test.peer,
            #[cfg(test)]
            loopback_origins: !policy.test.origins.is_empty(),
        };
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .dns_resolver(Arc::new(resolver))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .tls_sni(true)
            .tls_sslkeylogfile(false)
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ));
        if let Some(cookies) = cookies {
            builder = builder.cookie_provider(cookies);
        }
        #[cfg(test)]
        {
            builder = policy.test.configure(builder);
        }
        let client = reqwest::blocking::ClientBuilder::from(builder)
            // Blocking Response::Read polls outside Tokio. Its finite wait timeout
            // bounds each header/body operation; the async client above separately
            // bounds the complete request. Async read_timeout must not be used here.
            .timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| NetworkError::Initialization)?;
        Ok(Self {
            client,
            policy: Some((policy, source)),
        })
    }
    pub(crate) fn get(&self, url: Url) -> RequestBuilder {
        self.client.get(url)
    }
    pub(crate) fn post(&self, url: Url) -> RequestBuilder {
        self.client.post(url)
    }
    pub(crate) fn is_secured(&self) -> bool {
        self.policy.is_some()
    }
    pub(crate) fn send(&self, request: RequestBuilder) -> Result<Response, NetworkError> {
        let mut request = request.build().map_err(|_| NetworkError::Request)?;
        if let Some((policy, source)) = &self.policy {
            policy.validate(request.url(), *source)?;
            // Only the known fixed, query-free protocol Referers may be supplied.
            if let Some(value) = request.headers().get(REFERER) {
                let url = Url::parse(value.to_str().map_err(|_| NetworkError::Policy)?)
                    .map_err(|_| NetworkError::Policy)?;
                let mut clean = url.clone();
                clean.set_query(None);
                clean.set_fragment(None);
                request.headers_mut().insert(
                    REFERER,
                    HeaderValue::from_str(clean.as_str()).map_err(|_| NetworkError::Policy)?,
                );
            }
        }
        request
            .headers_mut()
            .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        self.client.execute(request).map_err(|e| {
            if e.is_timeout() {
                NetworkError::Timeout
            } else {
                NetworkError::Request
            }
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BodyKind {
    MoodleIcs,
    WebClassJson,
    AuthHtml,
    OpenAmJson,
}
impl BodyKind {
    pub(crate) fn limit(self) -> usize {
        match self {
            Self::MoodleIcs | Self::WebClassJson => 8 * 1024 * 1024,
            Self::AuthHtml => 1024 * 1024,
            Self::OpenAmJson => 256 * 1024,
        }
    }
}
pub(crate) fn read_text(mut response: Response, kind: BodyKind) -> Result<String, NetworkError> {
    let limit = kind.limit();
    if response
        .headers()
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|h| !h.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(NetworkError::Encoding);
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(NetworkError::BodyTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut
                || e.get_ref()
                    .and_then(|e| e.downcast_ref::<reqwest::Error>())
                    .is_some_and(reqwest::Error::is_timeout)
            {
                NetworkError::Timeout
            } else {
                NetworkError::BodyRead
            }
        })?;
    if bytes.len() > limit {
        return Err(NetworkError::BodyTooLarge);
    }
    String::from_utf8(bytes).map_err(|_| NetworkError::Decode)
}

/// Web cookies are partitioned by exact response origin, even when a server sets
/// Domain=.hosei.ac.jp. Desktop retains its existing shared-jar behavior.
pub(crate) struct SessionCookies {
    desktop: Option<Jar>,
    origins: Mutex<HashMap<String, Jar>>,
}
impl SessionCookies {
    pub(crate) fn new(isolated: bool) -> Self {
        Self {
            desktop: (!isolated).then(Jar::default),
            origins: Mutex::new(HashMap::new()),
        }
    }
    pub(crate) fn add_cookie_str(&self, cookie: &str, url: &Url) {
        if let Some(jar) = &self.desktop {
            jar.add_cookie_str(cookie, url);
        } else {
            self.origins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(url.origin().ascii_serialization())
                .or_default()
                .add_cookie_str(cookie, url);
        }
    }
}
impl CookieStore for SessionCookies {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        if let Some(jar) = &self.desktop {
            jar.set_cookies(headers, url);
        } else {
            self.origins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(url.origin().ascii_serialization())
                .or_default()
                .set_cookies(headers, url);
        }
    }
    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let mut value = if let Some(jar) = &self.desktop {
            jar.cookies(url)
        } else {
            self.origins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&url.origin().ascii_serialization())
                .and_then(|jar| jar.cookies(url))
        };
        if let Some(value) = &mut value {
            value.set_sensitive(true);
        }
        value
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestNetwork {
    pub(crate) origins: Vec<Url>,
    pub(crate) lookup: Option<Arc<dyn Resolve>>,
    pub(crate) peer: Option<SocketAddr>,
    pub(crate) root: Option<reqwest::Certificate>,
}
#[cfg(test)]
impl TestNetwork {
    fn configure(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if !self.origins.is_empty() {
            builder = builder.https_only(false);
        }
        // Use the same standard verifier for trusted and untrusted TLS tests;
        // an absent root is an empty isolated store, never OS trust fallback.
        builder.tls_certs_only(self.root.iter().cloned())
    }
}

#[cfg(test)]
#[path = "university_network_tests.rs"]
mod tests;
