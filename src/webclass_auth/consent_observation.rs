//! Read-only observation of a consent redirect in the current SAML trial.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use reqwest::{
    StatusCode, Url,
    header::{CONTENT_TYPE, LOCATION},
};
use scraper::{Html, Selector};
use serde::Serialize;

use crate::university_network::{BodyKind, Response, Transport, read_text, strict_url};

use super::{
    CONSENT, CONSENT_API,
    authn_request::CourseSamlSession,
    course_check::{saml_probe_path, saml_probe_query, valid_percent_escapes},
};

const MAX_QUERY_BYTES: usize = 4096;
const MAX_QUERY_FIELDS: usize = 16;
const MAX_NAME_BYTES: usize = 48;
const MAX_VALUE_BYTES: usize = 2048;

pub(crate) struct ConsentPageTarget(Url);

impl ConsentPageTarget {
    pub(crate) fn into_url(self) -> Url {
        self.0
    }
}

#[derive(Debug, Serialize)]
pub struct ConsentProbeReport {
    pub redirect_status: u16,
    pub query: Option<ConsentQuerySummary>,
    pub page_status: Option<u16>,
    pub page: Option<ConsentPageSummary>,
    pub stop: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ConsentQuerySummary {
    bytes: usize,
    field_count: usize,
    names: Vec<String>,
    duplicate_names: bool,
    encoded_names: bool,
    encoded_values: bool,
    malformed: bool,
    trial: Option<ConsentTrialChecks>,
}

#[derive(Debug, Serialize)]
struct ConsentTrialChecks {
    issuer_matches: bool,
    realm_matches: bool,
    idp_entity_at_idp_origin: bool,
    return_matches: bool,
}

#[derive(Debug, Serialize)]
pub struct ConsentPageSummary {
    service_markers: Vec<&'static str>,
    notice_markers: Vec<&'static str>,
    attribute_names: Vec<&'static str>,
    forms: Vec<ConsentFormSummary>,
    form_override_controls: usize,
    approve_control: bool,
    reject_control: bool,
    remember_choice: bool,
    each_time_choice: bool,
    external_scripts: usize,
    inline_markers: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ConsentFormSummary {
    method: &'static str,
    action: &'static str,
}

pub(super) fn observe(
    http: &Transport,
    response: Response,
    idp: &Url,
    session: &CourseSamlSession,
) -> ConsentProbeReport {
    let mut report = ConsentProbeReport {
        redirect_status: response.status().as_u16(),
        query: None,
        page_status: None,
        page: None,
        stop: "consent_page_observed",
    };
    if let Err(reason) = inspect(http, response, idp, session, &mut report) {
        report.stop = reason;
    }
    report
}

fn inspect(
    http: &Transport,
    response: Response,
    idp: &Url,
    session: &CourseSamlSession,
    report: &mut ConsentProbeReport,
) -> Result<(), &'static str> {
    if response.status() != StatusCode::FOUND {
        return Err("consent_requires_302");
    }
    let mut locations = response.headers().get_all(LOCATION).iter();
    let raw = locations
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or("consent_location_missing")?;
    if locations.next().is_some() {
        return Err("consent_location_ambiguous");
    }
    let (query, destination) =
        validate_location(raw, response.url(), idp, session.consent_entities())?;
    report.query = Some(query);
    let target = destination?;
    if !http.observes_course_consent() {
        return Err("stopped_before_consent_get");
    }
    drop(response);
    let page = http
        .send_course_consent_page(target)
        .map_err(|_| "consent_get_failed")?;
    report.page_status = Some(page.status().as_u16());
    if page.status() != StatusCode::OK {
        return Err("consent_page_status");
    }
    if !page
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|kind| {
            matches!(
                kind.trim().to_ascii_lowercase().as_str(),
                "text/html" | "application/xhtml+xml"
            )
        })
    {
        return Err("consent_page_not_html");
    }
    let page_url = page.url().clone();
    let html = read_text(page, BodyKind::AuthHtml).map_err(|_| "consent_page_body")?;
    report.page = Some(summarize_html(&html, &page_url, idp));
    Ok(())
}

