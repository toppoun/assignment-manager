//! Entirely offline tests. Every listener binds to loopback and uses synthetic values.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use reqwest::cookie::CookieStore;
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
const COURSES: &str = "/webclass/ip_mods.php/plugin/score_summary_table/courses";
const STUDENT: &str = "student-test";
const PASSWORD: &str = "synthetic-password";
const TOKEN: &str = "synthetic-openam-token";
const SECRETS: &[&str] = &[
    STUDENT,
    PASSWORD,
    TOKEN,
    "synthetic-saml-response-not-an-assertion",
    "synthetic-relay-state",
    "synthetic-uid",
    "1900000000",
    "synthetic-csso-hash",
    "synthetic-request-id",
    "synthetic-am-auth-cookie",
    "synthetic-consent-id",
    "synthetic-private-attribute",
    "synthetic-private-body",
    "synthetic-cookie",
];

fn assert_safe(error: HoseiAuthError) {
    let rendered = format!("{error:?} {error:#?} {error}");
    for secret in SECRETS {
        assert!(!rendered.contains(secret));
    }
    assert!(error.source().is_none());
}

fn xui_url(idp: &Url) -> Url {
    let mut url = endpoint(idp, XUI);
    let mut query = idp.clone();
    query.query_pairs_mut().extend_pairs([
        ("realm", "/"),
        ("forward", "true"),
        ("spEntityID", HOPPII_ORIGIN),
        ("askConsent", "true"),
        (
            "goto",
            "/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id",
        ),
        ("AMAuthCookie", "synthetic-am-auth-cookie"),
    ]);
    url.set_fragment(Some(&format!("login/&{}", query.query().unwrap())));
    url
}

#[test]
fn xui_fragment_decodes_and_reencodes_the_six_required_parameters() {
    let idp = Url::parse(IDP_ORIGIN).unwrap();
    let result = parse_xui_auth_url(&xui_url(&idp), &idp).unwrap();
    assert_eq!(result.path(), AUTHENTICATE);
    assert_eq!(result.origin(), idp.origin());
    assert!(result.fragment().is_none());
    let pairs: HashMap<_, _> = result.query_pairs().collect();
    assert_eq!(pairs.len(), 6);
    assert_eq!(pairs["realm"], "/");
    assert_eq!(
        pairs["goto"],
        "/openam/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id"
    );
    assert_eq!(pairs["spEntityID"], HOPPII_ORIGIN);
    assert_eq!(pairs["AMAuthCookie"], "synthetic-am-auth-cookie");
}

#[test]
fn xui_missing_duplicate_and_malformed_required_parameters_are_safe_errors() {
    let idp = Url::parse(IDP_ORIGIN).unwrap();
    let original = xui_url(&idp);
    let (_, query) = original.fragment().unwrap().split_once("/&").unwrap();
    let mut params = idp.clone();
    params.set_query(Some(query));
    for name in [
        "realm",
        "forward",
        "spEntityID",
        "askConsent",
        "goto",
        "AMAuthCookie",
    ] {
        for duplicate in [false, true] {
            let mut modified = idp.clone();
            modified.query_pairs_mut().extend_pairs(
                params
                    .query_pairs()
                    .filter(|(key, _)| duplicate || key != name),
            );
            if duplicate {
                modified
                    .query_pairs_mut()
                    .append_pair(name, "synthetic-private-body");
            }
            let mut url = original.clone();
            url.set_fragment(Some(&format!("login/&{}", modified.query().unwrap())));
            let error = parse_xui_auth_url(&url, &idp).unwrap_err();
            assert_eq!(
                error,
                HoseiAuthError::ProtocolChanged(AuthStep::IdpToOpenAm)
            );
            assert_safe(error);
        }
    }
    for fragment in [
        None,
        Some("login"),
        Some("login?realm=synthetic-private-body"),
        Some("login/&"),
        Some("/&realm=synthetic-private-body"),
    ] {
        let mut url = original.clone();
        url.set_fragment(fragment);
        assert_safe(parse_xui_auth_url(&url, &idp).unwrap_err());
    }
}

#[test]
fn xui_goto_preserves_openam_paths_encoded_values_and_blank_parameters() {
    let idp = Url::parse(IDP_ORIGIN).unwrap();
    for goto in [
        "/openam/return?a=1&b=%2F+#?",
        "https://idp.hosei.ac.jp/openam/return?a=1",
    ] {
        let mut xui = endpoint(&idp, XUI);
        let mut query = idp.clone();
        query.query_pairs_mut().extend_pairs([
            ("realm", "/"),
            ("forward", ""),
            ("spEntityID", "opaque + & ="),
            ("askConsent", "false"),
            ("goto", goto),
            ("AMAuthCookie", "opaque + & ="),
        ]);
        xui.set_fragment(Some(&format!("login/&{}", query.query().unwrap())));
        let result = parse_xui_auth_url(&xui, &idp).unwrap();
        assert_eq!(
            required_query_parameter(&result, "goto", AuthStep::IdpToOpenAm).unwrap(),
            goto
        );
        assert_eq!(
            required_query_parameter(&result, "forward", AuthStep::IdpToOpenAm).unwrap(),
            ""
        );
        assert_eq!(
            required_query_parameter(&result, "AMAuthCookie", AuthStep::IdpToOpenAm).unwrap(),
            "opaque + & ="
        );
    }
}

#[test]
fn destination_validation_checks_scheme_host_port_and_userinfo() {
    let origin = Url::parse(IDP_ORIGIN).unwrap();
    for target in [
        "/openam/return?secret=synthetic-private-body",
        "https://idp.hosei.ac.jp:443/openam/return",
    ] {
        assert!(resolve_destination(&origin, target, &origin, AuthStep::Saml).is_ok());
    }
    for target in [
        "http://idp.hosei.ac.jp/openam/",
        "https://idp.hosei.ac.jp:444/openam/",
        "https://idp.hosei.ac.jp.example.invalid/?token=synthetic-openam-token",
        "//outside.example.invalid/",
        "https://student-test:synthetic-password@idp.hosei.ac.jp/openam/",
        "javascript:location.href='synthetic-private-body'",
        "https://[broken",
    ] {
        assert_safe(resolve_destination(&origin, target, &origin, AuthStep::Saml).unwrap_err());
    }
    let a = Url::parse("http://127.0.0.1:10001").unwrap();
    let b = Url::parse("http://127.0.0.1:10002").unwrap();
    assert!(resolve_destination(&a, b.as_str(), &a, AuthStep::Saml).is_err());
}

