// Shared synthetic protocol script; included only by tests/offline fixture.
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

struct RecordedRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: String,
}

fn read_request(stream: &mut impl Read) -> RecordedRequest {
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

#[derive(Clone)]
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
    allow_body_disconnect: bool,
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
            allow_body_disconnect: false,
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
        if !request.headers["host"].starts_with("localhost:")
            && request.headers["host"] != "idp.synthetic.invalid"
        {
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
    script_with_probe(listener, exchanges, || {})
}

fn script_with_probe(
    listener: TcpListener,
    exchanges: Vec<Exchange>,
    before_response: impl Fn() + Send + 'static,
) -> JoinHandle<TcpListener> {
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
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            exchange.assert_request(&read_request(&mut stream));
            before_response();
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
            let write = stream.write_all(body);
            if !exchange.allow_body_disconnect {
                write.unwrap();
            }
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
        Self::with_origins(consent, None)
    }
    fn with_origins(consent: bool, origins: Option<[Url; 3]>) -> Self {
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
        let endpoints = if let Some([hoppii, idp, webclass]) = origins {
            AuthEndpoints {
                hoppii,
                idp,
                webclass,
                timeout: Duration::from_secs(3),
            }
        } else {
            endpoints
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