fn same_idp_url(raw: &str, idp: &Url) -> Option<Url> {
    let absolute;
    let absolute = if raw.starts_with('/') && !raw.starts_with("//") {
        absolute = format!("{}{raw}", idp.origin().ascii_serialization());
        &absolute
    } else {
        raw
    };
    let url = strict_url(absolute).ok()?;
    let raw_query = raw.split_once('?').map(|(_, query)| query);
    (url.origin() == idp.origin() && url.query() == raw_query).then_some(url)
}

fn validate_location(
    raw: &str,
    source: &Url,
    idp: &Url,
    (issuer, realm): (&str, &str),
) -> Result<(ConsentQuerySummary, Result<ConsentPageTarget, &'static str>), &'static str> {
    let location = same_idp_url(raw, idp).ok_or("consent_target_not_canonical_idp_https")?;
    if location.path() != CONSENT {
        return Err("unexpected_consent_path");
    }
    let (mut summary, query) = inspect_query(&location);
    let authorized = authorize_query(&mut summary, &query, source, idp, issuer, realm);
    drop(query);
    Ok((summary, authorized.map(|()| ConsentPageTarget(location))))
}

fn inspect_query(location: &Url) -> (ConsentQuerySummary, BTreeMap<Cow<'_, str>, Cow<'_, str>>) {
    let raw = location.query().unwrap_or_default();
    let field_count = if raw.is_empty() {
        0
    } else {
        raw.split('&').count()
    };
    let mut summary = ConsentQuerySummary {
        bytes: raw.len(),
        field_count,
        names: Vec::new(),
        duplicate_names: false,
        encoded_names: false,
        encoded_values: false,
        malformed: field_count == 0,
        trial: None,
    };
    if raw.len() > MAX_QUERY_BYTES || field_count > MAX_QUERY_FIELDS {
        summary.malformed = true;
        return (summary, BTreeMap::new());
    }
    let decoded: Vec<_> = location.query_pairs().collect();
    let values: BTreeSet<_> = decoded.iter().map(|(_, value)| value.as_ref()).collect();
    let mut names = BTreeSet::new();
    for field in raw.split('&').filter(|field| !field.is_empty()) {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        summary.encoded_names |= key.contains(['%', '+']);
        summary.encoded_values |= value.contains(['%', '+']);
        summary.malformed |=
            !field.contains('=') || key.len() > MAX_NAME_BYTES || !identifier_name(key);
        summary.names.push(
            if field_label(key) && !values.contains(key) {
                key
            } else {
                "[redacted]"
            }
            .into(),
        );
    }
    summary.malformed |= summary.names.len() != field_count || !valid_percent_escapes(raw);
    drop(values);
    let query = decoded
        .into_iter()
        .map(|(key, value)| {
            summary.duplicate_names |= !names.insert(key.to_ascii_lowercase());
            summary.malformed |= value.len() > MAX_VALUE_BYTES
                || value.is_empty()
                || value.chars().any(|ch| ch.is_control() || ch == '\u{fffd}');
            (key, value)
        })
        .collect();
    (summary, query)
}

fn identifier_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
}

