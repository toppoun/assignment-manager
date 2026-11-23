//! SAML request validation for the secured course-list login.

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status};
use quick_xml::{NsReader, XmlVersion, events::Event, name::ResolveResult};
use reqwest::Url;
use zeroize::Zeroizing;

use crate::university_network::strict_url;

use super::{
    AuthEndpoints, AuthStep, CONSENT, HoseiAuthError, IDP_SSO, OpenAmSuccess, ProtocolReason,
    SamlProbeReport, SamlProbeStop,
    course_check::{
        saml_probe_path, saml_probe_query, valid_percent_escapes, valid_saml_redirect_query,
    },
    endpoint, required_query_parameter,
};

const MAX_XML_BYTES: usize = 64 * 1024;
const MAX_XML_DEPTH: usize = 32;
const MAX_ENCODED_BYTES: usize = (MAX_XML_BYTES + 1024).div_ceil(3) * 4;
const SAML_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const SAML_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const ENTITY_FORMAT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:entity";
pub(super) const HTTP_POST: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";

pub(super) struct AuthnRequest {
    issuer: String,
    // Retained for this trial only. A SAML Response can refer to it via
    // InResponseTo. OpenAM's ReqID has no established relationship to it.
    id: String,
    acs: Url,
    binding: String,
}

pub(super) struct CourseSamlSession {
    request: AuthnRequest,
    goto: Url,
    realm: String,
}

/// A one-use GET target constructed from the current OpenAM success response.
pub(crate) struct SamlProbeTarget(Url);

impl SamlProbeTarget {
    pub(crate) fn into_url(self) -> Url {
        self.0
    }
}

impl AuthnRequest {
    pub(super) fn from_redirect(
        redirect: &Url,
        endpoints: &AuthEndpoints,
    ) -> Result<Self, HoseiAuthError> {
        if redirect.origin() != endpoints.idp.origin()
            || redirect.path() != IDP_SSO
            || redirect.fragment().is_some()
            || !valid_saml_redirect_query(redirect)
        {
            return Err(HoseiAuthError::ProtocolChangedWithReason(
                AuthStep::IdpToOpenAm,
                ProtocolReason::RequestNotAllowed,
            ));
        }

        let step = AuthStep::HoppiiToIdp;
        let encoded = required_query_parameter(redirect, "SAMLRequest", step)?;
        Self::parse(&encoded, endpoints).ok_or(HoseiAuthError::ProtocolChanged(step))
    }

    fn parse(encoded: &str, endpoints: &AuthEndpoints) -> Option<Self> {
        let xml = inflate(encoded)?;
        let (mut fields, issuer) = parse_xml(std::str::from_utf8(&xml).ok()?, "AuthnRequest")?;
        let id = fields.remove("ID").filter(|id| valid_id(id))?;
        let issuer = issuer.filter(|issuer| {
            issuer.is_ascii()
                && valid_percent_escapes(issuer)
                && !issuer
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control())
                && Url::parse(issuer).is_ok()
        })?;
        let binding = fields.remove("ProtocolBinding")?;
        if fields.get("Version")?.as_str() != "2.0"
            || chrono::DateTime::parse_from_rfc3339(fields.get("IssueInstant")?).is_err()
            || binding != HTTP_POST
            || fields.contains_key("AssertionConsumerServiceIndex")
        {
            return None;
        }
        let destination = checked_https_url(fields.get("Destination")?, &endpoints.idp)?;
        if destination != endpoint(&endpoints.idp, IDP_SSO) {
            return None;
        }
        let acs = checked_https_url(
            fields.get("AssertionConsumerServiceURL")?,
            &endpoints.hoppii,
        )?;
        Some(Self {
            issuer,
            id,
            acs,
            binding,
        })
    }

    pub(super) fn bind_xui(
        self,
        auth_url: &Url,
        idp: &Url,
    ) -> Result<CourseSamlSession, HoseiAuthError> {
        let step = AuthStep::IdpToOpenAm;
        let issuer = required_query_parameter(auth_url, "spEntityID", step)?;
        if issuer != self.issuer {
            return Err(HoseiAuthError::ProtocolChanged(step));
        }
        let goto = required_query_parameter(auth_url, "goto", step)?;
        let goto = checked_course_url(&goto, idp).ok_or(HoseiAuthError::ProtocolChanged(step))?;
        Ok(CourseSamlSession {
            request: self,
            goto,
            realm: required_query_parameter(auth_url, "realm", step)?,
        })
    }
}

