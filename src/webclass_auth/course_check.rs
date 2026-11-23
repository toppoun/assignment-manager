//! Exact, pre-dispatch request restrictions for a single course-list check.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use reqwest::{Method, Url};

use crate::{university_network::NetworkError, webclass_http::COURSES_PATH};

use super::{
    AUTHENTICATE, AuthEndpoints, AuthStep, CONSENT, ConsentProbeReport, IDP_SSO, PARM_PATH,
    PORTAL_LOGIN_PATH, PORTAL_PUBLIC_PATH, USER_PAGE,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CourseCheckRequest {
    Other,
    Auth(AuthStep),
    SamlSuccess,
    SamlProbe,
    ConsentPage,
    Courses,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebClassMethodCounts {
    pub get: u8,
    pub post: u8,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebClassRequestCounts {
    pub hoppii: WebClassMethodCounts,
    pub idp: WebClassMethodCounts,
    pub webclass: WebClassMethodCounts,
    pub courses_get: u8,
}

#[derive(Default)]
pub(crate) struct CourseCheckMeasurements {
    pub(crate) requests: WebClassRequestCounts,
    pub(crate) consent: Option<ConsentProbeReport>,
}

pub(crate) type CourseRequestMeter = Arc<Mutex<CourseCheckMeasurements>>;

#[derive(Clone)]
pub(crate) struct CourseCheck {
    origins: [Url; 3],
    fixture_paths: bool,
    consent_page: bool,
    meter: CourseRequestMeter,
    saml_success_used: Arc<AtomicBool>,
}

impl CourseCheck {
    pub(super) fn new(
        endpoints: &AuthEndpoints,
        fixture_paths: bool,
        meter: CourseRequestMeter,
    ) -> Self {
        Self {
            origins: [
                endpoints.hoppii.clone(),
                endpoints.idp.clone(),
                endpoints.webclass.clone(),
            ],
            fixture_paths,
            consent_page: !fixture_paths,
            meter,
            saml_success_used: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn observes_saml(&self) -> bool {
        !self.fixture_paths
    }

    pub(super) fn stop_before_consent(mut self) -> Self {
        self.consent_page = false;
        self
    }

    pub(crate) fn observes_consent(&self) -> bool {
        self.consent_page
    }

    pub(crate) fn record_consent(&self, report: ConsentProbeReport) {
        self.meter
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .consent = Some(report);
    }

    /// Called immediately before execute, after destination and transport checks.
    /// Failed exchanges consume their reservation. A rejected request consumes none.
    pub(crate) fn reserve(
        &self,
        method: &Method,
        url: &Url,
        purpose: CourseCheckRequest,
    ) -> Result<(), NetworkError> {
        let origin = self
            .origins
            .iter()
            .position(|base| base.origin() == url.origin())
            .ok_or(NetworkError::Policy)?;
        use CourseCheckRequest::{Auth, ConsentPage, Courses, SamlProbe, SamlSuccess};

        let allowed = match (purpose, origin, method, url.path(), url.query()) {
            (Courses, 2, &Method::GET, COURSES_PATH, None) => self.fixture_paths,
            (Courses, ..) => false,
            (SamlProbe, 1, &Method::GET, _, _) => {
                self.observes_saml()
                    && saml_probe_path(url)
                    && saml_probe_query(url).is_some_and(|query| query["index"] == "null")
            }
            (SamlProbe, ..) => false,
            (ConsentPage, 1, &Method::GET, CONSENT, Some(_)) => self.observes_consent(),
            (ConsentPage, ..) => false,
            (
                SamlSuccess,
                1,
                &Method::GET,
                "/openam/SSORedirect/metaAlias/synthetic-idp",
                Some("ReqID=synthetic-request-id"),
            ) => self.fixture_paths,
            (SamlSuccess, ..) => false,
            (_, 0, &Method::GET, USER_PAGE, None) => {
                self.fixture_paths || purpose == Auth(AuthStep::HoppiiToIdp)
            }
            (_, 0, &Method::GET, PORTAL_PUBLIC_PATH | PORTAL_LOGIN_PATH, None | Some("next=")) => {
                self.fixture_paths
            }
            (_, 0, &Method::GET, PARM_PATH, Some("auth_mode=1&group_id=")) => self.fixture_paths,
            (Auth(AuthStep::IdpToOpenAm), 1, &Method::GET, IDP_SSO, _) => {
                valid_saml_redirect_query(url)
            }
            (
                Auth(
                    AuthStep::OpenAmInitial
                    | AuthStep::OpenAmCredentials
                    | AuthStep::OpenAmScripted2,
                ),
                1,
                &Method::POST,
                AUTHENTICATE,
                _,
            ) => true,
            (_, 0, &Method::POST, "/saml/acs", _)
            | (_, 2, &Method::POST, super::WEBCLASS_SSO_POST_PATH, None)
            | (
                _,
                2,
                &Method::GET,
                "/webclass/" | "/webclass/login.php" | "/webclass/after-sso",
                None,
            ) => self.fixture_paths,
            _ => false,
        };
        if !allowed || url.fragment().is_some() {
            return Err(NetworkError::Policy);
        }

        let course_list = purpose == Courses;
        let mut meter = self.meter.lock().map_err(|_| NetworkError::ResourceLimit)?;
        let counts = &mut meter.requests;
        if self.observes_saml() && !next_observation_request(purpose, counts) {
            return Err(NetworkError::ResourceLimit);
        }
        if course_list && counts.courses_get != 0 {
            return Err(NetworkError::ResourceLimit);
        }
        let (counts_for_host, get_limit, post_limit) = match origin {
            0 => (&mut counts.hoppii, 34, 1),
            1 => (&mut counts.idp, 3, 5),
            _ => (&mut counts.webclass, 11, 1),
        };
        let (counter, limit) = if *method == Method::GET {
            (&mut counts_for_host.get, get_limit)
        } else {
            (&mut counts_for_host.post, post_limit)
        };
        if *counter >= limit {
            return Err(NetworkError::ResourceLimit);
        }
        if matches!(purpose, SamlSuccess | SamlProbe)
            && self.saml_success_used.swap(true, Ordering::Relaxed)
        {
            return Err(NetworkError::ResourceLimit);
        }
        *counter += 1;
        counts.courses_get += u8::from(course_list);
        Ok(())
    }
}

fn next_observation_request(purpose: CourseCheckRequest, counts: &WebClassRequestCounts) -> bool {
    use CourseCheckRequest::{Auth, ConsentPage, SamlProbe};

    let stage = (counts.hoppii.get, counts.idp.get, counts.idp.post);
    matches!(
        (purpose, stage),
        (Auth(AuthStep::HoppiiToIdp), (0, 0, 0))
            | (Auth(AuthStep::IdpToOpenAm), (1, 0, 0))
            | (Auth(AuthStep::OpenAmInitial), (1, 1, 0))
            | (Auth(AuthStep::OpenAmCredentials), (1, 1, 1))
            | (Auth(AuthStep::OpenAmScripted2), (1, 1, 2))
            | (SamlProbe, (1, 1, 3))
            | (ConsentPage, (1, 2, 3))
    )
}

pub(super) fn saml_probe_path(url: &Url) -> bool {
    let Some((realm, provider)) = url
        .path()
        .strip_prefix("/openam/SSORedirect/metaAlias/")
        .and_then(|suffix| suffix.split_once('/'))
    else {
        return false;
    };
    [realm, provider]
        .iter()
        .all(|segment| !segment.is_empty() && segment.bytes().all(|ch| ch.is_ascii_alphanumeric()))
}

pub(super) fn saml_probe_query(url: &Url) -> Option<BTreeMap<Cow<'_, str>, Cow<'_, str>>> {
    let query = url.query()?;
    const KEYS: [&str; 5] = ["ReqID", "index", "acsURL", "spEntityID", "binding"];
    if !valid_percent_escapes(query)
        || query.split('&').count() != KEYS.len()
        || !query.split('&').all(|field| {
            field
                .split_once('=')
                .is_some_and(|(key, _)| KEYS.contains(&key))
        })
    {
        return None;
    }
    let decoded: BTreeMap<_, _> = url.query_pairs().collect();
    if decoded.len() != KEYS.len()
        || decoded.values().any(|value| {
            value.trim().is_empty() || value.chars().any(|ch| ch.is_control() || ch == '\u{fffd}')
        })
    {
        return None;
    }
    Some(decoded)
}

pub(super) fn valid_saml_redirect_query(url: &Url) -> bool {
    let Some(query) = url.query() else {
        return false;
    };
    let mut pairs = query.split('&');
    let first = pairs.next().and_then(|pair| pair.split_once('='));
    let second = pairs.next().and_then(|pair| pair.split_once('='));
    match (first, second, pairs.next()) {
        (Some(("SAMLRequest", _)), Some(("RelayState", _)), None)
        | (Some(("RelayState", _)), Some(("SAMLRequest", _)), None) => {}
        _ => return false,
    }
    valid_percent_escapes(query)
        && url
            .query_pairs()
            .all(|(_, value)| !value.trim().is_empty() && !value.chars().any(char::is_control))
}

pub(super) fn valid_percent_escapes(value: &str) -> bool {
    value.split('%').skip(1).all(|suffix| {
        suffix
            .as_bytes()
            .get(..2)
            .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
    })
}

#[cfg(test)]
mod tests {
    use super::super::endpoint;
    use super::*;
    use CourseCheckRequest::{Auth, Courses, Other, SamlSuccess};

    #[test]
    fn all_remaining_buckets_and_the_course_request_are_independently_bounded() {
        let endpoints = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&endpoints, true, meter.clone());
        for (method, url, purpose, limit) in [
            (
                Method::GET,
                endpoint(
                    &endpoints.idp,
                    &format!(
                        "{IDP_SSO}?RelayState=fixture%26relay&SAMLRequest=fixture%2Brequest%3D"
                    ),
                ),
                Auth(AuthStep::IdpToOpenAm),
                3,
            ),
            (
                Method::POST,
                endpoint(&endpoints.idp, AUTHENTICATE),
                Auth(AuthStep::OpenAmInitial),
                5,
            ),
            (
                Method::POST,
                endpoint(&endpoints.webclass, "/webclass/singlesignon.php"),
                Other,
                1,
            ),
        ] {
            for _ in 0..limit {
                guard.reserve(&method, &url, purpose).unwrap();
            }
            assert_eq!(
                guard.reserve(&method, &url, purpose),
                Err(NetworkError::ResourceLimit)
            );
        }
        let dashboard = endpoint(&endpoints.webclass, "/webclass/");
        let courses = endpoint(&endpoints.webclass, COURSES_PATH);
        assert_eq!(
            guard.reserve(&Method::GET, &courses, Other),
            Err(NetworkError::Policy)
        );
        guard.reserve(&Method::GET, &courses, Courses).unwrap();
        assert_eq!(
            guard.reserve(&Method::GET, &courses, Courses),
            Err(NetworkError::ResourceLimit)
        );
        for _ in 0..10 {
            guard.reserve(&Method::GET, &dashboard, Other).unwrap();
        }
        assert_eq!(
            guard.reserve(&Method::GET, &dashboard, Other),
            Err(NetworkError::ResourceLimit)
        );
        assert_eq!(
            meter.lock().unwrap().requests.webclass,
            WebClassMethodCounts { get: 11, post: 1 }
        );
    }

    #[test]
    fn production_origins_do_not_make_synthetic_paths_valid() {
        let prod = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&prod, false, meter.clone());
        for (method, url, purpose) in [
            (
                Method::GET,
                endpoint(&prod.idp, "/idp/sso"),
                Auth(AuthStep::IdpToOpenAm),
            ),
            (
                Method::GET,
                endpoint(
                    &prod.idp,
                    "/openam/SSORedirect/metaAlias/synthetic-idp?ReqID=synthetic-request-id",
                ),
                SamlSuccess,
            ),
            (
                Method::GET,
                endpoint(
                    &prod.idp,
                    "/openam/SSORedirect/metaAlias/abc/abcd?ReqID=mock&index=mock&fixture=mock&spEntityID=mock&binding=mock",
                ),
                SamlSuccess,
            ),
            (
                Method::POST,
                endpoint(&prod.hoppii, "/saml/acs"),
                Auth(AuthStep::HoppiiAcs),
            ),
            (
                Method::GET,
                endpoint(&prod.webclass, "/webclass/after-sso"),
                Auth(AuthStep::WebClassRedirect),
            ),
        ] {
            assert_eq!(
                guard.reserve(&method, &url, purpose),
                Err(NetworkError::Policy)
            );
        }
        assert_eq!(
            meter.lock().unwrap().requests,
            WebClassRequestCounts::default()
        );
    }

    #[test]
    fn only_the_exact_fixture_success_target_has_a_single_shared_reservation() {
        let endpoints = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&endpoints, true, meter.clone());
        let path = "/openam/SSORedirect/metaAlias/synthetic-idp";
        let target = endpoint(
            &endpoints.idp,
            &format!("{path}?ReqID=synthetic-request-id"),
        );
        assert_eq!(
            guard.reserve(&Method::POST, &target, SamlSuccess),
            Err(NetworkError::Policy)
        );
        assert_eq!(
            guard.reserve(&Method::GET, &target, Auth(AuthStep::Saml)),
            Err(NetworkError::Policy)
        );
        for wrong in [
            format!("{path}?ReqID=synthetic-request-id&ReqID=other"),
            format!("{path}?Req%49D=synthetic-request-id"),
            format!("{path}/other?ReqID=synthetic-request-id"),
        ] {
            assert_eq!(
                guard.reserve(&Method::GET, &endpoint(&endpoints.idp, &wrong), SamlSuccess),
                Err(NetworkError::Policy)
            );
        }
        guard.reserve(&Method::GET, &target, SamlSuccess).unwrap();
        assert_eq!(
            guard.clone().reserve(&Method::GET, &target, SamlSuccess),
            Err(NetworkError::ResourceLimit)
        );
        assert_eq!(meter.lock().unwrap().requests.idp.get, 1);
    }

    #[test]
    fn production_probe_allows_one_consent_get_after_the_six_sso_requests() {
        use CourseCheckRequest::{ConsentPage, SamlProbe};

        let endpoints = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&endpoints, false, meter.clone());
        let portal = endpoint(&endpoints.hoppii, USER_PAGE);
        let entry = endpoint(
            &endpoints.idp,
            &format!("{IDP_SSO}?SAMLRequest=fixture&RelayState=fixture"),
        );
        let authenticate = endpoint(&endpoints.idp, AUTHENTICATE);
        let target = endpoint(
            &endpoints.idp,
            "/openam/SSORedirect/metaAlias/fixture/provider?ReqID=fixture&index=null&acsURL=fixture&spEntityID=fixture&binding=fixture",
        );
        let consent = endpoint(&endpoints.idp, "/openam/saml2/consent/?consentID=fixture");
        assert_eq!(
            guard.reserve(&Method::GET, &consent, ConsentPage),
            Err(NetworkError::ResourceLimit)
        );
        assert_eq!(
            guard.reserve(&Method::GET, &target, SamlProbe),
            Err(NetworkError::ResourceLimit)
        );
        for (method, url, purpose) in [
            (Method::GET, &portal, Auth(AuthStep::HoppiiToIdp)),
            (Method::GET, &entry, Auth(AuthStep::IdpToOpenAm)),
            (Method::POST, &authenticate, Auth(AuthStep::OpenAmInitial)),
            (
                Method::POST,
                &authenticate,
                Auth(AuthStep::OpenAmCredentials),
            ),
            (Method::POST, &authenticate, Auth(AuthStep::OpenAmScripted2)),
            (Method::GET, &target, SamlProbe),
            (Method::GET, &consent, ConsentPage),
        ] {
            guard.reserve(&method, url, purpose).unwrap();
            assert_eq!(
                guard.reserve(&method, url, purpose),
                Err(NetworkError::ResourceLimit)
            );
        }
        for (method, url, purpose) in [
            (Method::POST, consent, ConsentPage),
            (Method::POST, target, SamlProbe),
            (
                Method::POST,
                endpoint(&endpoints.hoppii, "/saml/acs"),
                Auth(AuthStep::HoppiiAcs),
            ),
            (Method::GET, endpoint(&endpoints.hoppii, PARM_PATH), Other),
            (
                Method::POST,
                endpoint(&endpoints.webclass, "/webclass/singlesignon.php"),
                Auth(AuthStep::WebClassSso),
            ),
            (
                Method::GET,
                endpoint(&endpoints.webclass, COURSES_PATH),
                Courses,
            ),
        ] {
            assert_eq!(
                guard.reserve(&method, &url, purpose),
                Err(NetworkError::Policy)
            );
        }
        assert_eq!(
            meter.lock().unwrap().requests,
            WebClassRequestCounts {
                hoppii: WebClassMethodCounts { get: 1, post: 0 },
                idp: WebClassMethodCounts { get: 3, post: 3 },
                ..WebClassRequestCounts::default()
            }
        );
    }
}