/// Only field-like words may be displayed. A key can itself carry an opaque value.
fn field_label(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "action",
        "acs",
        "adapter",
        "alias",
        "app",
        "assertion",
        "attribute",
        "auth",
        "binding",
        "client",
        "consent",
        "continue",
        "data",
        "decision",
        "destination",
        "display",
        "encoding",
        "entity",
        "error",
        "flow",
        "force",
        "format",
        "goto",
        "id",
        "idp",
        "index",
        "lang",
        "locale",
        "login",
        "meta",
        "method",
        "mode",
        "name",
        "next",
        "nonce",
        "operation",
        "original",
        "page",
        "path",
        "provider",
        "realm",
        "redirect",
        "relay",
        "req",
        "request",
        "response",
        "return",
        "saml",
        "service",
        "session",
        "sp",
        "sso",
        "state",
        "status",
        "target",
        "time",
        "token",
        "type",
        "uri",
        "url",
        "user",
        "version",
    ];
    if name.len() > MAX_NAME_BYTES || !identifier_name(name) {
        return false;
    }
    let normalized = name.to_ascii_lowercase();
    normalized
        .split(['_', '-', '.'])
        .filter(|part| !part.is_empty())
        .all(|mut part| {
            while !part.is_empty() {
                let Some(word) = WORDS
                    .iter()
                    .filter(|word| part.starts_with(**word))
                    .max_by_key(|word| word.len())
                else {
                    return false;
                };
                part = &part[word.len()..];
            }
            true
        })
        && name.bytes().any(|ch| ch.is_ascii_alphabetic())
}

fn authorize_query(
    summary: &mut ConsentQuerySummary,
    query: &BTreeMap<Cow<'_, str>, Cow<'_, str>>,
    source: &Url,
    idp: &Url,
    issuer: &str,
    realm: &str,
) -> Result<(), &'static str> {
    if summary.malformed || summary.duplicate_names || summary.encoded_names {
        return Err("invalid_consent_query");
    }
    const KEYS: [&str; 5] = [
        "realm",
        "idpEntityId",
        "spEntityId",
        "consentID",
        "originalRequestURI",
    ];
    if query.len() != KEYS.len() || !KEYS.iter().all(|key| query.contains_key(*key)) {
        return Err("unverified_consent_query");
    }
    let checks = ConsentTrialChecks {
        issuer_matches: query["spEntityId"] == issuer,
        realm_matches: query["realm"] == realm && valid_realm(realm),
        idp_entity_at_idp_origin: valid_idp_entity(&query["idpEntityId"], idp),
        return_matches: same_trial_return(&query["originalRequestURI"], source, idp),
    };
    let id = &query["consentID"];
    let result = match checks {
        _ if !identifier_name(id) || matches!(id.as_ref(), "." | "..") => {
            Err("invalid_consent_identifier")
        }
        ConsentTrialChecks {
            issuer_matches: false,
            ..
        } => Err("consent_issuer_mismatch"),
        ConsentTrialChecks {
            realm_matches: false,
            ..
        } => Err("consent_realm_mismatch"),
        ConsentTrialChecks {
            idp_entity_at_idp_origin: false,
            ..
        } => Err("unverified_consent_idp_entity"),
        ConsentTrialChecks {
            return_matches: false,
            ..
        } => Err("unverified_consent_return"),
        _ => Ok(()),
    };
    summary.trial = Some(checks);
    result
}

fn valid_realm(realm: &str) -> bool {
    realm == "/"
        || realm.strip_prefix('/').is_some_and(|path| {
            path.split('/')
                .all(|segment| identifier_name(segment) && !matches!(segment, "." | ".."))
        })
}

fn valid_idp_entity(entity: &str, idp: &Url) -> bool {
    // The identifier is asserted by this IdP's TLS response and is never fetched.
    // This checks its format and origin, independent of any metadata registration.
    let origin = idp.origin().ascii_serialization();
    entity == origin
        || entity == format!("{origin}:443")
        || strict_url(entity).is_ok_and(|url| {
            url.origin() == idp.origin()
                && url.query().is_none()
                && url.path().bytes().all(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, b'/' | b'_' | b'-' | b'.' | b'~')
                })
        })
}

fn same_trial_return(raw: &str, source: &Url, idp: &Url) -> bool {
    source.origin() == idp.origin()
        && saml_probe_path(source)
        && same_idp_url(raw, idp).is_some_and(|original| {
            original.path() == source.path()
                && saml_probe_query(source)
                    .is_some_and(|query| saml_probe_query(&original).as_ref() == Some(&query))
        })
}