impl CourseSamlSession {
    pub(super) fn consent_entities(&self) -> (&str, &str) {
        (&self.request.issuer, &self.realm)
    }

    pub(super) fn response_matches_request(&self, encoded: &str) -> Option<bool> {
        Some(response_request_id(encoded)? == self.request.id)
    }

    pub(super) fn probe_target(
        &self,
        success: &OpenAmSuccess,
        idp: &Url,
        report: &mut SamlProbeReport,
    ) -> Result<SamlProbeTarget, SamlProbeStop> {
        let target = checked_idp_reference(success.success_url(), idp)
            .ok_or(SamlProbeStop::InvalidTarget)?;
        if target.path() == CONSENT {
            return Err(SamlProbeStop::ConsentRequired);
        }
        let query = saml_probe_query(&target);
        report.query_matches = Some(query.is_some());
        let query = query.ok_or(SamlProbeStop::QueryStructure)?;
        let acs_matches = self.verify_acs(&query["acsURL"], &self.request.acs).is_ok();
        report.acs_url_matches = Some(acs_matches);
        if !saml_probe_path(&target) {
            return Err(SamlProbeStop::ProviderPath);
        }
        if !acs_matches {
            return Err(SamlProbeStop::AcsUrlMismatch);
        }
        if query["spEntityID"] != self.request.issuer {
            return Err(SamlProbeStop::IssuerMismatch);
        }
        if query["binding"] != self.request.binding {
            return Err(SamlProbeStop::BindingMismatch);
        }
        // PingAM serializes an absent ACS index as "null". The validated
        // AuthnRequest uses a direct ACS URL and contains no index.
        if query["index"] != "null" {
            return Err(SamlProbeStop::UnexpectedIndex);
        }
        if target != self.goto {
            return Err(SamlProbeStop::GotoMismatch);
        }
        Ok(SamlProbeTarget(target))
    }

    pub(super) fn verify_success(&self, raw: &str, idp: &Url) -> Result<Url, HoseiAuthError> {
        let step = AuthStep::OpenAmSuccess;
        let url = checked_course_url(raw, idp).ok_or(HoseiAuthError::ProtocolChanged(step))?;
        if url.path() == CONSENT {
            return Err(HoseiAuthError::ConsentRequired);
        }
        if url != self.goto {
            return Err(HoseiAuthError::ProtocolChanged(step));
        }
        Ok(url)
    }

    pub(super) fn verify_acs(&self, raw: &str, hoppii: &Url) -> Result<Url, HoseiAuthError> {
        checked_https_url(raw, hoppii)
            .filter(|target| target == &self.request.acs)
            .ok_or(HoseiAuthError::ProtocolChangedWithReason(
                AuthStep::HoppiiAcs,
                ProtocolReason::RequestNotAllowed,
            ))
    }
}

fn inflate(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() > MAX_ENCODED_BYTES {
        return None;
    }
    let compressed = STANDARD.decode(encoded).ok()?;
    let mut xml = vec![0; MAX_XML_BYTES + 1];
    let mut decoder = Decompress::new(false);
    let status = decoder
        .decompress(&compressed, &mut xml, FlushDecompress::Finish)
        .ok()?;
    let size = decoder.total_out() as usize;
    if status != Status::StreamEnd
        || decoder.total_in() as usize != compressed.len()
        || size > MAX_XML_BYTES
    {
        return None;
    }
    xml.truncate(size);
    Some(xml)
}

type Attributes = BTreeMap<String, String>;

/// Observe the response envelope's correlation attribute. This does not verify signatures.
fn response_request_id(encoded: &str) -> Option<String> {
    if encoded.len() > MAX_XML_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let xml = Zeroizing::new(STANDARD.decode(encoded).ok()?);
    if xml.len() > MAX_XML_BYTES {
        return None;
    }
    let (mut fields, _) = parse_xml(std::str::from_utf8(&xml).ok()?, "Response")?;
    if fields.get("Version")?.as_str() != "2.0" {
        return None;
    }
    fields.remove("InResponseTo").filter(|id| valid_id(id))
}

