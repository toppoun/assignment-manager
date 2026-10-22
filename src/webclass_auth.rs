//! Hosei HTTP authentication and internal parsers for the observed response shapes.
//! Credentials are borrowed for one login attempt; scripts are never executed.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, ORIGIN, REFERER};
use reqwest::{StatusCode, Url};
use scraper::{Html, Selector};
use serde_json::Value;

use crate::webclass_http::{WebClassClient, WebClassHttpError};

use crate::university_network::{
    BodyKind, HOPPII_ORIGIN, IDP_ORIGIN, NetworkError, NetworkPolicy, SessionCookies, Source,
    Transport, WEBCLASS_ORIGIN, read_text,
};

const USER_PAGE: &str = "/portal/u001/index.php";
const PORTAL_PUBLIC: &str = "/portal/index.php?next=";
const PORTAL_LOGIN: &str = "/portal/login/login.php?next=";
const PARM: &str = "/portal/sso/Parm.php?auth_mode=1&group_id=";
const AUTHENTICATE: &str = "/openam/json/authenticate";
const XUI: &str = "/openam/XUI/";
const CONSENT: &str = "/openam/saml2/consent/";
const CONSENT_API: &str = "/openam/json/saml2/consent/";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const PASSWORD_LOGIN_SELECTION: u64 = 0;

/// Borrowed credentials. Deliberately implements neither Debug, Clone nor Serialize.
///
/// ```compile_fail
/// use assignment_manager::webclass_auth::HoseiCredentials;
/// let credentials = HoseiCredentials::new("student-test", "synthetic-password");
/// format!("{credentials:?}");
/// ```
/// ```compile_fail
/// use assignment_manager::webclass_auth::HoseiCredentials;
/// let credentials = HoseiCredentials::new("student-test", "synthetic-password");
/// credentials.clone();
/// ```
/// ```compile_fail
/// use assignment_manager::webclass_auth::HoseiCredentials;
/// let credentials = HoseiCredentials::new("student-test", "synthetic-password");
/// serde_json::to_string(&credentials);
/// ```
pub struct HoseiCredentials<'a> {
    university_id: &'a str,
    password: &'a str,
}

impl<'a> HoseiCredentials<'a> {
    pub fn new(university_id: &'a str, password: &'a str) -> Self {
        Self {
            university_id,
            password,
        }
    }
}

/// Fixed diagnostic labels; no server-provided data is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStep {
    HoppiiToIdp,
    IdpToOpenAm,
    OpenAmInitial,
    OpenAmCredentials,
    OpenAmScripted2,
    OpenAmSuccess,
    Saml,
    Consent,
    /// SAML form POST to the HOPPII ACS and validation of its response.
    HoppiiAcs,
    /// GET redirects following the HOPPII ACS response.
    HoppiiPostAcs,
    /// Initial portal login GET and validation of its response.
    HoppiiPortalLoginStart,
    /// GET redirects following the initial portal login response.
    HoppiiPortalLoginRedirect,
    /// Portal user-page verification GET and its subsequent GET redirects.
    HoppiiPortalVerify,
    WebClassSso,
    WebClassRedirect,
    ApiVerification,
}

/// Fixed WebClass redirect failure reasons; no server-provided data is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolReason {
    UnexpectedSuccessStatus,
    /// Location is absent, blank, or cannot be read as a header string.
    MissingLocation,
    BodyDecode,
    InvalidJsRedirect,
    InvalidTarget,
    UnexpectedOrigin,
    UserInfo,
}

/// Contains only fixed categories, steps, reasons and HTTP status codes, never transport causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoseiAuthError {
    ClientInitialization,
    Timeout(AuthStep),
    Request(AuthStep),
    HttpStatus(AuthStep, StatusCode),
    ProtocolChanged(AuthStep),
    ProtocolChangedWithReason(AuthStep, ProtocolReason),
    RedirectLimit(AuthStep),
    ConsentFailed,
    HoppiiSessionFailed,
    ApiVerificationFailed,
}

impl fmt::Display for HoseiAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientInitialization => write!(f, "could not initialize Hosei HTTP session"),
            Self::Timeout(step) => write!(f, "Hosei authentication timed out at {step:?}"),
            Self::Request(step) => write!(f, "Hosei authentication request failed at {step:?}"),
            Self::HttpStatus(step, status) => {
                write!(f, "Hosei authentication returned HTTP {status} at {step:?}")
            }
            Self::ProtocolChanged(step) => {
                write!(f, "unexpected Hosei authentication protocol at {step:?}")
            }
            Self::ProtocolChangedWithReason(step, reason) => {
                write!(
                    f,
                    "unexpected Hosei authentication protocol at {step:?}: {reason:?}"
                )
            }
            Self::RedirectLimit(step) => {
                write!(f, "Hosei authentication redirect limit reached at {step:?}")
            }
            Self::ConsentFailed => write!(f, "Hosei SAML consent failed"),
            Self::HoppiiSessionFailed => {
                write!(f, "HOPPII application session was not established")
            }
            Self::ApiVerificationFailed => {
                write!(f, "WebClass authentication API verification failed")
            }
        }
    }
}

impl Error for HoseiAuthError {}

/// Each login creates a fresh transport and memory cookie jar. Web construction
/// retains its immutable policy across first login and the core's reauth once.
pub struct HoseiAuthenticator {
    policy: Option<NetworkPolicy>,
    #[cfg(test)]
    test_endpoints: Option<AuthEndpoints>,
}

impl Default for HoseiAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

impl HoseiAuthenticator {
    pub fn new() -> Self {
        Self {
            policy: None,
            #[cfg(test)]
            test_endpoints: None,
        }
    }

    /// Construct an explicitly secured Web authenticator; performs no request.
    pub fn for_web(policy: NetworkPolicy) -> Self {
        Self {
            policy: Some(policy),
            #[cfg(test)]
            test_endpoints: None,
        }
    }

    /// Log in once and verify `/courses` as a JSON array before returning the client.
    /// Does not store credentials, retry authentication, or access credential storage.
    pub fn login(
        &self,
        credentials: HoseiCredentials<'_>,
    ) -> Result<WebClassClient, HoseiAuthError> {
        #[cfg(test)]
        if let Some(endpoints) = &self.test_endpoints {
            return self.login_with_endpoints(credentials, endpoints);
        }
        self.login_with_endpoints(credentials, &AuthEndpoints::production())
    }

    fn login_with_endpoints(
        &self,
        credentials: HoseiCredentials<'_>,
        endpoints: &AuthEndpoints,
    ) -> Result<WebClassClient, HoseiAuthError> {
        let session = AuthSession::with_policy(endpoints, self.policy.clone())?;
        let auth_url = session.discover_openam()?;
        let success = session.authenticate(&auth_url, credentials)?;
        let success_url = resolve_destination(
            &endpoints.idp,
            success.success_url(),
            &endpoints.idp,
            AuthStep::OpenAmSuccess,
        )?;
        session.install_openam_cookie(success.token_id())?;
        drop(success);
        session.saml_to_hoppii(success_url)?;
        session.establish_hoppii_session()?;
        session.webclass_sso()?;

        let client = WebClassClient::from_transport(session.http.clone());
        #[cfg(test)]
        let client = {
            let mut client = client;
            client.set_test_origin(endpoints.webclass.clone());
            client
        };
        let body = client.fetch_courses_json().map_err(|error| match error {
            WebClassHttpError::Timeout => HoseiAuthError::Timeout(AuthStep::ApiVerification),
            WebClassHttpError::Request => HoseiAuthError::Request(AuthStep::ApiVerification),
            WebClassHttpError::HttpStatus(status) => {
                HoseiAuthError::HttpStatus(AuthStep::ApiVerification, status)
            }
            _ => HoseiAuthError::ApiVerificationFailed,
        })?;
        serde_json::from_str::<Vec<Value>>(&body)
            .map_err(|_| HoseiAuthError::ApiVerificationFailed)?;
        Ok(client)
    }
}

// This configuration never crosses the public API. Tests replace only the three origins.
struct AuthEndpoints {
    hoppii: Url,
    idp: Url,
    webclass: Url,
    timeout: Duration,
}

impl AuthEndpoints {
    fn production() -> Self {
        Self {
            hoppii: Url::parse(HOPPII_ORIGIN).expect("fixed HOPPII origin"),
            idp: Url::parse(IDP_ORIGIN).expect("fixed IdP origin"),
            webclass: Url::parse(WEBCLASS_ORIGIN).expect("fixed WebClass origin"),
            timeout: TIMEOUT,
        }
    }
}

struct AuthSession<'a> {
    http: Transport,
    jar: Arc<SessionCookies>,
    endpoints: &'a AuthEndpoints,
}