fn summarize_html(html: &str, page: &Url, idp: &Url) -> ConsentPageSummary {
    let doc = Html::parse_document(html);
    let selector = |text| Selector::parse(text).expect("fixed selector");
    let text = |selector_text| {
        doc.select(&selector(selector_text))
            .flat_map(|node| node.text())
            .collect::<Vec<_>>()
    };
    let headings = text("title, h1, h2, h3, [role=heading]");
    let service_markers = ["HOPPII", "WebClass", "法政大学"]
        .into_iter()
        .filter(|name| {
            headings
                .iter()
                .any(|line| line.to_lowercase().contains(&name.to_lowercase()))
        })
        .collect();
    let notices = text("title, h1, h2, h3, p, label");
    let notice_markers = [
        "属性提供",
        "個人情報",
        "第三者提供",
        "同意",
        "attribute release",
        "share your",
    ]
    .into_iter()
    .filter(|name| {
        notices
            .iter()
            .any(|line| line.to_lowercase().contains(name))
    })
    .collect();
    let attribute_nodes = selector("[data-attribute-name], [data-saml-attribute], th, dt, label");
    let attribute_names = [
        "uid",
        "mail",
        "email",
        "displayName",
        "givenName",
        "sn",
        "cn",
        "eduPersonPrincipalName",
        "eduPersonAffiliation",
        "eduPersonScopedAffiliation",
        "eduPersonTargetedID",
        "employeeNumber",
    ]
    .into_iter()
    .filter(|name| {
        doc.select(&attribute_nodes).any(|node| {
            ["data-attribute-name", "data-saml-attribute"]
                .iter()
                .any(|key| {
                    node.value()
                        .attr(key)
                        .is_some_and(|value| value.eq_ignore_ascii_case(name))
                })
                || node
                    .text()
                    .collect::<String>()
                    .trim()
                    .eq_ignore_ascii_case(name)
        })
    })
    .collect();
    let forms = doc
        .select(&selector("form"))
        .map(|form| ConsentFormSummary {
            method: match form
                .value()
                .attr("method")
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                None => "GET_default",
                Some("get") => "GET",
                Some("post") => "POST",
                _ => "other",
            },
            action: action_kind(form.value().attr("action"), page, idp),
        })
        .collect();
    let controls: Vec<_> = doc.select(&selector("button, input[type=submit], input[type=button], input[type=radio], input[type=checkbox], [role=button], option, label"))
        .flat_map(|element| {
            let mut labels = vec![element.text().collect::<String>()];
            labels.extend(["value", "aria-label", "data-action", "name"].into_iter()
                .filter_map(|key| element.value().attr(key).map(str::to_string)));
            labels
        }).map(|label| label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
        .collect();
    let labeled = |names: &[&str]| controls.iter().any(|label| names.contains(&label.as_str()));
    let script = text("script:not([src])").join("");
    ConsentPageSummary {
        service_markers,
        notice_markers,
        attribute_names,
        forms,
        form_override_controls: doc
            .select(&selector(
                "button[formaction], input[formaction], button[formmethod], input[formmethod]",
            ))
            .count(),
        approve_control: labeled(&[
            "agree",
            "approve",
            "accept",
            "allow",
            "同意する",
            "同意",
            "許可する",
            "許可",
        ]),
        reject_control: labeled(&[
            "reject",
            "decline",
            "deny",
            "disagree",
            "同意しない",
            "拒否",
            "拒否する",
        ]),
        remember_choice: labeled(&[
            "ask_if_change",
            "never_ask",
            "remember",
            "remember my choice",
            "変更時のみ確認",
            "同意を保存する",
            "次回から表示しない",
        ]),
        each_time_choice: labeled(&[
            "ask_always",
            "ask always",
            "always ask",
            "毎回確認",
            "毎回確認する",
        ]),
        external_scripts: doc.select(&selector("script[src]")).count(),
        inline_markers: [
            "getAttributes",
            "ASK_IF_CHANGE",
            "ASK_ALWAYS",
            "/json/saml2/consent",
        ]
        .into_iter()
        .filter(|marker| script.contains(marker))
        .collect(),
    }
}