#[test]
fn openam_session_cookie_is_secure_and_scoped_and_rejects_injection() {
    let endpoints = AuthEndpoints::production();
    let session = AuthSession::new(&endpoints).unwrap();
    session.install_openam_cookie(TOKEN).unwrap();
    assert!(session.jar.cookies(&endpoints.idp).is_some());
    assert!(
        session
            .jar
            .cookies(&Url::parse("https://idp.hosei.ac.jp/openam/return").unwrap())
            .is_some()
    );
    assert!(
        session
            .jar
            .cookies(&Url::parse("http://idp.hosei.ac.jp/").unwrap())
            .is_none()
    );
    assert!(session.jar.cookies(&endpoints.hoppii).is_none());
    assert!(session.jar.cookies(&endpoints.webclass).is_none());
    for token in [
        "",
        "synthetic-openam-token; Domain=outside",
        "synthetic-openam-token\r\n",
        "synthetic-openam-token,other",
        "synthetic-openam-token\\",
        "日本",
    ] {
        assert_safe(session.install_openam_cookie(token).unwrap_err());
    }
}

#[test]
fn every_public_authentication_error_contains_only_safe_categories() {
    for error in [
        HoseiAuthError::ClientInitialization,
        HoseiAuthError::Timeout(AuthStep::OpenAmCredentials),
        HoseiAuthError::Request(AuthStep::Saml),
        HoseiAuthError::HttpStatus(AuthStep::Consent, StatusCode::FORBIDDEN),
        HoseiAuthError::ProtocolChanged(AuthStep::OpenAmScripted2),
        HoseiAuthError::RedirectLimit(AuthStep::WebClassRedirect),
        HoseiAuthError::ConsentFailed,
        HoseiAuthError::HoppiiSessionFailed,
        HoseiAuthError::ApiVerificationFailed,
    ] {
        assert_safe(error);
    }
    for step in [
        AuthStep::HoppiiAcs,
        AuthStep::HoppiiPostAcs,
        AuthStep::HoppiiPortalLoginStart,
        AuthStep::HoppiiPortalLoginRedirect,
        AuthStep::HoppiiPortalVerify,
    ] {
        for error in [
            HoseiAuthError::Timeout(step),
            HoseiAuthError::Request(step),
            HoseiAuthError::HttpStatus(step, StatusCode::FORBIDDEN),
            HoseiAuthError::ProtocolChanged(step),
            HoseiAuthError::RedirectLimit(step),
        ] {
            assert_safe(error);
        }
    }
    for reason in [
        ProtocolReason::UnexpectedSuccessStatus,
        ProtocolReason::MissingLocation,
        ProtocolReason::BodyDecode,
        ProtocolReason::InvalidJsRedirect,
        ProtocolReason::InvalidTarget,
        ProtocolReason::UnexpectedOrigin,
        ProtocolReason::UserInfo,
    ] {
        assert_safe(HoseiAuthError::ProtocolChangedWithReason(
            AuthStep::WebClassRedirect,
            reason,
        ));
    }
}

struct RecordedRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    // Accepted sockets inherit nonblocking mode on Windows.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0 && bytes.len() + count < 65536);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let text = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let mut lines = text.lines();
    let first = lines.next().unwrap().split_whitespace().collect::<Vec<_>>();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    while bytes.len() < end + length {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0 && bytes.len() + count < 65536);
        bytes.extend_from_slice(&chunk[..count]);
    }
    RecordedRequest {
        method: first[0].to_owned(),
        target: first[1].to_owned(),
        headers,
        body: String::from_utf8(bytes[end..end + length].to_vec()).unwrap(),
    }
}

struct Exchange {
    method: &'static str,
    target: String,
    headers: Vec<(&'static str, String)>,
    cookies: Vec<(&'static str, &'static str)>,
    forbid_cookies: bool,
    forbid_referer: bool,
    json: Option<Value>,
    form: Option<Vec<(String, String)>>,
    status: u16,
    response_headers: Vec<(&'static str, String)>,
    body: String,
    response_bytes: Option<Vec<u8>>,
}

impl Exchange {
    fn new(method: &'static str, target: &str, status: u16, body: &str) -> Self {
        Self {
            method,
            target: target.to_owned(),
            headers: Vec::new(),
            cookies: Vec::new(),
            forbid_cookies: false,
            forbid_referer: false,
            json: None,
            form: None,
            status,
            response_headers: vec![("Content-Type", "text/html".to_owned())],
            body: body.to_owned(),
            response_bytes: None,
        }
    }
    fn redirect(method: &'static str, target: &str, location: &str) -> Self {
        Self::new(method, target, 302, "").response_header("Location", location)
    }
    fn response_header(mut self, name: &'static str, value: &str) -> Self {
        self.response_headers.push((name, value.to_owned()));
        self
    }
    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }
    fn cookie(mut self, name: &'static str, value: &'static str) -> Self {
        self.cookies.push((name, value));
        self
    }
    fn without_cookies(mut self) -> Self {
        self.forbid_cookies = true;
        self
    }
    fn without_referer(mut self) -> Self {
        self.forbid_referer = true;
        self
    }
    fn request_json(mut self, value: Value) -> Self {
        self.json = Some(value);
        self
    }
    fn request_form(mut self, html: &str) -> Self {
        self.form = Some(
            parse_auto_submit_form(html)
                .unwrap()
                .fields()
                .iter()
                .map(|field| (field.name().to_owned(), field.value().to_owned()))
                .collect(),
        );
        self
    }
    fn json_response(mut self) -> Self {
        self.response_headers[0].1 = "application/json".to_owned();
        self
    }
    fn assert_request(&self, request: &RecordedRequest) {
        assert_eq!(request.method, self.method);
        assert_eq!(request.target, self.target);
        assert_eq!(request.headers["user-agent"], USER_AGENT);
        if self.forbid_cookies {
            assert!(!request.headers.contains_key("cookie"));
        }
        if self.forbid_referer {
            assert!(!request.headers.contains_key("referer"));
        }
        if !request.headers["host"].starts_with("localhost:") {
            assert!(
                request
                    .headers
                    .get("cookie")
                    .is_none_or(|cookies| !cookies.contains("iPlanetDirectoryPro"))
            );
        }
        assert!(!request.target.contains(STUDENT));
        assert!(!request.target.contains(PASSWORD));
        for (name, value) in &self.headers {
            assert_eq!(request.headers.get(*name), Some(value));
        }
        for (name, value) in &self.cookies {
            assert!(
                request.headers["cookie"]
                    .split(';')
                    .map(str::trim)
                    .any(|cookie| cookie == format!("{name}={value}"))
            );
        }
        if let Some(expected) = &self.json {
            assert_eq!(request.headers["content-type"], "application/json");
            assert_eq!(
                serde_json::from_str::<Value>(&request.body).unwrap(),
                *expected
            );
        }
        if let Some(expected) = &self.form {
            assert_eq!(
                request.headers["content-type"],
                "application/x-www-form-urlencoded"
            );
            let mut encoded = Url::parse("http://localhost").unwrap();
            encoded.set_query(Some(&request.body));
            let actual = encoded
                .query_pairs()
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            assert_eq!(actual, *expected);
        }
        if self
            .json
            .as_ref()
            .is_none_or(|json| json.get("authId").is_none())
        {
            assert!(!request.body.contains(PASSWORD));
            assert!(!request.body.contains(STUDENT));
        }
    }
}

fn script(listener: TcpListener, exchanges: Vec<Exchange>) -> JoinHandle<TcpListener> {
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for exchange in exchanges {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("loopback mock did not receive expected request: {error}"),
                }
            };
            exchange.assert_request(&read_request(&mut stream));
            let body = exchange
                .response_bytes
                .as_deref()
                .unwrap_or(exchange.body.as_bytes());
            let mut headers = format!(
                "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
                exchange.status,
                body.len()
            );
            for (name, value) in exchange.response_headers {
                headers.push_str(&format!("{name}: {value}\r\n"));
            }
            headers.push_str("\r\n");
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        }
        listener
    })
}