impl<'a> AuthSession<'a> {
    #[cfg(test)]
    fn new(endpoints: &'a AuthEndpoints) -> Result<Self, HoseiAuthError> {
        Self::with_policy(endpoints, None)
    }
    fn with_policy(
        endpoints: &'a AuthEndpoints,
        policy: Option<NetworkPolicy>,
    ) -> Result<Self, HoseiAuthError> {
        let jar = Arc::new(SessionCookies::new(policy.is_some()));
        let http = if let Some(policy) = policy {
            Transport::secured(policy, Source::WebClass, Some(Arc::clone(&jar)))
                .map_err(|_| HoseiAuthError::ClientInitialization)?
        } else {
            let builder = Client::builder()
                .cookie_provider(Arc::clone(&jar))
                .timeout(endpoints.timeout)
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .referer(false)
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd();
            #[cfg(test)]
            let builder = builder.no_proxy();
            Transport::desktop(
                builder
                    .build()
                    .map_err(|_| HoseiAuthError::ClientInitialization)?,
            )
        };
        Ok(Self {
            http,
            jar,
            endpoints,
        })
    }

    fn send(&self, request: RequestBuilder, step: AuthStep) -> Result<Response, HoseiAuthError> {
        self.http
            .send(request)
            .map_err(|error| transport_error(error, step))
    }

    fn discover_openam(&self) -> Result<Url, HoseiAuthError> {
        let response = self.send(
            self.http.get(endpoint(&self.endpoints.hoppii, USER_PAGE)),
            AuthStep::HoppiiToIdp,
        )?;
        let idp_url = redirect_destination(&response, &self.endpoints.idp, AuthStep::HoppiiToIdp)?;
        let response = self.send(self.http.get(idp_url), AuthStep::IdpToOpenAm)?;
        let xui_url = redirect_destination(&response, &self.endpoints.idp, AuthStep::IdpToOpenAm)?;
        parse_xui_auth_url(&xui_url, &self.endpoints.idp)
    }

    fn openam_post(&self, url: &Url) -> RequestBuilder {
        self.http
            .post(url.clone())
            .header("Accept-API-Version", "protocol=1.0,resource=2.0")
            .header("X-Requested-With", "XMLHttpRequest")
            .header(CONTENT_TYPE, "application/json")
            .header("X-Username", "anonymous")
            .header("X-Password", "anonymous")
            .header("X-NoSession", "true")
            .header(REFERER, endpoint(&self.endpoints.idp, XUI).as_str())
    }

    fn authenticate(
        &self,
        auth_url: &Url,
        credentials: HoseiCredentials<'_>,
    ) -> Result<OpenAmSuccess, HoseiAuthError> {
        let initial = read_ok(
            self.send(self.openam_post(auth_url), AuthStep::OpenAmInitial)?,
            AuthStep::OpenAmInitial,
        )?;
        let state = parse_openam_state(&initial)
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::OpenAmInitial))?;
        drop(initial);
        let raw = credential_payload(state, credentials)?;
        let response = self.send(
            self.openam_post(auth_url).json(&raw),
            AuthStep::OpenAmCredentials,
        );
        // Drop the credential-bearing tree before inspecting either the response or error.
        drop(raw);
        let text = read_ok(response?, AuthStep::OpenAmCredentials)?;
        let state = parse_openam_state(&text)
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::OpenAmCredentials))?;
        drop(text);
        state
            .validate_scripted2()
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::OpenAmScripted2))?;
        let raw = state.into_raw();
        let response = self.send(
            self.openam_post(auth_url).json(&raw),
            AuthStep::OpenAmScripted2,
        );
        drop(raw);
        let text = read_ok(response?, AuthStep::OpenAmScripted2)?;
        parse_openam_success(&text)
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::OpenAmSuccess))
    }

    fn install_openam_cookie(&self, token: &str) -> Result<(), HoseiAuthError> {
        // RFC 6265 cookie-octet: prevent an opaque token from injecting cookie attributes.
        if token.is_empty()
            || !token.bytes().all(
                |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
            )
        {
            return Err(HoseiAuthError::ProtocolChanged(AuthStep::OpenAmSuccess));
        }
        let domain = self.endpoints.idp.host_str().expect("fixed IdP has a host");
        // Only the private localhost test configuration uses HTTP.
        let secure = if self.endpoints.idp.scheme() == "https" {
            "; Secure"
        } else {
            ""
        };
        let cookie =
            format!("iPlanetDirectoryPro={token}; Domain={domain}; Path=/; HttpOnly{secure}");
        self.jar.add_cookie_str(&cookie, &self.endpoints.idp);
        Ok(())
    }

    fn saml_to_hoppii(&self, success_url: Url) -> Result<(), HoseiAuthError> {
        let response = self.send(self.http.get(success_url), AuthStep::Saml)?;
        let response = if is_redirect(response.status()) {
            let consent_url =
                redirect_destination(&response, &self.endpoints.idp, AuthStep::Consent)?;
            if consent_url.path() != CONSENT {
                return Err(HoseiAuthError::ProtocolChanged(AuthStep::Consent));
            }
            self.consent(&consent_url)?
        } else {
            response
        };
        let base = response.url().clone();
        let text = read_ok(response, AuthStep::Saml)?;
        let form = parse_auto_submit_form(&text)
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::Saml))?;
        form.validate_saml()
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::Saml))?;
        let action =
            resolve_destination(&base, form.action(), &self.endpoints.hoppii, AuthStep::Saml)?;
        let response = self.send(
            self.http.post(action).form(&form_pairs(&form)),
            AuthStep::HoppiiAcs,
        )?;
        if !matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
        ) {
            require_ok(&response, AuthStep::HoppiiAcs)?;
            return Err(HoseiAuthError::ProtocolChanged(AuthStep::HoppiiAcs));
        }
        self.follow_hoppii_get_redirects(
            response,
            AuthStep::HoppiiAcs,
            AuthStep::HoppiiPostAcs,
            None,
        )?;
        Ok(())
    }

    fn consent(&self, url: &Url) -> Result<Response, HoseiAuthError> {
        let step = AuthStep::Consent;
        let consent_id = required_query_parameter(url, "consentID", step)?;
        let original = required_query_parameter(url, "originalRequestURI", step)?;
        if consent_id.is_empty() || matches!(consent_id.as_str(), "." | "..") || original.is_empty()
        {
            return Err(HoseiAuthError::ConsentFailed);
        }
        let mut return_url =
            resolve_destination(&self.endpoints.idp, &original, &self.endpoints.idp, step)?;
        return_url
            .query_pairs_mut()
            .append_pair("consentID", &consent_id);
        let mut api = endpoint(&self.endpoints.idp, CONSENT_API);
        api.path_segments_mut()
            .expect("fixed HTTP URL")
            .pop_if_empty()
            .push(&consent_id);
        api.query_pairs_mut()
            .append_pair("_action", "getAttributes");
        let text = read_ok(
            self.send(
                self.http
                    .post(api.clone())
                    .header("Accept-API-Version", "protocol=1.0,resource=1.0")
                    .json(&serde_json::json!({"history": null})),
                step,
            )?,
            step,
        )?;
        let attributes: Value =
            serde_json::from_str(&text).map_err(|_| HoseiAuthError::ConsentFailed)?;
        let attributes = attributes.as_array().ok_or(HoseiAuthError::ConsentFailed)?;
        let mut names = Vec::new();
        for item in attributes {
            let item = item.as_object().ok_or(HoseiAuthError::ConsentFailed)?;
            if let Some(name) = item.get("attributeName") {
                names.push(
                    name.as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or(HoseiAuthError::ConsentFailed)?,
                );
            }
        }
        api.set_query(None);
        api.query_pairs_mut().append_pair("_action", "agree");
        let response = self.send(self.http.post(api)
            .header("Accept-API-Version", "protocol=1.0,resource=1.0")
            .json(&serde_json::json!({"type": "ASK_IF_CHANGE", "attributes": names, "history": null})), step)?;
        if !response.status().is_success() {
            return Err(HoseiAuthError::HttpStatus(step, response.status()));
        }
        self.send(self.http.get(return_url), AuthStep::Saml)
    }

    /// Only bounded same-origin GETs; never replay a SAML POST on a redirect.
    /// A fixed Referer is supplied only for the portal login redirect chain.
    fn follow_hoppii_get_redirects(
        &self,
        mut response: Response,
        mut response_step: AuthStep,
        redirect_step: AuthStep,
        fixed_referer: Option<&Url>,
    ) -> Result<Response, HoseiAuthError> {
        for followed in 0..=MAX_REDIRECTS {
            if !is_redirect(response.status()) {
                require_ok(&response, response_step)?;
                return Ok(response);
            }
            let target = redirect_destination(&response, &self.endpoints.hoppii, response_step)?;
            if followed == MAX_REDIRECTS {
                return Err(HoseiAuthError::RedirectLimit(response_step));
            }
            let request = self.http.get(target);
            let request = if let Some(referer) = fixed_referer {
                request.header(REFERER, referer.as_str())
            } else {
                request
            };
            response = self.send(request, redirect_step)?;
            response_step = redirect_step;
        }
        unreachable!("bounded loop returns at the limit")
    }

    fn establish_hoppii_session(&self) -> Result<(), HoseiAuthError> {
        let origin = &self.endpoints.hoppii;
        let login_referer = endpoint(origin, PORTAL_PUBLIC);
        let response = self.send(
            self.http
                .get(endpoint(origin, PORTAL_LOGIN))
                .header(REFERER, login_referer.as_str()),
            AuthStep::HoppiiPortalLoginStart,
        )?;
        self.follow_hoppii_get_redirects(
            response,
            AuthStep::HoppiiPortalLoginStart,
            AuthStep::HoppiiPortalLoginRedirect,
            Some(&login_referer),
        )?;
        let response = self.send(
            self.http
                .get(endpoint(origin, USER_PAGE))
                .header(REFERER, endpoint(origin, PORTAL_LOGIN).as_str()),
            AuthStep::HoppiiPortalVerify,
        )?;
        let response = self.follow_hoppii_get_redirects(
            response,
            AuthStep::HoppiiPortalVerify,
            AuthStep::HoppiiPortalVerify,
            None,
        )?;
        if response.url().path() != USER_PAGE {
            return Err(HoseiAuthError::HoppiiSessionFailed);
        }
        Ok(())
    }

    fn webclass_sso(&self) -> Result<(), HoseiAuthError> {
        let response = self.send(
            self.http
                .get(endpoint(&self.endpoints.hoppii, PARM))
                .header(
                    REFERER,
                    endpoint(&self.endpoints.hoppii, USER_PAGE).as_str(),
                )
                .header(ACCEPT, "text/html"),
            AuthStep::WebClassSso,
        )?;
        let base = response.url().clone();
        let text = read_ok(response, AuthStep::WebClassSso)?;
        let form = parse_auto_submit_form(&text)
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::WebClassSso))?;
        form.validate_hoppii_csso()
            .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::WebClassSso))?;
        let action = resolve_destination(
            &base,
            form.action(),
            &self.endpoints.webclass,
            AuthStep::WebClassSso,
        )?;
        let response = self.send(
            self.http
                .post(action)
                .header(REFERER, base.as_str())
                .header(ORIGIN, self.endpoints.hoppii.origin().ascii_serialization())
                .form(&form_pairs(&form)),
            AuthStep::WebClassSso,
        )?;
        self.follow_webclass_redirects(response)
    }

    fn follow_webclass_redirects(&self, mut response: Response) -> Result<(), HoseiAuthError> {
        let step = AuthStep::WebClassRedirect;
        for followed in 0..=MAX_REDIRECTS {
            let base = response.url().clone();
            let target = if is_redirect(response.status()) {
                Some(redirect_destination(
                    &response,
                    &self.endpoints.webclass,
                    step,
                )?)
            } else {
                let text = read_ok(response, step)?;
                match parse_webclass_js_redirect(&text) {
                    Ok(Some(location)) => Some(resolve_destination(
                        &base,
                        &location,
                        &self.endpoints.webclass,
                        step,
                    )?),
                    // End uninterpretable navigation; login still requires API verification.
                    Ok(None) | Err(WebClassAuthParseError::InvalidRedirect) => None,
                    Err(_) => {
                        return Err(protocol_changed(step, ProtocolReason::InvalidJsRedirect));
                    }
                }
            };
            let Some(target) = target else { return Ok(()) };
            if followed == MAX_REDIRECTS {
                return Err(HoseiAuthError::RedirectLimit(step));
            }
            response = self.send(self.http.get(target).header(REFERER, base.as_str()), step)?;
        }
        unreachable!("bounded loop returns at the limit")
    }
}