fn parse_xml(xml: &str, root: &str) -> Option<(Attributes, Option<String>)> {
    if !xml.chars().all(xml_char) {
        return None;
    }
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut fields = None;
    let mut issuer = None;
    let mut depth = 0_usize;
    let mut reading_issuer = false;
    let mut seen_declaration = false;
    loop {
        let (namespace, event) = reader.read_resolved_event().ok()?;
        match event {
            Event::Start(tag) => {
                depth += 1;
                if depth > MAX_XML_DEPTH
                    || reading_issuer
                    || matches!(&namespace, ResolveResult::Unknown(_))
                {
                    return None;
                }
                // The reader never loads entities. Inspect attribute values even
                // on unused elements so an undefined entity is rejected.
                let attributes: Attributes = tag
                    .attributes()
                    .map(|attribute| {
                        let attribute = attribute.ok()?;
                        let value = attribute
                            .normalized_value(XmlVersion::Implicit1_0)
                            .ok()?
                            .into_owned();
                        value
                            .chars()
                            .all(xml_char)
                            .then(|| (attribute.key.as_ref().to_owned(), value))
                    })
                    .collect::<Option<_>>()?;
                let local_name = tag.local_name();
                if depth == 1 {
                    if fields.is_some()
                        || !matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == SAML_PROTOCOL)
                        || local_name.as_ref() != root
                    {
                        return None;
                    }
                    fields = Some(attributes);
                } else if root == "AuthnRequest"
                    && depth == 2
                    && matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == SAML_ASSERTION)
                    && local_name.as_ref() == "Issuer"
                {
                    if issuer.is_some()
                        || attributes
                            .get("Format")
                            .is_some_and(|format| format != ENTITY_FORMAT)
                    {
                        return None;
                    }
                    issuer = Some(String::new());
                    reading_issuer = true;
                }
            }
            Event::End(_) => {
                depth = depth.checked_sub(1)?;
                reading_issuer = false;
            }
            Event::Text(text) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if depth <= 1
                    && !text
                        .chars()
                        .all(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
                {
                    return None;
                }
                if reading_issuer {
                    issuer.as_mut()?.push_str(&text);
                }
            }
            Event::CData(text) if depth > 1 => {
                if reading_issuer {
                    issuer
                        .as_mut()?
                        .push_str(&text.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(reference) if depth > 1 => {
                let reference = format!("&{};", reference.xml_content(XmlVersion::Implicit1_0));
                let value = quick_xml::escape::unescape(&reference).ok()?;
                if !value.chars().all(xml_char) {
                    return None;
                }
                if reading_issuer {
                    issuer.as_mut()?.push_str(&value);
                }
            }
            Event::Decl(declaration) if fields.is_none() && depth == 0 && !seen_declaration => {
                seen_declaration = true;
                if declaration.version().ok()?.as_ref() != "1.0"
                    || declaration.encoding().is_some_and(|encoding| {
                        !encoding.is_ok_and(|value| value.eq_ignore_ascii_case("UTF-8"))
                    })
                {
                    return None;
                }
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof if depth == 0 => return Some((fields?, issuer)),
            // Includes every DTD, regardless of its contents.
            _ => return None,
        }
    }
}

fn xml_char(ch: char) -> bool {
    matches!(ch, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == b'_')
        && bytes.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
}

fn checked_https_url(raw: &str, expected: &Url) -> Option<Url> {
    let target = strict_url(raw).ok()?;
    let raw_query = raw.split_once('?').map(|(_, query)| query);
    if target.origin() != expected.origin()
        || target.path() == "/"
        || target.query() != raw_query
        || !target
            .path()
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'/' | b'_' | b'-' | b'.' | b'~'))
        || target
            .query()
            .is_some_and(|query| !valid_percent_escapes(query))
        || target.query_pairs().any(|(key, value)| {
            key.chars()
                .chain(value.chars())
                .any(|ch| ch.is_control() || ch == '\u{fffd}')
        })
    {
        return None;
    }
    Some(target)
}