fn finish(server: JoinHandle<TcpListener>) {
    let listener = server.join().unwrap();
    match listener.accept() {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
        Ok(_) => panic!("an unplanned request was sent"),
    }
}

struct FullFlow {
    endpoints: AuthEndpoints,
    listeners: [TcpListener; 3],
    exchanges: [Vec<Exchange>; 3],
}

impl FullFlow {
    fn new(consent: bool) -> Self {
        let listeners = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
        let endpoints = AuthEndpoints {
            hoppii: Url::parse(&format!("http://{}", listeners[0].local_addr().unwrap())).unwrap(),
            // A distinct host also verifies that the OpenAM cookie never reaches the SPs.
            idp: Url::parse(&format!(
                "http://localhost:{}",
                listeners[1].local_addr().unwrap().port()
            ))
            .unwrap(),
            webclass: Url::parse(&format!("http://{}", listeners[2].local_addr().unwrap()))
                .unwrap(),
            timeout: Duration::from_secs(3),
        };
        let idp_sso = endpoint(&endpoints.idp, "/idp/sso?request=synthetic-request-id");
        let auth = parse_xui_auth_url(&xui_url(&endpoints.idp), &endpoints.idp).unwrap();
        let auth_target = format!("{}?{}", auth.path(), auth.query().unwrap());
        let acs = endpoint(
            &endpoints.hoppii,
            "/saml/acs?requestId=synthetic-saml-request-id",
        );
        let saml = SAML.replace("https://synthetic-hoppii.example.invalid/synthetic-saml/acs?requestId=synthetic-saml-request-id", acs.as_str());
        let csso = CSSO.replace(
            "/webclass/singlesignon.php",
            endpoint(&endpoints.webclass, "/webclass/singlesignon.php").as_str(),
        );
        let mut initial: Value = serde_json::from_str(INITIAL).unwrap();
        initial["stage"] = json!("an-unfixed-initial-stage");
        initial["opaqueMetadata"] = json!({"preserve": [1, 2, 3]});
        initial["callbacks"][1]["input"][0]["value"] = json!(1);
        let mut expected_credentials = initial.clone();
        expected_credentials["callbacks"][1]["input"][0]["value"] = json!(0);
        expected_credentials["callbacks"][2]["input"][0]["value"] = json!(PASSWORD);
        expected_credentials["callbacks"][3]["input"][0]["value"] = json!(STUDENT);
        let openam = |exchange: Exchange| {
            exchange
                .header("accept-api-version", "protocol=1.0,resource=2.0")
                .header("x-requested-with", "XMLHttpRequest")
                .header("x-username", "anonymous")
                .header("x-password", "anonymous")
                .header("x-nosession", "true")
                .header("referer", endpoint(&endpoints.idp, XUI).as_str())
                .cookie("idp_test", "synthetic-cookie")
                .json_response()
        };
        let mut idp = vec![
            Exchange::redirect(
                "GET",
                "/idp/sso?request=synthetic-request-id",
                xui_url(&endpoints.idp).as_str(),
            )
            .response_header("Set-Cookie", "idp_test=synthetic-cookie; Path=/openam"),
            openam(Exchange::new(
                "POST",
                &auth_target,
                200,
                &initial.to_string(),
            )),
            openam(
                Exchange::new("POST", &auth_target, 200, SCRIPTED2)
                    .request_json(expected_credentials),
            ),
            openam(
                Exchange::new("POST", &auth_target, 200, SUCCESS)
                    .request_json(serde_json::from_str(SCRIPTED2).unwrap()),
            ),
        ];
        let success_target =
            "/openam/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id";
        if consent {
            let mut consent_url = endpoint(&endpoints.idp, CONSENT);
            consent_url.query_pairs_mut().extend_pairs([
                ("consentID", "synthetic-consent-id"),
                (
                    "originalRequestURI",
                    "/openam/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id",
                ),
            ]);
            idp.extend([
                Exchange::redirect("GET", success_target, consent_url.as_str()).cookie("iPlanetDirectoryPro", TOKEN),
                Exchange::new("POST", "/openam/json/saml2/consent/synthetic-consent-id?_action=getAttributes", 200,
                    r#"[{"attributeName":"mail","attributeValues":["synthetic-private-attribute"]},{"attributeName":"displayName"}]"#)
                    .json_response().header("accept-api-version", "protocol=1.0,resource=1.0")
                    .request_json(json!({"history": null})).cookie("iPlanetDirectoryPro", TOKEN),
                Exchange::new("POST", "/openam/json/saml2/consent/synthetic-consent-id?_action=agree", 204, "")
                    .header("accept-api-version", "protocol=1.0,resource=1.0")
                    .request_json(json!({"type": "ASK_IF_CHANGE", "attributes": ["mail", "displayName"], "history": null}))
                    .cookie("iPlanetDirectoryPro", TOKEN),
                Exchange::new("GET", &format!("{success_target}&consentID=synthetic-consent-id"), 200, &saml).cookie("iPlanetDirectoryPro", TOKEN),
            ]);
        } else {
            idp.push(
                Exchange::new("GET", success_target, 200, &saml)
                    .cookie("iPlanetDirectoryPro", TOKEN),
            );
        }
        let hoppii = vec![
            Exchange::redirect("GET", USER_PAGE, idp_sso.as_str()).without_cookies(),
            Exchange::redirect(
                "POST",
                "/saml/acs?requestId=synthetic-saml-request-id",
                "/portal/index.php?next=",
            )
            .request_form(&saml)
            .without_referer()
            .response_header("Set-Cookie", "hop_test=synthetic-cookie; Path=/portal"),
            Exchange::new("GET", PORTAL_PUBLIC, 200, "synthetic portal")
                .without_referer()
                .cookie("hop_test", "synthetic-cookie"),
            Exchange::redirect("GET", PORTAL_LOGIN, USER_PAGE)
                .header(
                    "referer",
                    endpoint(&endpoints.hoppii, PORTAL_PUBLIC).as_str(),
                )
                .cookie("hop_test", "synthetic-cookie")
                .response_header("Set-Cookie", "app_test=synthetic-cookie; Path=/portal"),
            Exchange::new("GET", USER_PAGE, 200, "synthetic user page")
                .header(
                    "referer",
                    endpoint(&endpoints.hoppii, PORTAL_PUBLIC).as_str(),
                )
                .cookie("app_test", "synthetic-cookie"),
            Exchange::new("GET", USER_PAGE, 200, "synthetic user page")
                .header(
                    "referer",
                    endpoint(&endpoints.hoppii, PORTAL_LOGIN).as_str(),
                )
                .cookie("app_test", "synthetic-cookie"),
            Exchange::new("GET", PARM, 200, &csso)
                .header("referer", endpoint(&endpoints.hoppii, USER_PAGE).as_str())
                .cookie("app_test", "synthetic-cookie"),
        ];
        let webclass = vec![
            Exchange::redirect("POST", "/webclass/singlesignon.php", "/webclass/after-sso")
                .header("referer", endpoint(&endpoints.hoppii, PARM).as_str())
                .header("origin", &endpoints.hoppii.origin().ascii_serialization())
                .request_form(&csso)
                .response_header("Set-Cookie", "wc_test=synthetic-cookie; Path=/webclass"),
            Exchange::new("GET", "/webclass/after-sso", 200, JS)
                .header(
                    "referer",
                    endpoint(&endpoints.webclass, "/webclass/singlesignon.php").as_str(),
                )
                .cookie("wc_test", "synthetic-cookie"),
            Exchange::new("GET", "/webclass/login.php", 200, JS_ESCAPED)
                .header(
                    "referer",
                    endpoint(&endpoints.webclass, "/webclass/after-sso").as_str(),
                )
                .cookie("wc_test", "synthetic-cookie"),
            Exchange::new("GET", "/webclass/", 200, "synthetic dashboard")
                .header(
                    "referer",
                    endpoint(&endpoints.webclass, "/webclass/login.php").as_str(),
                )
                .cookie("wc_test", "synthetic-cookie"),
            // Deliberately incompatible with WebClassCourse: schema is not an auth criterion.
            Exchange::new("GET", COURSES, 200, r#"[{"futureSchema":true},null,42]"#)
                .json_response()
                .header("accept", "application/json")
                .header(
                    "referer",
                    endpoint(
                        &endpoints.webclass,
                        "/webclass/ip_mods.php/plugin/score_summary_table/dashboard",
                    )
                    .as_str(),
                )
                .cookie("wc_test", "synthetic-cookie"),
        ];
        Self {
            endpoints,
            listeners,
            exchanges: [hoppii, idp, webclass],
        }
    }

    fn run(
        mut self,
        authenticator: &HoseiAuthenticator,
        use_returned_client: bool,
    ) -> Result<WebClassClient, HoseiAuthError> {
        if use_returned_client {
            self.exchanges[2].push(
                Exchange::new("GET", COURSES, 200, "[]")
                    .json_response()
                    .cookie("wc_test", "synthetic-cookie"),
            );
        }
        let servers = self
            .listeners
            .into_iter()
            .zip(self.exchanges)
            .map(|(listener, exchanges)| script(listener, exchanges))
            .collect::<Vec<_>>();
        let result = authenticator
            .login_with_endpoints(HoseiCredentials::new(STUDENT, PASSWORD), &self.endpoints);
        if use_returned_client {
            assert_eq!(
                result
                    .as_ref()
                    .expect("mock flow must succeed")
                    .fetch_courses_json()
                    .unwrap(),
                "[]"
            );
        }
        for server in servers {
            finish(server);
        }
        result
    }
}

#[test]
fn full_flow_without_consent_reaches_courses_and_returns_the_same_authenticated_session() {
    FullFlow::new(false)
        .run(&HoseiAuthenticator::new(), true)
        .expect("full mock flow");
}

#[test]
fn full_flow_with_consent_forwards_attribute_names_and_reaches_courses() {
    FullFlow::new(true)
        .run(&HoseiAuthenticator::new(), true)
        .expect("consent mock flow");
}

#[test]
fn portal_login_redirects_keep_initial_referer_and_shared_cookies() {
    for status in [301, 302, 303] {
        let mut flow = FullFlow::new(false);
        let referer = endpoint(&flow.endpoints.hoppii, PORTAL_PUBLIC);
        let first_target = "/portal/login/first-hop";
        let second_target = "/portal/login/second-hop";

        // The existing initial GET checks the same Referer and the HOPPII cookie.
        flow.exchanges[0][3].status = status;
        flow.exchanges[0][3].response_headers[1].1 = first_target.to_owned();
        let mut first_redirect = Exchange::redirect("GET", first_target, second_target)
            .header("referer", referer.as_str())
            .cookie("hop_test", "synthetic-cookie")
            .cookie("app_test", "synthetic-cookie")
            .response_header("Set-Cookie", "redirect_test=synthetic-cookie; Path=/portal");
        first_redirect.status = status;
        flow.exchanges[0][4] = first_redirect;
        flow.exchanges[0].insert(
            5,
            Exchange::new("GET", second_target, 200, "synthetic user page")
                .header("referer", referer.as_str())
                .cookie("hop_test", "synthetic-cookie")
                .cookie("app_test", "synthetic-cookie")
                .cookie("redirect_test", "synthetic-cookie"),
        );
        // Verification keeps its own Referer and receives the redirect's cookie.
        flow.exchanges[0][6]
            .cookies
            .push(("redirect_test", "synthetic-cookie"));
        flow.run(&HoseiAuthenticator::new(), true)
            .expect("portal redirects must preserve the fixed Referer and session");
    }
}

#[test]
fn consecutive_logins_start_with_new_clients_and_empty_cookie_jars() {
    let authenticator = HoseiAuthenticator::new();
    for _ in 0..2 {
        FullFlow::new(false)
            .run(&authenticator, false)
            .expect("new login");
    }
}

#[test]
fn api_verification_rejects_auth_expired_non_json_and_non_arrays() {
    for (status, content_type, body, location) in [
        (
            302,
            "text/html",
            "",
            Some("/webclass/login.php?token=synthetic-openam-token"),
        ),
        (
            200,
            "text/html",
            "<script>location='/webclass/login.php'</script>",
            None,
        ),
        (200, "application/json", "synthetic-private-body", None),
        (
            200,
            "application/json",
            r#"{"private":"synthetic-private-body"}"#,
            None,
        ),
        (200, "application/json", "null", None),
    ] {
        let mut flow = FullFlow::new(false);
        let response = flow.exchanges[2].last_mut().unwrap();
        response.status = status;
        response.body = body.to_owned();
        response.response_headers[0].1 = content_type.to_owned();
        if let Some(location) = location {
            response
                .response_headers
                .push(("Location", location.to_owned()));
        }
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("API must reject response");
        assert_eq!(error, HoseiAuthError::ApiVerificationFailed);
        assert_safe(error);
    }
}

#[test]
fn openam_unknown_callbacks_and_unconfirmed_failure_shapes_stop_the_flow() {
    for (stage, body, expected) in [
        (
            1,
            INITIAL.replace("NameCallback", "SyntheticMfaCallback"),
            AuthStep::OpenAmCredentials,
        ),
        (2, INITIAL.to_owned(), AuthStep::OpenAmScripted2),
        (
            2,
            r#"{"message":"synthetic-password"}"#.to_owned(),
            AuthStep::OpenAmCredentials,
        ),
        (
            3,
            r#"{"tokenId":"synthetic-openam-token"}"#.to_owned(),
            AuthStep::OpenAmSuccess,
        ),
    ] {
        let mut flow = FullFlow::new(false);
        flow.exchanges[1][stage].body = body;
        flow.exchanges[1].truncate(stage + 1);
        flow.exchanges[0].truncate(1);
        flow.exchanges[2].clear();
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("unknown OpenAM state");
        assert_eq!(error, HoseiAuthError::ProtocolChanged(expected));
        assert_safe(error);
    }
}

#[test]
fn consent_invalid_attributes_and_http_failure_are_secret_safe() {
    for (index, status, body, expected) in [
        (
            5,
            200,
            r#"{"attributeName":"synthetic-private-attribute"}"#,
            HoseiAuthError::ConsentFailed,
        ),
        (
            5,
            200,
            r#"[{"attributeName":42}]"#,
            HoseiAuthError::ConsentFailed,
        ),
        (
            6,
            403,
            "synthetic-private-body",
            HoseiAuthError::HttpStatus(AuthStep::Consent, StatusCode::FORBIDDEN),
        ),
    ] {
        let mut flow = FullFlow::new(true);
        flow.exchanges[1][index].status = status;
        flow.exchanges[1][index].body = body.to_owned();
        flow.exchanges[1].truncate(index + 1);
        flow.exchanges[0].truncate(1);
        flow.exchanges[2].clear();
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("consent must fail");
        assert_eq!(error, expected);
        assert_safe(error);
    }
}

#[test]
fn webclass_redirect_response_failures_report_safe_reasons() {
    let cases: &[(u16, Option<&str>, &[u8], ProtocolReason)] = &[
        (
            201,
            None,
            b"synthetic-private-body",
            ProtocolReason::UnexpectedSuccessStatus,
        ),
        (204, None, b"", ProtocolReason::UnexpectedSuccessStatus),
        (
            299,
            None,
            b"synthetic-private-body",
            ProtocolReason::UnexpectedSuccessStatus,
        ),
        (
            301,
            None,
            b"synthetic-private-body",
            ProtocolReason::MissingLocation,
        ),
        (
            302,
            Some(""),
            b"synthetic-private-body",
            ProtocolReason::MissingLocation,
        ),
        (
            303,
            Some(" \t "),
            b"synthetic-private-body",
            ProtocolReason::MissingLocation,
        ),
        (
            307,
            Some("\u{e9}"),
            b"synthetic-private-body",
            ProtocolReason::MissingLocation,
        ),
        (
            308,
            None,
            b"synthetic-private-body",
            ProtocolReason::MissingLocation,
        ),
        (200, None, &[0xff, 0xfe], ProtocolReason::BodyDecode),
    ];
    for &(status, location, body, reason) in cases {
        // Exercise both the SSO POST response and a subsequent redirect GET response.
        for index in [0, 1] {
            let mut flow = FullFlow::new(false);
            let response = &mut flow.exchanges[2][index];
            response.status = status;
            response.response_bytes = Some(body.to_vec());
            response
                .response_headers
                .retain(|(name, _)| *name != "Location");
            if let Some(location) = location {
                response
                    .response_headers
                    .push(("Location", location.to_owned()));
            }
            assert_webclass_protocol_failure(flow, index, reason);
        }
    }
}

fn assert_webclass_protocol_failure(mut flow: FullFlow, index: usize, reason: ProtocolReason) {
    flow.exchanges[2].truncate(index + 1);
    let error = flow
        .run(&HoseiAuthenticator::new(), false)
        .err()
        .expect("WebClass protocol failure must stop the flow");
    assert_eq!(
        error,
        HoseiAuthError::ProtocolChangedWithReason(AuthStep::WebClassRedirect, reason)
    );
    assert_eq!(
        error.to_string(),
        format!("unexpected Hosei authentication protocol at WebClassRedirect: {reason:?}")
    );
    assert_eq!(
        format!("{error:?}"),
        format!("ProtocolChangedWithReason(WebClassRedirect, {reason:?})")
    );
    assert_safe(error);
}

#[test]
fn webclass_redirect_target_failures_report_reasons_without_following_the_target() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    for reason in [
        ProtocolReason::InvalidTarget,
        ProtocolReason::UnexpectedOrigin,
        ProtocolReason::UserInfo,
    ] {
        for js_redirect in [false, true] {
            for index in [0, 1] {
                let mut flow = FullFlow::new(false);
                let target = match reason {
                    ProtocolReason::InvalidTarget => {
                        "http://[broken?secret=synthetic-private-body".to_owned()
                    }
                    ProtocolReason::UnexpectedOrigin => format!(
                        "http://{}/?secret=synthetic-private-body",
                        trap.local_addr().unwrap()
                    ),
                    ProtocolReason::UserInfo => {
                        let mut target = endpoint(&flow.endpoints.webclass, "/webclass/");
                        target.set_username(STUDENT).unwrap();
                        target.set_password(Some(PASSWORD)).unwrap();
                        target.query_pairs_mut().append_pair("token", TOKEN);
                        target.to_string()
                    }
                    _ => unreachable!(),
                };
                let response = &mut flow.exchanges[2][index];
                response
                    .response_headers
                    .retain(|(name, _)| *name != "Location");
                if js_redirect {
                    response.status = 200;
                    response.body = format!("<script>location='{target}';</script>");
                } else {
                    response.status = 302;
                    response.body = SECRETS.join(" ");
                    response.response_headers.push(("Location", target));
                }
                assert_webclass_protocol_failure(flow, index, reason);
                assert_eq!(
                    trap.accept().err().unwrap().kind(),
                    std::io::ErrorKind::WouldBlock
                );
            }
        }
    }
}

#[test]
fn webclass_200_with_known_js_redirect_still_follows_the_target() {
    let mut flow = FullFlow::new(false);
    let response = &mut flow.exchanges[2][0];
    response.status = 200;
    response.body = "<script>location='/webclass/after-sso';</script>".to_owned();
    response
        .response_headers
        .retain(|(name, _)| *name != "Location");
    flow.run(&HoseiAuthenticator::new(), true)
        .expect("known JS redirects must still be followed before API verification");
}

#[test]
fn webclass_invalid_js_ends_navigation_but_login_requires_an_api_array() {
    for body in [
        "<script>location=syntheticValue;</script>",
        "<script>location='synthetic-private-body';</script><script>location='synthetic-cookie';</script>",
        "<script>location='/webclass/not-followed'; extraFunction();</script>",
    ] {
        // Phase 5B remains strict; only the consumer's handling of this error changes.
        assert_eq!(
            parse_webclass_js_redirect(body),
            Err(WebClassAuthParseError::InvalidRedirect)
        );
        for index in [0, 1] {
            for (status, api_body, location, expected_error) in [
                (200, "[]", None, None),
                (
                    302,
                    "",
                    Some("/webclass/login.php?token=synthetic-openam-token"),
                    Some(HoseiAuthError::ApiVerificationFailed),
                ),
                (
                    200,
                    r#"{"private":"synthetic-private-body"}"#,
                    None,
                    Some(HoseiAuthError::ApiVerificationFailed),
                ),
                (
                    200,
                    "synthetic-private-body",
                    None,
                    Some(HoseiAuthError::ApiVerificationFailed),
                ),
            ] {
                let mut flow = FullFlow::new(false);
                let mut courses = flow.exchanges[2].pop().unwrap();
                courses.status = status;
                courses.body = api_body.to_owned();
                if let Some(location) = location {
                    courses.response_headers[0].1 = "text/html".to_owned();
                    courses
                        .response_headers
                        .push(("Location", location.to_owned()));
                }
                flow.exchanges[2].truncate(index + 1);
                let response = &mut flow.exchanges[2][index];
                response.status = 200;
                response.body = body.to_owned();
                response
                    .response_headers
                    .retain(|(name, _)| *name != "Location");
                // The next permitted request is the fixed courses API, not a guessed target.
                flow.exchanges[2].push(courses);
                let result = flow.run(&HoseiAuthenticator::new(), expected_error.is_none());
                if let Some(expected) = expected_error {
                    let error = result
                        .err()
                        .expect("API verification must reject this response");
                    assert_eq!(error, expected);
                    assert_safe(error);
                } else {
                    assert!(result.is_ok());
                }
            }
        }
    }
}

#[test]
fn webclass_redirect_http_errors_still_stop_before_api_verification() {
    for status in [403, 500] {
        for index in [0, 1] {
            let mut flow = FullFlow::new(false);
            flow.exchanges[2].truncate(index + 1);
            let response = &mut flow.exchanges[2][index];
            response.status = status;
            response.body = SECRETS.join(" ");
            let error = flow
                .run(&HoseiAuthenticator::new(), false)
                .err()
                .expect("HTTP errors must stop navigation without API verification");
            assert_eq!(
                error,
                HoseiAuthError::HttpStatus(
                    AuthStep::WebClassRedirect,
                    StatusCode::from_u16(status).unwrap(),
                )
            );
            assert_safe(error);
        }
    }
}

#[test]
fn webclass_200_without_js_redirect_still_reaches_api_verification() {
    for content_type in ["text/html", "application/json", "application/octet-stream"] {
        let mut flow = FullFlow::new(false);
        let courses = flow.exchanges[2].pop().unwrap();
        flow.exchanges[2].truncate(1);
        let response = &mut flow.exchanges[2][0];
        response.status = 200;
        response.body = "<p>synthetic-private-body</p>".to_owned();
        response
            .response_headers
            .retain(|(name, _)| *name != "Location" && *name != "Content-Type");
        response
            .response_headers
            .push(("Content-Type", content_type.to_owned()));
        flow.exchanges[2].push(courses);
        flow.run(&HoseiAuthenticator::new(), true)
            .expect("no JS redirect must proceed to API verification");
    }
}

#[test]
fn hoppii_http_failures_identify_the_request_step_without_exposing_secrets() {
    let location = "/portal/diagnostic?token=synthetic-openam-token&uid=synthetic-uid";
    for (index, step, after_redirect) in [
        (1, AuthStep::HoppiiAcs, false),
        (2, AuthStep::HoppiiPostAcs, false),
        (2, AuthStep::HoppiiPostAcs, true),
        (3, AuthStep::HoppiiPortalLoginStart, false),
        (4, AuthStep::HoppiiPortalLoginRedirect, false),
        (4, AuthStep::HoppiiPortalLoginRedirect, true),
        (5, AuthStep::HoppiiPortalVerify, false),
        (5, AuthStep::HoppiiPortalVerify, true),
    ] {
        let mut flow = FullFlow::new(false);
        flow.exchanges[0].truncate(index + 1);
        flow.exchanges[2].clear();
        if after_redirect {
            flow.exchanges[0][index].status = 302;
            flow.exchanges[0][index]
                .response_headers
                .push(("Location", location.to_owned()));
            let failure = Exchange::new("GET", location, 403, "");
            let failure = if step == AuthStep::HoppiiPortalLoginRedirect {
                failure.header(
                    "referer",
                    endpoint(&flow.endpoints.hoppii, PORTAL_PUBLIC).as_str(),
                )
            } else {
                failure.without_referer()
            };
            flow.exchanges[0].push(failure);
        } else {
            flow.exchanges[0][index].status = 403;
        }
        let failure = flow.exchanges[0].last_mut().unwrap();
        failure.body = SECRETS.join(" ");
        failure
            .response_headers
            .retain(|(name, _)| *name != "Location");
        failure
            .response_headers
            .push(("Location", location.to_owned()));
        failure.response_headers.push((
            "Set-Cookie",
            "private_session=synthetic-cookie; Path=/".to_owned(),
        ));

        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("HTTP failure must stop the flow");
        assert_eq!(
            error,
            HoseiAuthError::HttpStatus(step, StatusCode::FORBIDDEN)
        );
        assert_eq!(
            error.to_string(),
            format!("Hosei authentication returned HTTP 403 Forbidden at {step:?}")
        );
        assert_safe(error);
    }
}

#[test]
fn hoppii_final_user_page_is_required_before_csso() {
    let mut flow = FullFlow::new(false);
    flow.exchanges[0][5] = Exchange::redirect("GET", USER_PAGE, "/portal/anonymous");
    flow.exchanges[0].truncate(6);
    flow.exchanges[0].push(Exchange::new(
        "GET",
        "/portal/anonymous",
        200,
        "synthetic-private-body",
    ));
    flow.exchanges[2].clear();
    let error = flow
        .run(&HoseiAuthenticator::new(), false)
        .err()
        .expect("portal must fail");
    assert_eq!(error, HoseiAuthError::HoppiiSessionFailed);
    assert_safe(error);
}

#[test]
fn untrusted_destinations_stop_before_any_outbound_request() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let malicious = format!(
        "http://{}/?secret=synthetic-private-body",
        trap.local_addr().unwrap()
    );
    for (case, expected) in [
        (0, AuthStep::HoppiiToIdp),
        (1, AuthStep::IdpToOpenAm),
        (2, AuthStep::OpenAmSuccess),
        (3, AuthStep::Saml),
        (4, AuthStep::HoppiiAcs),
        (5, AuthStep::WebClassSso),
        (6, AuthStep::WebClassRedirect),
        (7, AuthStep::WebClassRedirect),
        (8, AuthStep::Consent),
        (9, AuthStep::HoppiiPostAcs),
        (10, AuthStep::HoppiiPortalLoginStart),
        (11, AuthStep::HoppiiPortalVerify),
        (12, AuthStep::HoppiiPortalLoginRedirect),
    ] {
        let mut flow = FullFlow::new(case == 8);
        match case {
            0 => {
                // Replace, rather than append, the Location header to avoid ambiguity.
                flow.exchanges[0][0]
                    .response_headers
                    .retain(|(name, _)| *name != "Location");
                flow.exchanges[0][0]
                    .response_headers
                    .push(("Location", malicious.clone()));
                flow.exchanges[0].truncate(1);
                flow.exchanges[1].clear();
                flow.exchanges[2].clear();
            }
            1 => {
                flow.exchanges[1][0].response_headers[1].1 = malicious.clone();
                flow.exchanges[1].truncate(1);
                flow.exchanges[0].truncate(1);
                flow.exchanges[2].clear();
            }
            2 => {
                flow.exchanges[1][3].body =
                    json!({"tokenId":TOKEN,"successUrl":malicious}).to_string();
                flow.exchanges[1].truncate(4);
                flow.exchanges[0].truncate(1);
                flow.exchanges[2].clear();
            }
            3 => {
                flow.exchanges[1][4].body = format!(
                    "<form action='{malicious}'><input name='SAMLResponse' value='synthetic-saml-response-not-an-assertion'><input name='RelayState' value='synthetic-relay-state'></form>"
                );
                flow.exchanges[0].truncate(1);
                flow.exchanges[2].clear();
            }
            4 => {
                flow.exchanges[0][1].response_headers[1].1 = malicious.clone();
                flow.exchanges[0].truncate(2);
                flow.exchanges[2].clear();
            }
            5 => {
                let action = endpoint(&flow.endpoints.webclass, "/webclass/singlesignon.php");
                flow.exchanges[0][6].body = flow.exchanges[0][6]
                    .body
                    .replace(action.as_str(), &malicious);
                flow.exchanges[2].clear();
            }
            6 => {
                flow.exchanges[2][0].response_headers[1].1 = malicious.clone();
                flow.exchanges[2].truncate(1);
            }
            7 => {
                flow.exchanges[2][1].body =
                    format!("<script>location.href='{malicious}';</script>");
                flow.exchanges[2].truncate(2);
            }
            8 => {
                let mut url = endpoint(&flow.endpoints.idp, CONSENT);
                url.query_pairs_mut()
                    .append_pair("consentID", "synthetic-consent-id")
                    .append_pair("originalRequestURI", &malicious);
                flow.exchanges[1][4].response_headers[1].1 = url.to_string();
                flow.exchanges[1].truncate(5);
                flow.exchanges[0].truncate(1);
                flow.exchanges[2].clear();
            }
            9..=12 => {
                let index = match case {
                    9 => 2,
                    10 => 3,
                    11 => 5,
                    12 => 4,
                    _ => unreachable!(),
                };
                flow.exchanges[0][index].status = 302;
                flow.exchanges[0][index]
                    .response_headers
                    .retain(|(name, _)| *name != "Location");
                flow.exchanges[0][index]
                    .response_headers
                    .push(("Location", malicious.clone()));
                flow.exchanges[0].truncate(index + 1);
                flow.exchanges[2].clear();
            }
            _ => unreachable!(),
        }
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("destination must be blocked");
        let expected_error = if expected == AuthStep::WebClassRedirect {
            HoseiAuthError::ProtocolChangedWithReason(expected, ProtocolReason::UnexpectedOrigin)
        } else {
            HoseiAuthError::ProtocolChanged(expected)
        };
        assert_eq!(error, expected_error);
        assert_safe(error);
        assert_eq!(
            trap.accept().err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn redirect_limits_bound_hoppii_http_and_webclass_http_or_js_loops() {
    for (hoppii_index, js_redirect, expected) in [
        (Some(1), false, AuthStep::HoppiiPostAcs),
        (Some(3), false, AuthStep::HoppiiPortalLoginRedirect),
        (Some(5), false, AuthStep::HoppiiPortalVerify),
        (None, false, AuthStep::WebClassRedirect),
        (None, true, AuthStep::WebClassRedirect),
    ] {
        let mut flow = FullFlow::new(false);
        if let Some(index) = hoppii_index {
            flow.exchanges[0].truncate(index + 1);
            flow.exchanges[0][index].status = 302;
            flow.exchanges[0][index]
                .response_headers
                .retain(|(name, _)| *name != "Location");
            flow.exchanges[0][index]
                .response_headers
                .push(("Location", "/portal/loop".to_owned()));
            flow.exchanges[0].extend(
                (0..MAX_REDIRECTS)
                    .map(|_| Exchange::redirect("GET", "/portal/loop", "/portal/loop")),
            );
            flow.exchanges[2].clear();
        } else {
            flow.exchanges[2].truncate(1);
            flow.exchanges[2][0].response_headers[1].1 = "/webclass/loop".to_owned();
            flow.exchanges[2].extend((0..MAX_REDIRECTS).map(|_| {
                if !js_redirect {
                    Exchange::redirect("GET", "/webclass/loop", "/webclass/loop")
                } else {
                    Exchange::new(
                        "GET",
                        "/webclass/loop",
                        200,
                        "<script>location.href='\\/webclass\\/loop';</script>",
                    )
                }
            }));
        }
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("loop must stop");
        assert_eq!(error, HoseiAuthError::RedirectLimit(expected));
        assert_safe(error);
    }
}

#[test]
fn request_failure_redacts_the_raw_url_and_transport_cause() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/?secret=synthetic-password",
        listener.local_addr().unwrap()
    );
    drop(listener);
    let endpoints = AuthEndpoints::production();
    let session = AuthSession::new(&endpoints).unwrap();
    let error = send(session.http.get(url), AuthStep::OpenAmCredentials)
        .err()
        .unwrap();
    assert_eq!(error, HoseiAuthError::Request(AuthStep::OpenAmCredentials));
    assert_safe(error);
}

#[test]
fn xui_rejects_external_goto_wrong_origin_and_wrong_path() {
    let idp = Url::parse(IDP_ORIGIN).unwrap();
    for goto in [
        "https://outside.example.invalid/?secret=synthetic-private-body",
        "//outside.example.invalid/",
        "https://student-test:synthetic-password@idp.hosei.ac.jp/openam/",
    ] {
        let original = xui_url(&idp);
        let (_, query) = original.fragment().unwrap().split_once("/&").unwrap();
        let mut params = idp.clone();
        params.set_query(Some(query));
        let mut modified = idp.clone();
        modified
            .query_pairs_mut()
            .extend_pairs(params.query_pairs().filter(|(name, _)| name != "goto"));
        modified.query_pairs_mut().append_pair("goto", goto);
        let mut xui = original;
        xui.set_fragment(Some(&format!("login/&{}", modified.query().unwrap())));
        assert_safe(parse_xui_auth_url(&xui, &idp).unwrap_err());
    }
    for target in [
        "https://outside.example.invalid/openam/XUI/",
        "https://idp.hosei.ac.jp/other",
    ] {
        let mut xui = Url::parse(target).unwrap();
        xui.set_fragment(xui_url(&idp).fragment());
        assert_safe(parse_xui_auth_url(&xui, &idp).unwrap_err());
    }
}

#[test]
fn full_flow_preserves_opaque_form_values_through_url_encoding() {
    let mut flow = FullFlow::new(false);
    let saml = "opaque +/=&<>\"";
    let relay = "opaque relay + & = 日本";
    flow.exchanges[1][4].body = flow.exchanges[1][4]
        .body
        .replace(
            "synthetic-saml-response-not-an-assertion",
            "opaque +/=&amp;&lt;&gt;&quot;",
        )
        .replace("synthetic-relay-state", "opaque relay + &amp; = 日本");
    flow.exchanges[0][1].form = Some(vec![
        ("SAMLResponse".to_owned(), saml.to_owned()),
        ("RelayState".to_owned(), relay.to_owned()),
    ]);
    flow.exchanges[0][6].body = flow.exchanges[0][6]
        .body
        .replace("synthetic-csso-hash", "opaque +/=&amp;&lt;&gt;&quot;");
    let form = flow.exchanges[2][0].form.as_mut().unwrap();
    form.iter_mut().find(|(name, _)| name == "hash").unwrap().1 = saml.to_owned();
    flow.run(&HoseiAuthenticator::new(), false)
        .expect("opaque fields must roundtrip");
}

#[test]
fn exactly_ten_webclass_redirects_can_complete_successfully() {
    let mut flow = FullFlow::new(false);
    let courses = flow.exchanges[2].pop().unwrap();
    flow.exchanges[2].truncate(1);
    flow.exchanges[2][0].response_headers[1].1 = "/webclass/redirect-0".to_owned();
    for index in 0..MAX_REDIRECTS {
        let path = format!("/webclass/redirect-{index}");
        let response = if index + 1 == MAX_REDIRECTS {
            Exchange::new("GET", &path, 200, "synthetic dashboard")
        } else {
            Exchange::redirect("GET", &path, &format!("/webclass/redirect-{}", index + 1))
        };
        flow.exchanges[2].push(response.cookie("wc_test", "synthetic-cookie"));
    }
    flow.exchanges[2].push(courses);
    flow.run(&HoseiAuthenticator::new(), false)
        .expect("redirect limit is inclusive");
}

#[test]
fn missing_or_empty_location_and_http_failure_stop_before_credentials() {
    for (status, location, expected) in [
        (
            302,
            None,
            HoseiAuthError::ProtocolChanged(AuthStep::HoppiiToIdp),
        ),
        (
            302,
            Some(""),
            HoseiAuthError::ProtocolChanged(AuthStep::HoppiiToIdp),
        ),
        (
            200,
            None,
            HoseiAuthError::ProtocolChanged(AuthStep::HoppiiToIdp),
        ),
        (
            503,
            None,
            HoseiAuthError::HttpStatus(AuthStep::HoppiiToIdp, StatusCode::SERVICE_UNAVAILABLE),
        ),
    ] {
        let mut flow = FullFlow::new(false);
        flow.exchanges[0][0].status = status;
        flow.exchanges[0][0].body = "synthetic-private-body".to_owned();
        flow.exchanges[0][0]
            .response_headers
            .retain(|(name, _)| *name != "Location");
        if let Some(location) = location {
            flow.exchanges[0][0]
                .response_headers
                .push(("Location", location.to_owned()));
        }
        flow.exchanges[0].truncate(1);
        flow.exchanges[1].clear();
        flow.exchanges[2].clear();
        let error = flow
            .run(&HoseiAuthenticator::new(), false)
            .err()
            .expect("unexpected response");
        assert_eq!(error, expected);
        assert_safe(error);
    }
}

#[test]
fn finite_timeout_covers_headers_and_response_body_without_exposing_secrets() {
    for body_timeout in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (release, waiting) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            if body_timeout {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
            waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let endpoints = AuthEndpoints {
            hoppii: origin.clone(),
            idp: origin.clone(),
            webclass: origin.clone(),
            timeout: Duration::from_millis(150),
        };
        let session = AuthSession::new(&endpoints).unwrap();
        let step = AuthStep::OpenAmCredentials;
        let result = send(
            session
                .http
                .get(endpoint(&origin, "/?secret=synthetic-password")),
            step,
        );
        let error = if body_timeout {
            read_ok(result.unwrap(), step).unwrap_err()
        } else {
            result.err().unwrap()
        };
        release.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(error, HoseiAuthError::Timeout(step));
        assert_safe(error);
    }
}