fn action_kind(raw: Option<&str>, page: &Url, idp: &Url) -> &'static str {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return "current_page_default";
    };
    let Ok(target) = page.join(raw) else {
        return "unparseable";
    };
    if target.origin() != idp.origin() {
        return "other_origin";
    }
    match target.path() {
        CONSENT => CONSENT,
        CONSENT_API => CONSENT_API,
        path if path.starts_with(CONSENT_API) => "/openam/json/saml2/consent/{redacted}",
        _ => "other_idp_path",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "urn:fixture:hoppii:service-provider";
    const REALM: &str = "/fixture";

    fn urls() -> (Url, Url, Url) {
        let idp = Url::parse("https://idp.synthetic.invalid").unwrap();
        let source = idp.join(
            "/openam/SSORedirect/metaAlias/fixture/provider?ReqID=synthetic&index=null&acsURL=fixture&spEntityID=fixture&binding=fixture",
        ).unwrap();
        let mut location = idp.join(CONSENT).unwrap();
        location.query_pairs_mut().extend_pairs([
            ("realm", REALM),
            ("idpEntityId", "https://idp.synthetic.invalid/idp/metadata"),
            ("spEntityId", ISSUER),
            ("consentID", "synthetic-private-id"),
            ("originalRequestURI", source.as_str()),
        ]);
        (idp, source, location)
    }

    #[test]
    fn requires_the_canonical_consent_path_and_verified_trial_return() {
        let (idp, source, location) = urls();
        let validate = |raw: &str| validate_location(raw, &source, &idp, (ISSUER, REALM));
        assert!(validate(location.as_str()).unwrap().1.is_ok());
        for raw in [
            location.as_str().replace("https://", "http://"),
            location
                .as_str()
                .replace("idp.synthetic.invalid", "other.synthetic.invalid"),
            location
                .as_str()
                .replace("idp.synthetic.invalid/", "idp.synthetic.invalid:444/"),
            location.as_str().replace("https://", "https://dummy@"),
            format!("{location}#"),
            location
                .as_str()
                .replace(CONSENT, "/openam/saml2/redirect/"),
            location
                .as_str()
                .replace(CONSENT, "/openam/saml2/consent/%2F"),
            location
                .as_str()
                .replace(CONSENT, "/openam/temp/%2e%2e/saml2/consent/"),
            location
                .as_str()
                .replace(CONSENT, "/openam/temp/../saml2/consent/"),
        ] {
            assert!(validate(&raw).is_err());
        }
        for raw in [
            location
                .as_str()
                .replace("ReqID%3Dsynthetic", "ReqID%3Ddifferent"),
            location.as_str().replace(
                "originalRequestURI=https%3A%2F%2Fidp.synthetic.invalid",
                "originalRequestURI=https%3A%2F%2Fothers.synthetic.invalid",
            ),
        ] {
            let (report, target) = validate(&raw).unwrap();
            assert!(!report.malformed);
            assert!(matches!(target, Err("unverified_consent_return")));
        }
    }

    #[test]
    fn requires_same_trial_entities_and_canonical_idp_entity_format() {
        let (idp, source, location) = urls();
        for (name, value, reason) in [
            (
                "spEntityId",
                "urn:fixture:unrelated",
                "consent_issuer_mismatch",
            ),
            ("realm", "/other", "consent_realm_mismatch"),
            ("realm", "/fixture/../fixture", "consent_realm_mismatch"),
            (
                "idpEntityId",
                "http://idp.synthetic.invalid/idp",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://other.synthetic.invalid/idp",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://idp.synthetic.invalid:444/idp",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://idp.synthetic.invalid/%69dp",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://idp.synthetic.invalid/idp?switch=1",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://idp.synthetic.invalid/idp#fragment",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "https://test@idp.synthetic.invalid/idp",
                "unverified_consent_idp_entity",
            ),
            (
                "idpEntityId",
                "urn:fixture:idp",
                "unverified_consent_idp_entity",
            ),
        ] {
            let mut altered = location.clone();
            altered
                .query_pairs_mut()
                .clear()
                .extend_pairs(location.query_pairs().map(|(key, original)| {
                    let value = if key == name {
                        Cow::Borrowed(value)
                    } else {
                        original
                    };
                    (key, value)
                }));
            let (summary, target) =
                validate_location(altered.as_str(), &source, &idp, (ISSUER, REALM)).unwrap();
            assert_eq!(target.err(), Some(reason));
            assert!(!serde_json::to_string(&summary).unwrap().contains(value));
        }
        for invalid in [
            "//fixture",
            "/fixture/.",
            "/fixture/..",
            "/%66ixture",
            "/fixture/",
        ] {
            assert!(!valid_realm(invalid));
        }
        assert!(valid_realm("/"));
        assert!(valid_realm("/fixture/subrealm"));
    }

    #[test]
    fn reports_safe_names_and_blocks_unknown_or_malformed_parameters() {
        let (idp, source, location) = urls();
        let validate = |raw: &str| validate_location(raw, &source, &idp, (ISSUER, REALM));
        let extra_key = format!("{location}&relayState=synthetic-private-state");
        let (summary, target) = validate(&extra_key).unwrap();
        assert_eq!(summary.field_count, 6);
        assert_eq!(
            summary.names,
            [
                "realm",
                "idpEntityId",
                "spEntityId",
                "consentID",
                "originalRequestURI",
                "relayState"
            ]
        );
        assert!(matches!(target, Err("unverified_consent_query")));
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("synthetic-private")
        );
        let opaque = format!("{location}&K4q1SessionW7b=private");
        let (summary, target) = validate(&opaque).unwrap();
        assert_eq!(summary.names.last().unwrap(), "[redacted]");
        assert!(target.is_err());
        for raw in [
            format!("{location}&consentID=duplicate"),
            format!("{location}&ConsentID=duplicate"),
            format!("{location}&%63onsentID=duplicate"),
            format!("{location}&malformed"),
            format!("{location}&extra="),
            location.as_str().replace("synthetic-private-id", "%0A"),
            location.as_str().replace("synthetic-private-id", "%ZZ"),
            location.as_str().replace("synthetic-private-id", "%FF"),
            location.as_str().replace("realm=%2Ffixture&", ""),
            location.as_str().replace("spEntityId=", "spEntityID="),
            format!("{location}&overflow={}", "a".repeat(MAX_QUERY_BYTES)),
            format!("{location}{}", "&extra=value".repeat(MAX_QUERY_FIELDS)),
        ] {
            assert!(validate(&raw).unwrap().1.is_err());
        }
    }

    #[test]
    fn reports_html_structure_without_retaining_values() {
        let (idp, _, location) = urls();
        let html = r#"<h1>WebClassの属性提供への同意</h1><form method="post" action="/openam/json/saml2/consent/private-reference"><label data-attribute-name="mail">synthetic@example.invalid</label><input type="hidden" name="RelayState" value="private-state"><button>同意する</button><button>拒否</button><label>変更時のみ確認</label></form><script src="/assets/page.js"></script>"#;
        let page = summarize_html(html, &location, &idp);
        assert_eq!(page.attribute_names, ["mail"]);
        assert_eq!(page.service_markers, ["WebClass"]);
        assert_eq!(page.forms[0].method, "POST");
        assert_eq!(
            page.forms[0].action,
            "/openam/json/saml2/consent/{redacted}"
        );
        assert!(page.approve_control && page.reject_control && page.remember_choice);
        let output = serde_json::to_string(&page).unwrap();
        assert!(!output.contains("private-"));
        assert!(!output.contains("example.invalid"));
    }
}