fn checked_idp_reference(raw: &str, idp: &Url) -> Option<Url> {
    let absolute;
    let raw = if raw.starts_with('/') && !raw.starts_with("//") {
        absolute = format!("{}{raw}", idp.origin().ascii_serialization());
        &absolute
    } else {
        raw
    };
    checked_https_url(raw, idp)
}

fn checked_course_url(raw: &str, idp: &Url) -> Option<Url> {
    let target = checked_idp_reference(raw, idp)?;
    if let Some(query) = target.query() {
        let mut names = BTreeSet::new();
        for pair in query.split('&') {
            let (name, _) = pair.split_once('=')?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
                || !names.insert(name)
            {
                return None;
            }
        }
        if target
            .query_pairs()
            .any(|(_, value)| value.trim().is_empty())
        {
            return None;
        }
    }
    Some(target)
}

#[cfg(any(test, feature = "synthetic-https-fixture"))]
pub(super) fn encode_fixture(xml: &[u8]) -> String {
    use std::io::Write;

    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(xml).expect("write fixture into memory");
    STANDARD.encode(encoder.finish().expect("finish fixture in memory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "urn:fixture:hoppii:sp";
    const ACS: &str = "https://hoppii.synthetic.invalid/saml/acs";
    const DESTINATION: &str = "https://idp.synthetic.invalid/idp/profile/SAML2/Redirect/SSO";

    fn endpoints() -> AuthEndpoints {
        let mut endpoints = AuthEndpoints::production();
        endpoints.hoppii = Url::parse("https://hoppii.synthetic.invalid").unwrap();
        endpoints.idp = Url::parse("https://idp.synthetic.invalid").unwrap();
        endpoints
    }

    fn xml() -> String {
        format!(
            r#"<p:AuthnRequest xmlns:p="{SAML_PROTOCOL}" xmlns:a="{SAML_ASSERTION}" ID="_saml-id" Version="2.0" IssueInstant="2026-01-01T00:00:00Z" Destination="{DESTINATION}" AssertionConsumerServiceURL="{ACS}" ProtocolBinding="{HTTP_POST}"><a:Issuer>{ISSUER}</a:Issuer></p:AuthnRequest>"#
        )
    }

    fn parse(xml: &str) -> Option<AuthnRequest> {
        AuthnRequest::parse(&encode_fixture(xml.as_bytes()), &endpoints())
    }

    #[test]
    fn parses_a_distinct_sp_identifier_and_matches_the_exact_acs_origin() {
        let request = parse(&xml()).expect("valid AuthnRequest");
        assert_eq!(request.issuer, ISSUER);
        assert_eq!(request.id, "_saml-id");
        assert_eq!(request.acs.as_str(), ACS);
    }

    #[test]
    fn reads_only_a_bounded_saml_response_envelope() {
        let response = format!(
            r#"<p:Response xmlns:p="{SAML_PROTOCOL}" Version="2.0" InResponseTo="_saml-id"><p:Status/></p:Response>"#,
        );
        let parse = |xml: &str| response_request_id(&STANDARD.encode(xml));
        assert_eq!(parse(&response).as_deref(), Some("_saml-id"));
        assert!(response_request_id("invalid Base64").is_none());
        let nested = format!(
            "{}{}",
            "<a>".repeat(MAX_XML_DEPTH),
            "</a>".repeat(MAX_XML_DEPTH)
        );
        for xml in [
            format!(
                "<!DOCTYPE p:Response SYSTEM 'https://other.synthetic.invalid/entity'>{response}"
            ),
            response.replace(SAML_PROTOCOL, "urn:fixture:not-saml"),
            response.replace("Version=\"2.0\"", "Version=\"1.0\""),
            response.replace(" InResponseTo=\"_saml-id\"", ""),
            response.replace("_saml-id", "&undefined;"),
            response.replace("<p:Status/>", &nested),
            response.replace(
                "<p:Status/>",
                "<p:Other InResponseTo='_different'/><p:Status>",
            ),
            format!("{response}{response}"),
            format!("{response}{}", " ".repeat(MAX_XML_BYTES)),
        ] {
            assert!(parse(&xml).is_none());
        }
    }

    #[test]
    fn rejects_wrong_request_fields_and_noncanonical_acs_urls() {
        let base = xml();
        for (original, replacement) in [
            ("ID=\"_saml-id\"", "ID=\"\""),
            ("ID=\"_saml-id\"", "ID=\"bad:id\""),
            ("ID=\"_saml-id\"", ""),
            (DESTINATION, "https://idp.synthetic.invalid/openam/XUI/"),
            (
                DESTINATION,
                "https://other.synthetic.invalid/idp/profile/SAML2/Redirect/SSO",
            ),
            (
                DESTINATION,
                "https://idp.synthetic.invalid/idp/temp/../profile/SAML2/Redirect/SSO",
            ),
            (ACS, "http://hoppii.synthetic.invalid/saml/acs"),
            (ACS, "https://other.synthetic.invalid/saml/acs"),
            (ACS, "https://hoppii.synthetic.invalid:444/saml/acs"),
            (ACS, "https://hoppii.synthetic.invalid/saml/%61cs"),
            (ACS, "https://hoppii.synthetic.invalid/saml/a/../acs"),
            (ACS, "https://hoppii.synthetic.invalid/saml//acs"),
            (ACS, "https://hoppii.synthetic.invalid/saml/acs#not-allowed"),
            (ACS, "https://hoppii.synthetic.invalid/saml/acs?test=%GG"),
            (
                "ProtocolBinding=",
                "AssertionConsumerServiceIndex=\"1\" ProtocolBinding=",
            ),
            (
                HTTP_POST,
                "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Artifact",
            ),
            (ISSUER, "relative"),
            ("<a:Issuer>", "<a:Issuer Format=\"urn:other\">"),
        ] {
            assert!(
                parse(&base.replace(original, replacement)).is_none(),
                "{replacement}"
            );
        }
    }

    #[test]
    fn rejects_dtds_namespace_spoofs_and_unbounded_or_malformed_xml() {
        let base = xml();
        let nested = |levels: usize| {
            base.replace(
                "<a:Issuer>",
                &format!(
                    "{}{}<a:Issuer>",
                    "<p:Extensions>".repeat(levels),
                    "</p:Extensions>".repeat(levels)
                ),
            )
        };
        assert!(parse(&nested(MAX_XML_DEPTH - 1)).is_some());
        for bad in [
            format!("<!DOCTYPE test [<!ENTITY value 'substituted'>]>{base}"),
            format!(r#"<!DOCTYPE test SYSTEM "file:///synthetic-never-opened">{base}"#),
            base.replace(ISSUER, "&undefined;"),
            base.replace(SAML_PROTOCOL, "urn:fixture:not-saml"),
            base.replace(SAML_ASSERTION, "urn:fixture:not-saml"),
            base.replace(
                "</a:Issuer>",
                "</a:Issuer><a:Issuer>urn:fixture:other</a:Issuer>",
            ),
            base.replace("<a:Issuer>", "<a:Issuer><other/>"),
            base.replace("</p:AuthnRequest>", ""),
            format!("{base}{base}"),
            nested(MAX_XML_DEPTH),
        ] {
            assert!(parse(&bad).is_none());
        }
    }

    #[test]
    fn bounds_inflation_and_rejects_incomplete_or_concatenated_streams() {
        let mut at_limit = xml();
        at_limit.push_str(&" ".repeat(MAX_XML_BYTES - at_limit.len()));
        assert!(parse(&at_limit).is_some());
        at_limit.push(' ');
        assert!(parse(&at_limit).is_none());

        let encoded = encode_fixture(xml().as_bytes());
        let mut compressed = STANDARD.decode(encoded).unwrap();
        let complete = compressed.clone();
        compressed.pop();
        assert!(AuthnRequest::parse(&STANDARD.encode(compressed), &endpoints()).is_none());
        let concatenated = [&complete[..], &complete[..]].concat();
        assert!(AuthnRequest::parse(&STANDARD.encode(concatenated), &endpoints()).is_none());
        assert!(AuthnRequest::parse("not a Base64 request", &endpoints()).is_none());
    }
}