fn endpoint(origin: &Url, path: &str) -> Url {
    origin.join(path).expect("fixed endpoint path")
}

// Preserve existing error categories outside the WebClass redirect stage.
fn protocol_changed(step: AuthStep, reason: ProtocolReason) -> HoseiAuthError {
    if step == AuthStep::WebClassRedirect {
        HoseiAuthError::ProtocolChangedWithReason(step, reason)
    } else {
        HoseiAuthError::ProtocolChanged(step)
    }
}

fn resolve_destination(
    base: &Url,
    location: &str,
    origin: &Url,
    step: AuthStep,
) -> Result<Url, HoseiAuthError> {
    if location
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || location.contains('\\')
    {
        return Err(protocol_changed(step, ProtocolReason::InvalidTarget));
    }
    let target = base
        .join(location)
        .map_err(|_| protocol_changed(step, ProtocolReason::InvalidTarget))?;
    if target.origin() != origin.origin() {
        return Err(protocol_changed(step, ProtocolReason::UnexpectedOrigin));
    }
    if !target.username().is_empty() || target.password().is_some() {
        return Err(protocol_changed(step, ProtocolReason::UserInfo));
    }
    // Preserve the original authority check before URL normalization erases an
    // empty userinfo marker or percent/unicode hostname disguise. Relative paths
    // inherit the already validated origin and have no authority of their own.
    let authority = location
        .split_once("://")
        .map(|(_, rest)| rest)
        .or_else(|| location.strip_prefix("//"))
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(""));
    if let Some(authority) = authority {
        if authority.contains('@') {
            return Err(protocol_changed(step, ProtocolReason::UserInfo));
        }
        let host = target
            .host_str()
            .ok_or_else(|| protocol_changed(step, ProtocolReason::InvalidTarget))?;
        let port = target
            .port_or_known_default()
            .ok_or_else(|| protocol_changed(step, ProtocolReason::InvalidTarget))?;
        if authority != host && authority != format!("{host}:{port}") {
            return Err(protocol_changed(step, ProtocolReason::InvalidTarget));
        }
    }
    Ok(target)
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn redirect_destination(
    response: &Response,
    origin: &Url,
    step: AuthStep,
) -> Result<Url, HoseiAuthError> {
    if !is_redirect(response.status()) {
        require_ok(response, step)?;
        return Err(HoseiAuthError::ProtocolChanged(step));
    }
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| protocol_changed(step, ProtocolReason::MissingLocation))?;
    resolve_destination(response.url(), location, origin, step)
}

fn transport_error(error: NetworkError, step: AuthStep) -> HoseiAuthError {
    if error == NetworkError::Timeout {
        HoseiAuthError::Timeout(step)
    } else {
        HoseiAuthError::Request(step)
    }
}

fn require_ok(response: &Response, step: AuthStep) -> Result<(), HoseiAuthError> {
    if response.status() == StatusCode::OK {
        Ok(())
    } else if !response.status().is_success() {
        Err(HoseiAuthError::HttpStatus(step, response.status()))
    } else {
        Err(protocol_changed(
            step,
            ProtocolReason::UnexpectedSuccessStatus,
        ))
    }
}

fn read_ok(response: Response, step: AuthStep) -> Result<String, HoseiAuthError> {
    require_ok(&response, step)?;
    let kind = if matches!(
        step,
        AuthStep::OpenAmInitial | AuthStep::OpenAmCredentials | AuthStep::OpenAmScripted2
    ) {
        BodyKind::OpenAmJson
    } else {
        BodyKind::AuthHtml
    };
    read_text(response, kind).map_err(|error| {
        if error == NetworkError::Decode {
            protocol_changed(step, ProtocolReason::BodyDecode)
        } else {
            transport_error(error, step)
        }
    })
}

fn required_query_parameter(
    url: &Url,
    name: &str,
    step: AuthStep,
) -> Result<String, HoseiAuthError> {
    let mut matches = url.query_pairs().filter(|(key, _)| key == name);
    let (_, value) = matches
        .next()
        .ok_or(HoseiAuthError::ProtocolChanged(step))?;
    if matches.next().is_some() {
        return Err(HoseiAuthError::ProtocolChanged(step));
    }
    Ok(value.into_owned())
}

fn parse_xui_auth_url(xui: &Url, idp: &Url) -> Result<Url, HoseiAuthError> {
    let step = AuthStep::IdpToOpenAm;
    resolve_destination(idp, xui.as_str(), idp, step)?;
    if xui.path() != XUI {
        return Err(HoseiAuthError::ProtocolChanged(step));
    }
    let (_, query) = xui
        .fragment()
        .and_then(|fragment| fragment.split_once("/&"))
        .filter(|(route, query)| !route.is_empty() && !query.is_empty())
        .ok_or(HoseiAuthError::ProtocolChanged(step))?;
    let mut params = xui.clone();
    params.set_fragment(None);
    params.set_query(Some(query));
    let mut auth_url = endpoint(idp, AUTHENTICATE);
    for name in [
        "realm",
        "forward",
        "spEntityID",
        "askConsent",
        "goto",
        "AMAuthCookie",
    ] {
        let mut value = required_query_parameter(&params, name, step)?;
        if name == "goto" {
            if value.starts_with('/') && !value.starts_with("//") && !value.starts_with("/openam/")
            {
                value.insert_str(0, "/openam");
            }
            resolve_destination(idp, &value, idp, step)?;
        }
        auth_url.query_pairs_mut().append_pair(name, &value);
    }
    Ok(auth_url)
}

fn credential_payload(
    state: OpenAmState,
    credentials: HoseiCredentials<'_>,
) -> Result<Value, HoseiAuthError> {
    state
        .validate_credential_callbacks()
        .map_err(|_| HoseiAuthError::ProtocolChanged(AuthStep::OpenAmCredentials))?;
    let mut raw = state.into_raw();
    for callback in raw["callbacks"]
        .as_array_mut()
        .expect("validated callbacks")
    {
        let value = match callback["type"].as_str() {
            Some("NameCallback") => Value::String(credentials.university_id.to_owned()),
            Some("PasswordCallback") => Value::String(credentials.password.to_owned()),
            Some("ConfirmationCallback") => Value::from(PASSWORD_LOGIN_SELECTION),
            _ => continue, // Validation permits only the read-only TextOutputCallback here.
        };
        callback["input"][0]["value"] = value;
    }
    Ok(raw)
}

fn form_pairs(form: &AutoSubmitForm) -> Vec<(&str, &str)> {
    form.fields()
        .iter()
        .map(|field| (field.name(), field.value()))
        .collect()
}

/// Only fixed field names and categories are retained, never response values or causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebClassAuthParseError {
    InvalidJson,
    UnexpectedOpenAmState,
    MissingField(&'static str),
    InvalidForm,
    UnexpectedFormShape,
    InvalidRedirect,
}

impl fmt::Display for WebClassAuthParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => write!(f, "invalid OpenAM JSON"),
            Self::UnexpectedOpenAmState => write!(f, "unexpected OpenAM response shape or stage"),
            Self::MissingField(name) => write!(f, "authentication response missing field: {name}"),
            Self::InvalidForm => write!(f, "invalid authentication form"),
            Self::UnexpectedFormShape => write!(f, "unexpected authentication form shape"),
            Self::InvalidRedirect => write!(f, "invalid WebClass JavaScript redirect"),
        }
    }
}

impl Error for WebClassAuthParseError {}

/// Retains the entire JSON tree, including unknown fields and callbacks.
/// Deliberately implements neither Clone nor Serialize; Debug hides the tree.
pub(crate) struct OpenAmState {
    raw: Value,
}

impl fmt::Debug for OpenAmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAmState")
            .field("raw", &"[redacted]")
            .finish()
    }
}

pub(crate) fn parse_openam_state(json: &str) -> Result<OpenAmState, WebClassAuthParseError> {
    let raw = parse_json_object(json)?;
    required_string(&raw, "authId")?;
    let callbacks = raw
        .get("callbacks")
        .ok_or(WebClassAuthParseError::MissingField("callbacks"))?
        .as_array()
        .filter(|callbacks| !callbacks.is_empty())
        .ok_or(WebClassAuthParseError::UnexpectedOpenAmState)?;
    for callback in callbacks {
        if !callback.is_object() {
            return Err(WebClassAuthParseError::UnexpectedOpenAmState);
        }
        required_string(callback, "type")?;
    }
    if raw.get("stage").is_some_and(|stage| !stage.is_string()) {
        return Err(WebClassAuthParseError::UnexpectedOpenAmState);
    }
    Ok(OpenAmState { raw })
}

impl OpenAmState {
    pub(crate) fn stage(&self) -> Option<&str> {
        self.raw.get("stage").and_then(Value::as_str)
    }

    fn callbacks(&self) -> &[Value] {
        self.raw["callbacks"]
            .as_array()
            .expect("parse_openam_state validated callbacks; raw is private")
    }

    #[cfg(test)]
    pub(crate) fn callback_types(&self) -> impl Iterator<Item = &str> {
        self.callbacks().iter().map(|callback| {
            callback["type"]
                .as_str()
                .expect("parse_openam_state validated callback types")
        })
    }

    /// Finds by type, rejecting ambiguity instead of choosing the first duplicate.
    /// The returned JSON may contain secrets; it is for inspection, never logging.
    pub(crate) fn callback(
        &self,
        callback_type: &str,
    ) -> Result<Option<&Value>, WebClassAuthParseError> {
        let mut matches = self
            .callbacks()
            .iter()
            .filter(|callback| callback["type"].as_str() == Some(callback_type));
        let first = matches.next();
        if matches.next().is_some() {
            return Err(WebClassAuthParseError::UnexpectedOpenAmState);
        }
        Ok(first)
    }

    /// Conservative observed callback shape, independent of the synthetic initial stage name.
    /// Unknown authentication requests (including MFA) must reach a protocol-change path.
    pub(crate) fn validate_credential_callbacks(&self) -> Result<(), WebClassAuthParseError> {
        self.validate_callbacks(&[
            "NameCallback",
            "PasswordCallback",
            "ConfirmationCallback",
            "TextOutputCallback",
        ])
    }

    pub(crate) fn validate_scripted2(&self) -> Result<(), WebClassAuthParseError> {
        if self.stage() != Some("Scripted2") {
            return Err(WebClassAuthParseError::UnexpectedOpenAmState);
        }
        self.validate_callbacks(&["HiddenValueCallback", "TextOutputCallback"])
    }

    fn validate_callbacks(&self, expected: &[&str]) -> Result<(), WebClassAuthParseError> {
        if self.callbacks().len() != expected.len() {
            return Err(WebClassAuthParseError::UnexpectedOpenAmState);
        }
        for &callback_type in expected {
            let callback = self
                .callback(callback_type)?
                .ok_or(WebClassAuthParseError::UnexpectedOpenAmState)?;
            let output = callback
                .get("output")
                .and_then(Value::as_array)
                .filter(|output| !output.is_empty())
                .ok_or(WebClassAuthParseError::UnexpectedOpenAmState)?;
            validate_name_value_pairs(output)?;

            if callback_type == "TextOutputCallback" {
                if callback
                    .get("input")
                    .is_some_and(|input| !input.as_array().is_some_and(Vec::is_empty))
                {
                    return Err(WebClassAuthParseError::UnexpectedOpenAmState);
                }
                continue;
            }
            let input = callback
                .get("input")
                .and_then(Value::as_array)
                .filter(|input| input.len() == 1)
                .ok_or(WebClassAuthParseError::UnexpectedOpenAmState)?;
            validate_name_value_pairs(input)?;
            let value = &input[0]["value"];
            let valid = if callback_type == "ConfirmationCallback" {
                value.is_i64() || value.is_u64()
            } else {
                value.is_string()
            };
            if !valid {
                return Err(WebClassAuthParseError::UnexpectedOpenAmState);
            }
        }
        Ok(())
    }

    /// Transfer the unchanged tree for selective edits and reposting.
    /// Serializing this Value retains unknown fields; callers must never log it.
    pub(crate) fn into_raw(self) -> Value {
        self.raw
    }
}

fn validate_name_value_pairs(pairs: &[Value]) -> Result<(), WebClassAuthParseError> {
    if pairs.iter().any(|pair| {
        !pair.is_object()
            || !pair
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
            || pair.get("value").is_none()
    }) {
        return Err(WebClassAuthParseError::UnexpectedOpenAmState);
    }
    Ok(())
}

pub(crate) struct OpenAmSuccess {
    token_id: String,
    success_url: String,
}

impl OpenAmSuccess {
    pub(crate) fn token_id(&self) -> &str {
        &self.token_id
    }

    /// Opaque response string; URL resolution and destination checks belong to Phase 5C.
    pub(crate) fn success_url(&self) -> &str {
        &self.success_url
    }
}

impl fmt::Debug for OpenAmSuccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAmSuccess")
            .field("token_id", &"[redacted]")
            .field("success_url", &"[redacted]")
            .finish()
    }
}

pub(crate) fn parse_openam_success(json: &str) -> Result<OpenAmSuccess, WebClassAuthParseError> {
    let raw = parse_json_object(json)?;
    Ok(OpenAmSuccess {
        token_id: required_string(&raw, "tokenId")?.to_owned(),
        success_url: required_string(&raw, "successUrl")?.to_owned(),
    })
}

fn parse_json_object(json: &str) -> Result<Value, WebClassAuthParseError> {
    let raw: Value = serde_json::from_str(json).map_err(|_| WebClassAuthParseError::InvalidJson)?;
    if !raw.is_object() {
        return Err(WebClassAuthParseError::UnexpectedOpenAmState);
    }
    Ok(raw)
}

fn required_string<'a>(
    raw: &'a Value,
    name: &'static str,
) -> Result<&'a str, WebClassAuthParseError> {
    raw.get(name)
        .ok_or(WebClassAuthParseError::MissingField(name))?
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(WebClassAuthParseError::UnexpectedOpenAmState)
}

/// Individual fields are also safe to Debug, even through a slice or iterator.
pub(crate) struct FormField {
    name: String,
    value: String,
}

impl FormField {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for FormField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormField")
            // Even an unrecognized field name can carry attacker-controlled private data.
            .field("name", &"[redacted]")
            .field("value", &"[redacted]")
            .finish()
    }
}

pub(crate) struct AutoSubmitForm {
    action: String,
    fields: Vec<FormField>,
}

impl fmt::Debug for AutoSubmitForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutoSubmitForm")
            .field("action", &"[redacted]")
            .field("fields", &self.fields)
            .finish()
    }
}

/// Reads one form and its descendant named inputs; does not emulate browser submission.
/// Multiple forms are ambiguous and rejected rather than mixing their fields.
pub(crate) fn parse_auto_submit_form(html: &str) -> Result<AutoSubmitForm, WebClassAuthParseError> {
    let document = Html::parse_document(html);
    let form_selector = Selector::parse("form").expect("fixed form selector is valid");
    let input_selector = Selector::parse("input[name]").expect("fixed input selector is valid");
    let mut forms = document.select(&form_selector);
    let form = forms.next().ok_or(WebClassAuthParseError::InvalidForm)?;
    if forms.next().is_some() {
        return Err(WebClassAuthParseError::InvalidForm);
    }
    let action = form
        .attr("action")
        .filter(|action| !action.trim().is_empty())
        .ok_or(WebClassAuthParseError::MissingField("action"))?
        .to_owned();
    let fields = form
        .select(&input_selector)
        .filter_map(|input| {
            let name = input.attr("name").filter(|name| !name.is_empty())?;
            Some(FormField {
                name: name.to_owned(),
                value: input.attr("value").unwrap_or("").to_owned(),
            })
        })
        .collect();
    Ok(AutoSubmitForm { action, fields })
}

impl AutoSubmitForm {
    pub(crate) fn action(&self) -> &str {
        &self.action
    }

    /// Retains document order and duplicates for future forwarding.
    pub(crate) fn fields(&self) -> &[FormField] {
        &self.fields
    }

    pub(crate) fn field_value(&self, name: &str) -> Result<Option<&str>, WebClassAuthParseError> {
        let mut matches = self.fields.iter().filter(|field| field.name == name);
        let first = matches.next();
        if matches.next().is_some() {
            return Err(WebClassAuthParseError::UnexpectedFormShape);
        }
        Ok(first.map(FormField::value))
    }

    /// Checks only unique field presence, not SAML/XML, signatures, or RelayState semantics.
    pub(crate) fn validate_saml(&self) -> Result<(), WebClassAuthParseError> {
        self.validate_required_fields(&["SAMLResponse", "RelayState"])
    }

    /// All values remain opaque, including an empty password, uid, timestamp, and hash.
    pub(crate) fn validate_hoppii_csso(&self) -> Result<(), WebClassAuthParseError> {
        self.validate_required_fields(&[
            "uid",
            "password",
            "timestamp",
            "hash",
            "AC_LOGIN",
            "btnLogin",
        ])
    }

    fn validate_required_fields(
        &self,
        names: &[&'static str],
    ) -> Result<(), WebClassAuthParseError> {
        for &name in names {
            self.field_value(name)?
                .ok_or(WebClassAuthParseError::MissingField(name))?;
        }
        Ok(())
    }
}

/// Recognizes only an inline script consisting of one known literal direct assignment.
/// Surrounding whitespace and one trailing semicolon are allowed. No expressions,
/// comments, embedded assignments, other escapes, or executable JS are interpreted.
pub(crate) fn parse_webclass_js_redirect(
    html: &str,
) -> Result<Option<String>, WebClassAuthParseError> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("script").expect("fixed script selector is valid");
    let mut redirect = None;
    for script in document.select(&selector) {
        if script.attr("src").is_some()
            || script.attr("type").is_some_and(|kind| {
                !matches!(kind, "" | "text/javascript" | "application/javascript")
            })
        {
            continue;
        }
        if let Some(value) = parse_direct_assignment(&script.text().collect::<String>())? {
            if redirect.is_some() {
                return Err(WebClassAuthParseError::InvalidRedirect);
            }
            redirect = Some(value);
        }
    }
    Ok(redirect)
}

fn parse_direct_assignment(script: &str) -> Result<Option<String>, WebClassAuthParseError> {
    const TARGETS: [&str; 6] = [
        "window.top.location.href",
        "top.location.href",
        "window.location.href",
        "location.href",
        "window.location",
        "location",
    ];
    let script = script.trim();
    for target in TARGETS {
        let Some(tail) = script.strip_prefix(target) else {
            continue;
        };
        let Some(value) = tail.trim_start().strip_prefix('=') else {
            continue;
        };
        return quoted_redirect(value.trim_start()).map(Some);
    }
    Ok(None)
}

fn quoted_redirect(value: &str) -> Result<String, WebClassAuthParseError> {
    let mut characters = value.char_indices();
    let (_, quote @ ('\'' | '"')) = characters
        .next()
        .ok_or(WebClassAuthParseError::InvalidRedirect)?
    else {
        return Err(WebClassAuthParseError::InvalidRedirect);
    };
    let mut decoded = String::new();
    while let Some((index, character)) = characters.next() {
        match character {
            character if character == quote => {
                let tail = value[index + quote.len_utf8()..].trim();
                if decoded.is_empty() || !matches!(tail, "" | ";") {
                    return Err(WebClassAuthParseError::InvalidRedirect);
                }
                return Ok(decoded);
            }
            '\\' => match characters.next() {
                Some((_, '/')) => decoded.push('/'),
                _ => return Err(WebClassAuthParseError::InvalidRedirect),
            },
            character if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') => {
                return Err(WebClassAuthParseError::InvalidRedirect);
            }
            character => decoded.push(character),
        }
    }
    Err(WebClassAuthParseError::InvalidRedirect)
}

#[cfg(test)]
#[path = "webclass_auth_tests.rs"]
mod flow_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const INITIAL: &str = include_str!("../fixtures/webclass_auth/openam_initial_callbacks.json");
    const SCRIPTED2: &str = include_str!("../fixtures/webclass_auth/openam_scripted2.json");
    const SUCCESS: &str = include_str!("../fixtures/webclass_auth/openam_success.json");
    const SAML: &str = include_str!("../fixtures/webclass_auth/saml_auto_submit.html");
    const CSSO: &str = include_str!("../fixtures/webclass_auth/hoppii_csso_form.html");
    const JS: &str = include_str!("../fixtures/webclass_auth/webclass_js_redirect.html");
    const JS_ESCAPED: &str =
        include_str!("../fixtures/webclass_auth/webclass_js_redirect_escaped.html");

    fn state(raw: &Value) -> OpenAmState {
        parse_openam_state(&raw.to_string()).unwrap()
    }

    fn callback_mut<'a>(raw: &'a mut Value, name: &str) -> &'a mut Value {
        raw["callbacks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|callback| callback["type"] == name)
            .unwrap()
    }

    fn assert_secret_free(text: &str, secrets: &[&str]) {
        for secret in secrets {
            assert!(
                !text.contains(secret),
                "secret appeared in diagnostic output"
            );
        }
    }

    fn assert_safe_error(error: WebClassAuthParseError, secrets: &[&str]) {
        assert_secret_free(&format!("{error:?} {error:#?} {error}"), secrets);
        assert!(error.source().is_none());
    }

    #[test]
    fn initial_callbacks_are_found_by_type_including_text_output() {
        let state = parse_openam_state(INITIAL).unwrap();
        state.validate_credential_callbacks().unwrap();
        assert_eq!(state.stage(), Some("SyntheticInitial"));
        assert_eq!(state.callback_types().count(), 4);
        for name in [
            "NameCallback",
            "PasswordCallback",
            "ConfirmationCallback",
            "TextOutputCallback",
        ] {
            assert_eq!(state.callback(name).unwrap().unwrap()["type"], name);
        }
        assert!(state.callback("HiddenValueCallback").unwrap().is_none());
    }

    #[test]
    fn credential_shape_is_independent_of_callback_order_and_synthetic_identifiers() {
        let original: Value = serde_json::from_str(INITIAL).unwrap();
        for reverse in [false, true] {
            for rotation in 0..4 {
                let mut raw = original.clone();
                let callbacks = raw["callbacks"].as_array_mut().unwrap();
                callbacks.rotate_left(rotation);
                if reverse {
                    callbacks.reverse();
                }
                raw["stage"] = json!("another-synthetic-initial-stage");
                for name in ["NameCallback", "PasswordCallback", "ConfirmationCallback"] {
                    callback_mut(&mut raw, name)["input"][0]["name"] = json!("opaque-input-name");
                }
                let state = state(&raw);
                state.validate_credential_callbacks().unwrap();
                assert_eq!(
                    state.callback("NameCallback").unwrap().unwrap()["input"][0]["name"],
                    "opaque-input-name"
                );
            }
        }
    }

    #[test]
    fn unknown_callbacks_are_preserved_but_expected_credential_shape_rejects_them() {
        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        let unknown =
            json!({"type":"SyntheticMfaCallback", "opaque":{"challenge":"synthetic-mfa-secret"}});
        raw["callbacks"]
            .as_array_mut()
            .unwrap()
            .push(unknown.clone());
        let state = state(&raw);
        assert_eq!(
            state.callback("SyntheticMfaCallback").unwrap(),
            Some(&unknown)
        );
        assert!(
            state
                .callback_types()
                .any(|kind| kind == "SyntheticMfaCallback")
        );
        assert_eq!(
            state.validate_credential_callbacks(),
            Err(WebClassAuthParseError::UnexpectedOpenAmState)
        );
        assert_eq!(state.into_raw(), raw);

        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        callback_mut(&mut raw, "TextOutputCallback")["type"] = json!("SyntheticMfaCallback");
        assert!(
            super::parse_openam_state(&raw.to_string())
                .unwrap()
                .validate_credential_callbacks()
                .is_err()
        );
    }

    #[test]
    fn credential_shape_rejects_missing_and_duplicate_callback_types() {
        for name in [
            "NameCallback",
            "PasswordCallback",
            "ConfirmationCallback",
            "TextOutputCallback",
        ] {
            let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
            raw["callbacks"]
                .as_array_mut()
                .unwrap()
                .retain(|callback| callback["type"] != name);
            assert_eq!(
                state(&raw).validate_credential_callbacks(),
                Err(WebClassAuthParseError::UnexpectedOpenAmState)
            );
        }
        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        let duplicate = callback_mut(&mut raw, "NameCallback").clone();
        *callback_mut(&mut raw, "PasswordCallback") = duplicate;
        let state = state(&raw);
        assert_eq!(
            state.callback("NameCallback"),
            Err(WebClassAuthParseError::UnexpectedOpenAmState)
        );
        assert!(state.validate_credential_callbacks().is_err());
    }

    #[test]
    fn malformed_json_and_invalid_state_roots_fail_without_response_causes() {
        for json in [
            "",
            "{",
            r#"{"authId":"synthetic-private-json", "callbacks": BROKEN}"#,
        ] {
            assert_eq!(
                parse_openam_state(json).unwrap_err(),
                WebClassAuthParseError::InvalidJson
            );
        }
        for raw in ["null", "[]", "42", "\"synthetic-private-json\""] {
            assert_eq!(
                parse_openam_state(raw).unwrap_err(),
                WebClassAuthParseError::UnexpectedOpenAmState
            );
        }
    }

    #[test]
    fn missing_or_malformed_callback_state_fields_fail() {
        for name in ["authId", "callbacks"] {
            let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
            raw.as_object_mut().unwrap().remove(name);
            assert_eq!(
                parse_openam_state(&raw.to_string()).unwrap_err(),
                WebClassAuthParseError::MissingField(name)
            );
        }
        for callbacks in [
            json!(null),
            json!({}),
            json!([]),
            json!([null]),
            json!([{}]),
            json!([{"type":42}]),
            json!([{"type":""}]),
        ] {
            let raw = json!({"authId":"synthetic-auth-id", "callbacks":callbacks});
            assert!(parse_openam_state(&raw.to_string()).is_err());
        }
        for (name, value) in [
            ("authId", json!(null)),
            ("authId", json!("")),
            ("authId", json!(5)),
            ("stage", json!(null)),
            ("stage", json!([])),
        ] {
            let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
            raw[name] = value;
            assert!(parse_openam_state(&raw.to_string()).is_err());
        }
    }

    #[test]
    fn expected_callback_validation_checks_named_input_and_output_shapes() {
        for name in ["NameCallback", "PasswordCallback", "ConfirmationCallback"] {
            for invalid in [
                json!(null),
                json!([]),
                json!({}),
                json!([{"name":"input"}]),
                json!([{"value":""}]),
                json!([{"name":"", "value":""}]),
                json!([{"name":"input", "value":[]}]),
                json!([{"name":"one","value":""},{"name":"two","value":""}]),
            ] {
                let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
                callback_mut(&mut raw, name)["input"] = invalid;
                assert!(state(&raw).validate_credential_callbacks().is_err());
            }
        }
        for invalid in [
            json!(null),
            json!([]),
            json!([{"name":"message"}]),
            json!([{"value":"synthetic-message"}]),
        ] {
            let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
            callback_mut(&mut raw, "TextOutputCallback")["output"] = invalid;
            assert!(state(&raw).validate_credential_callbacks().is_err());
        }
        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        callback_mut(&mut raw, "TextOutputCallback")["input"] =
            json!([{"name":"unexpected-auth-request", "value":""}]);
        assert!(state(&raw).validate_credential_callbacks().is_err());
        callback_mut(&mut raw, "TextOutputCallback")["input"] = json!([]);
        state(&raw).validate_credential_callbacks().unwrap();
        for (name, value) in [
            ("NameCallback", json!(0)),
            ("PasswordCallback", json!(0)),
            ("ConfirmationCallback", json!("0")),
            ("ConfirmationCallback", json!(0.5)),
        ] {
            let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
            callback_mut(&mut raw, name)["input"][0]["value"] = value;
            assert!(state(&raw).validate_credential_callbacks().is_err());
        }
    }

    #[test]
    fn raw_json_roundtrip_and_selective_edits_preserve_all_unknown_fields() {
        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        raw["unknownTopLevel"] = json!({"values":[null, true, 17, "opaque"]});
        let name = callback_mut(&mut raw, "NameCallback");
        name["unknownCallbackField"] = json!(["opaque", {"nested":true}]);
        name["input"][0]["unknownInputField"] = json!({"retain":42});
        name["output"][0]["unknownOutputField"] = json!(false);
        let parsed = state(&raw);
        parsed.validate_credential_callbacks().unwrap();
        let serialized = serde_json::to_string(&parsed.into_raw()).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&serialized).unwrap(), raw);

        let mut edited = state(&raw).into_raw();
        callback_mut(&mut edited, "NameCallback")["input"][0]["value"] =
            json!("synthetic-edited-name");
        callback_mut(&mut raw, "NameCallback")["input"][0]["value"] =
            json!("synthetic-edited-name");
        assert_eq!(edited, raw);
    }

    #[test]
    fn scripted2_recognizes_stage_and_hidden_value_independent_of_order() {
        let mut raw: Value = serde_json::from_str(SCRIPTED2).unwrap();
        for _ in 0..2 {
            raw["callbacks"].as_array_mut().unwrap().reverse();
            let state = state(&raw);
            assert_eq!(state.stage(), Some("Scripted2"));
            state.validate_scripted2().unwrap();
            assert_eq!(
                state.callback("HiddenValueCallback").unwrap().unwrap()["input"][0]["value"],
                "synthetic-hidden-input"
            );
            assert!(
                state
                    .callback_types()
                    .any(|kind| kind == "TextOutputCallback")
            );
            assert!(state.validate_credential_callbacks().is_err());
        }
    }

    #[test]
    fn scripted2_rejects_unknown_or_absent_stage_and_unexpected_callbacks() {
        for stage in [
            Some("SyntheticUnknownStage"),
            Some("scripted2"),
            Some(""),
            None,
        ] {
            let mut raw: Value = serde_json::from_str(SCRIPTED2).unwrap();
            if let Some(stage) = stage {
                raw["stage"] = json!(stage);
            } else {
                raw.as_object_mut().unwrap().remove("stage");
            }
            assert_eq!(
                state(&raw).validate_scripted2(),
                Err(WebClassAuthParseError::UnexpectedOpenAmState)
            );
        }
        let mut raw: Value = serde_json::from_str(SCRIPTED2).unwrap();
        callback_mut(&mut raw, "HiddenValueCallback")["type"] = json!("SyntheticMfaCallback");
        assert!(state(&raw).validate_scripted2().is_err());
        let mut raw: Value = serde_json::from_str(SCRIPTED2).unwrap();
        let hidden = callback_mut(&mut raw, "HiddenValueCallback").clone();
        *callback_mut(&mut raw, "TextOutputCallback") = hidden;
        assert!(state(&raw).validate_scripted2().is_err());
        let mut raw: Value = serde_json::from_str(SCRIPTED2).unwrap();
        callback_mut(&mut raw, "HiddenValueCallback")["input"] = json!([]);
        assert!(state(&raw).validate_scripted2().is_err());
    }

    #[test]
    fn callback_state_debug_hides_secrets_in_all_response_positions() {
        let secrets = [
            "synthetic-user-secret",
            "synthetic-password-secret",
            "synthetic-state-id-secret",
            "synthetic-output-secret",
            "synthetic-stage-secret",
            "synthetic-unknown-type-secret",
        ];
        let mut raw: Value = serde_json::from_str(INITIAL).unwrap();
        raw["authId"] = json!(secrets[2]);
        raw["stage"] = json!(secrets[4]);
        callback_mut(&mut raw, "NameCallback")["input"][0]["value"] = json!(secrets[0]);
        callback_mut(&mut raw, "PasswordCallback")["input"][0]["value"] = json!(secrets[1]);
        callback_mut(&mut raw, "TextOutputCallback")["output"][0]["value"] = json!(secrets[3]);
        raw["callbacks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":secrets[5]}));
        let state = state(&raw);
        assert_secret_free(&format!("{state:?} {state:#?}"), &secrets);
        assert_safe_error(state.validate_credential_callbacks().unwrap_err(), &secrets);
    }

    #[test]
    fn success_extracts_token_and_opaque_relative_url() {
        let success = parse_openam_success(SUCCESS).unwrap();
        assert_eq!(success.token_id(), "synthetic-openam-token");
        assert_eq!(
            success.success_url(),
            "/openam/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id"
        );
    }

    #[test]
    fn success_rejects_missing_empty_and_wrong_type_required_fields() {
        for name in ["tokenId", "successUrl"] {
            let mut raw: Value = serde_json::from_str(SUCCESS).unwrap();
            raw.as_object_mut().unwrap().remove(name);
            assert_eq!(
                parse_openam_success(&raw.to_string()).unwrap_err(),
                WebClassAuthParseError::MissingField(name)
            );
            for invalid in [json!(null), json!(42), json!(""), json!([])] {
                let mut raw: Value = serde_json::from_str(SUCCESS).unwrap();
                raw[name] = invalid;
                assert_eq!(
                    parse_openam_success(&raw.to_string()).unwrap_err(),
                    WebClassAuthParseError::UnexpectedOpenAmState
                );
            }
        }
        assert_eq!(
            parse_openam_success("{").unwrap_err(),
            WebClassAuthParseError::InvalidJson
        );
        assert!(parse_openam_success("[]").is_err());
    }

    #[test]
    fn success_debug_and_parse_errors_hide_token_and_success_url_query() {
        let secrets = ["synthetic-private-token", "synthetic-private-request-query"];
        let mut raw =
            json!({"tokenId":secrets[0], "successUrl":format!("/opaque?request={}", secrets[1])});
        let success = parse_openam_success(&raw.to_string()).unwrap();
        assert_secret_free(&format!("{success:?} {success:#?}"), &secrets);
        for name in ["tokenId", "successUrl"] {
            let removed = raw.as_object_mut().unwrap().remove(name).unwrap();
            assert_safe_error(
                parse_openam_success(&raw.to_string()).unwrap_err(),
                &secrets,
            );
            raw[name] = removed;
        }
        assert_safe_error(
            parse_openam_success(&format!("{raw} BROKEN")).unwrap_err(),
            &secrets,
        );
    }

    #[test]
    fn saml_form_extracts_action_and_opaque_values() {
        let form = parse_auto_submit_form(SAML).unwrap();
        form.validate_saml().unwrap();
        assert_eq!(
            form.action(),
            "https://synthetic-hoppii.example.invalid/synthetic-saml/acs?requestId=synthetic-saml-request-id"
        );
        assert_eq!(
            form.field_value("SAMLResponse").unwrap(),
            Some("synthetic-saml-response-not-an-assertion")
        );
        assert_eq!(
            form.field_value("RelayState").unwrap(),
            Some("synthetic-relay-state")
        );
        assert_eq!(
            form.fields()
                .iter()
                .map(FormField::name)
                .collect::<Vec<_>>(),
            ["SAMLResponse", "RelayState"]
        );
        assert_eq!(form.field_value("absent").unwrap(), None);
    }

    #[test]
    fn saml_validation_does_not_depend_on_field_order_or_interpret_values() {
        let html = r#"<form action='/opaque'><input name='RelayState' value='not-a-relay-encoding'><input name='SAMLResponse' value='not-base64-or-xml'></form>"#;
        let form = parse_auto_submit_form(html).unwrap();
        form.validate_saml().unwrap();
        assert_eq!(form.fields()[0].name(), "RelayState");
        assert_eq!(form.fields()[1].value(), "not-base64-or-xml");
    }

    #[test]
    fn saml_validation_detects_each_missing_required_field() {
        for name in ["SAMLResponse", "RelayState"] {
            let html = SAML.replace(&format!("name=\"{name}\""), "");
            assert_eq!(
                parse_auto_submit_form(&html).unwrap().validate_saml(),
                Err(WebClassAuthParseError::MissingField(name))
            );
        }
    }

    #[test]
    fn csso_form_retains_all_six_fields_and_empty_password() {
        let form = parse_auto_submit_form(CSSO).unwrap();
        form.validate_hoppii_csso().unwrap();
        assert_eq!(form.action(), "/webclass/singlesignon.php");
        assert_eq!(form.fields().len(), 6);
        for (name, value) in [
            ("uid", "synthetic-uid"),
            ("password", ""),
            ("timestamp", "1900000000"),
            ("hash", "synthetic-csso-hash"),
            ("AC_LOGIN", "synthetic-login-action"),
            ("btnLogin", "Synthetic login"),
        ] {
            assert_eq!(form.field_value(name).unwrap(), Some(value));
        }
    }

    #[test]
    fn csso_validation_is_order_independent_and_leaves_values_opaque() {
        let html = r#"<form action='/opaque'><input name='btnLogin' value=''><input name='AC_LOGIN' value='other-action'><input name='hash' value='not-a-hash'><input name='timestamp' value='not-a-time'><input name='password' value=''><input name='uid' value='opaque-uid'></form>"#;
        let form = parse_auto_submit_form(html).unwrap();
        form.validate_hoppii_csso().unwrap();
        assert_eq!(form.field_value("timestamp").unwrap(), Some("not-a-time"));
        assert_eq!(form.field_value("hash").unwrap(), Some("not-a-hash"));
    }

    #[test]
    fn csso_validation_detects_each_missing_required_field() {
        for name in [
            "uid",
            "password",
            "timestamp",
            "hash",
            "AC_LOGIN",
            "btnLogin",
        ] {
            let html = CSSO.replace(&format!("name=\"{name}\""), "");
            assert_eq!(
                parse_auto_submit_form(&html)
                    .unwrap()
                    .validate_hoppii_csso(),
                Err(WebClassAuthParseError::MissingField(name))
            );
        }
    }

    #[test]
    fn html_parser_handles_entities_quote_styles_case_and_missing_value() {
        let html = r#"<FORM action='/bridge?a=1&amp;b=2'><div><INPUT NAME="SAMLResponse" VALUE="a&amp;b&lt;c&gt;&quot;d&#39;é"></div><input name=RelayState><input value='unnamed'><input name='' value='empty-name'></FORM><input name='outside' value='not-in-the-form'>"#;
        let form = parse_auto_submit_form(html).unwrap();
        form.validate_saml().unwrap();
        assert_eq!(form.action(), "/bridge?a=1&b=2");
        assert_eq!(
            form.field_value("SAMLResponse").unwrap(),
            Some("a&b<c>\"d'é")
        );
        assert_eq!(form.field_value("RelayState").unwrap(), Some(""));
        assert_eq!(form.fields().len(), 2);
        assert_eq!(form.field_value("outside").unwrap(), None);
    }

    #[test]
    fn duplicate_fields_are_retained_but_required_field_ambiguity_is_rejected() {
        let html = r#"<form action='/opaque'><input name='SAMLResponse' value='first'><input name='SAMLResponse' value='second'><input name='RelayState' value='opaque'></form>"#;
        let form = parse_auto_submit_form(html).unwrap();
        assert_eq!(form.fields().len(), 3);
        assert_eq!(form.fields()[0].value(), "first");
        assert_eq!(form.fields()[1].value(), "second");
        assert_eq!(
            form.validate_saml(),
            Err(WebClassAuthParseError::UnexpectedFormShape)
        );
        let html = CSSO.replace("</form>", "<input name='hash' value='second-hash'></form>");
        let form = parse_auto_submit_form(&html).unwrap();
        assert_eq!(form.fields().len(), 7);
        assert_eq!(
            form.validate_hoppii_csso(),
            Err(WebClassAuthParseError::UnexpectedFormShape)
        );
    }

    #[test]
    fn unknown_and_duplicate_extra_form_fields_are_preserved() {
        let html = SAML.replace(
            "</form>",
            "<input name='extra' value='first'><input name='extra' value='second'></form>",
        );
        let form = parse_auto_submit_form(&html).unwrap();
        form.validate_saml().unwrap();
        assert_eq!(form.fields().len(), 4);
        assert_eq!(form.fields()[2].name(), "extra");
        assert_eq!(form.fields()[3].value(), "second");
    }

    #[test]
    fn absent_multiple_and_actionless_forms_are_rejected() {
        for html in [
            "",
            "<p>no form</p>",
            "<form action='/first'></form><form action='/second'></form>",
        ] {
            assert_eq!(
                parse_auto_submit_form(html).unwrap_err(),
                WebClassAuthParseError::InvalidForm
            );
        }
        for html in [
            "<form></form>",
            "<form action=''></form>",
            "<form action='  '></form>",
        ] {
            assert_eq!(
                parse_auto_submit_form(html).unwrap_err(),
                WebClassAuthParseError::MissingField("action")
            );
        }
    }

    #[test]
    fn saml_form_and_individual_field_debug_and_errors_hide_secrets() {
        let secrets = [
            "synthetic-private-saml",
            "synthetic-private-relay",
            "synthetic-private-action-query",
            "synthetic-private-field-name",
        ];
        let html = format!(
            "<form action='/opaque?request={}'><input name='SAMLResponse' value='{}'><input name='RelayState' value='{}'><input name='{}' value='opaque'></form>",
            secrets[2], secrets[0], secrets[1], secrets[3]
        );
        let form = parse_auto_submit_form(&html).unwrap();
        assert_secret_free(
            &format!(
                "{form:?} {form:#?} {:?} {:#?}",
                form.fields(),
                form.fields()
            ),
            &secrets,
        );
        assert_safe_error(form.validate_hoppii_csso().unwrap_err(), &secrets);
        let missing = html.replace("name='SAMLResponse'", "");
        assert_safe_error(
            parse_auto_submit_form(&missing)
                .unwrap()
                .validate_saml()
                .unwrap_err(),
            &secrets,
        );
        assert_safe_error(
            parse_auto_submit_form(&format!("{html}{html}")).unwrap_err(),
            &secrets,
        );
    }

    #[test]
    fn csso_debug_and_errors_hide_all_opaque_values() {
        let secrets = [
            "synthetic-private-uid",
            "synthetic-private-password",
            "synthetic-private-timestamp",
            "synthetic-private-hash",
            "synthetic-private-action",
            "synthetic-private-button",
        ];
        let mut html = CSSO.to_owned();
        for (original, secret) in [
            "synthetic-uid",
            "",
            "1900000000",
            "synthetic-csso-hash",
            "synthetic-login-action",
            "Synthetic login",
        ]
        .into_iter()
        .zip(secrets)
        {
            html = html.replace(
                &format!("value=\"{original}\""),
                &format!("value=\"{secret}\""),
            );
        }
        let form = parse_auto_submit_form(&html).unwrap();
        form.validate_hoppii_csso().unwrap();
        assert_secret_free(&format!("{form:?} {form:#?} {:?}", form.fields()), &secrets);
        let missing = html.replace("name=\"hash\"", "");
        assert_safe_error(
            parse_auto_submit_form(&missing)
                .unwrap()
                .validate_hoppii_csso()
                .unwrap_err(),
            &secrets,
        );
    }

    #[test]
    fn js_redirect_fixtures_extract_location_and_restore_escaped_slashes() {
        assert_eq!(
            parse_webclass_js_redirect(JS).unwrap().as_deref(),
            Some("/webclass/login.php")
        );
        assert_eq!(
            parse_webclass_js_redirect(JS_ESCAPED).unwrap().as_deref(),
            Some("/webclass/")
        );
    }

    #[test]
    fn js_redirect_accepts_only_known_literal_direct_assignment_targets() {
        for target in [
            "window.top.location.href",
            "top.location.href",
            "window.location.href",
            "location.href",
            "window.location",
            "location",
        ] {
            for quote in ['\'', '"'] {
                for end in ["", ";"] {
                    let html = format!(
                        "<script> \n{target} \t= {quote}/webclass/課題?next=opaque{quote}{end} \n</script>"
                    );
                    assert_eq!(
                        parse_webclass_js_redirect(&html).unwrap().as_deref(),
                        Some("/webclass/課題?next=opaque")
                    );
                }
            }
        }
    }

    #[test]
    fn unrelated_js_comments_strings_and_other_targets_do_not_match() {
        for script in [
            "",
            "console.log('location')",
            "// window.top.location.href='/not-a-redirect';",
            "/* location.href='/not-a-redirect'; */",
            "const message = `location.href='/not-a-redirect'`;",
            "alert(\"window.location='/not-a-redirect';\");",
            "if (ready) { location.href='/unsupported'; }",
            "function go() { location='/unsupported'; }",
            "obj.location='/unsupported';",
            "mylocation='/unsupported';",
            "window.location.replace('/unsupported');",
            "window.location.assign('/unsupported');",
            "location.hash='/unsupported';",
            "location.hrefExtra='/unsupported';",
            "WINDOW.TOP.LOCATION.HREF='/unsupported';",
        ] {
            assert_eq!(
                parse_webclass_js_redirect(&format!("<script>{script}</script>")).unwrap(),
                None
            );
        }
        for html in [
            "<p>window.top.location.href='/plain-text';</p>",
            "<!-- <script>location='/comment';</script> -->",
            "<script src='/external.js'>location='/ignored';</script>",
            "<script type='application/json'>location='/ignored';</script>",
        ] {
            assert_eq!(parse_webclass_js_redirect(html).unwrap(), None);
        }
    }

    #[test]
    fn malformed_or_nonliteral_js_assignments_fail_without_panicking() {
        for value in [
            "",
            "42",
            "someVariable",
            "`/template`",
            "'/unterminated",
            "\"/mismatched'",
            "''",
            "'/one' + '/two'",
            "'/one'; alert('extra')",
            "'/one' || '/two'",
            "'/one', other()",
            "= '/comparison'",
            "> '/arrow'",
            "'/unsupported\\n'",
            "'/unsupported\\u002f'",
            "'/unsupported\\'",
            "'/literal\nnewline'",
            "'/literal\u{2028}separator'",
        ] {
            let html = format!("<script>window.top.location.href={value}</script>");
            assert_eq!(
                parse_webclass_js_redirect(&html).unwrap_err(),
                WebClassAuthParseError::InvalidRedirect
            );
        }
    }

    #[test]
    fn multiple_js_redirects_are_ambiguous_and_rejected() {
        let html = "<script>console.log('unrelated')</script><script>location='/first'</script><script>location='/second'</script>";
        assert_eq!(
            parse_webclass_js_redirect(html).unwrap_err(),
            WebClassAuthParseError::InvalidRedirect
        );
        let html = "<script>console.log('unrelated')</script><script type='text/javascript'>location='/only';</script>";
        assert_eq!(
            parse_webclass_js_redirect(html).unwrap().as_deref(),
            Some("/only")
        );
    }

    #[test]
    fn every_parse_error_has_secret_free_debug_display_and_source() {
        let secrets = [
            "synthetic-private-body",
            "synthetic-private-cookie",
            "synthetic-private-json",
        ];
        for error in [
            WebClassAuthParseError::InvalidJson,
            WebClassAuthParseError::UnexpectedOpenAmState,
            WebClassAuthParseError::MissingField("tokenId"),
            WebClassAuthParseError::InvalidForm,
            WebClassAuthParseError::UnexpectedFormShape,
            WebClassAuthParseError::InvalidRedirect,
        ] {
            assert_safe_error(error, &secrets);
        }
        assert_safe_error(
            parse_openam_state(&format!("{{\"authId\":\"{}\",BROKEN", secrets[2])).unwrap_err(),
            &secrets,
        );
        assert_safe_error(
            parse_auto_submit_form(&format!("<p>{}</p>", secrets[0])).unwrap_err(),
            &secrets,
        );
        let html = format!(
            "<script>location='{}' + '{}';</script>",
            secrets[0], secrets[1]
        );
        assert_safe_error(parse_webclass_js_redirect(&html).unwrap_err(), &secrets);
    }
}
