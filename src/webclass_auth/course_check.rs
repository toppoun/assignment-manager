//! Exact, pre-dispatch request restrictions for a single course-list check.

use std::sync::{Arc, Mutex};

use reqwest::{Method, Url};

use crate::{university_network::NetworkError, webclass_http::COURSES_PATH};

use super::{AUTHENTICATE, AuthEndpoints, PORTAL_LOGIN_PATH, PORTAL_PUBLIC_PATH, USER_PAGE};

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

pub(crate) type CourseRequestMeter = Arc<Mutex<WebClassRequestCounts>>;

#[derive(Clone)]
pub(crate) struct CourseCheck {
    origins: [Url; 3],
    fixture_paths: bool,
    meter: CourseRequestMeter,
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
            meter,
        }
    }

    /// Called immediately before execute, after destination and transport checks.
    /// Failed exchanges consume their reservation. A rejected request consumes none.
    pub(crate) fn reserve(
        &self,
        method: &Method,
        url: &Url,
        course_list: bool,
    ) -> Result<(), NetworkError> {
        let origin = self
            .origins
            .iter()
            .position(|base| base.origin() == url.origin())
            .ok_or(NetworkError::Policy)?;
        let path = url.path();
        let query = url.query();
        let allowed = if course_list {
            origin == 2 && *method == Method::GET && path == COURSES_PATH && query.is_none()
        } else {
            match (origin, method, path, query) {
                (0, &Method::GET, USER_PAGE, None) => true,
                (0, &Method::GET, PORTAL_PUBLIC_PATH | PORTAL_LOGIN_PATH, None | Some("next=")) => {
                    true
                }
                (0, &Method::GET, super::PARM_PATH, Some("auth_mode=1&group_id=")) => true,
                (1, &Method::POST, AUTHENTICATE, _) => true,
                (2, &Method::POST, "/webclass/singlesignon.php", None) => true,
                (2, &Method::GET, "/webclass/" | "/webclass/login.php", None) => true,
                // The actual IdP navigation and HOPPII ACS paths are absent
                // from the recorded production contract. Do not guess them.
                (0, &Method::POST, "/saml/acs", _)
                | (
                    1,
                    &Method::GET,
                    "/idp/sso" | "/openam/SSORedirect/metaAlias/synthetic-idp",
                    _,
                )
                | (2, &Method::GET, "/webclass/after-sso", None) => self.fixture_paths,
                _ => false,
            }
        };
        if !allowed {
            return Err(NetworkError::Policy);
        }

        let mut counts = self.meter.lock().map_err(|_| NetworkError::ResourceLimit)?;
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
        *counter += 1;
        counts.courses_get += u8::from(course_list);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::endpoint;
    use super::*;

    #[test]
    fn all_remaining_buckets_and_the_course_request_are_independently_bounded() {
        let endpoints = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&endpoints, true, meter.clone());
        for (method, url, limit) in [
            (Method::GET, endpoint(&endpoints.idp, "/idp/sso"), 3),
            (Method::POST, endpoint(&endpoints.idp, AUTHENTICATE), 5),
            (
                Method::POST,
                endpoint(&endpoints.webclass, "/webclass/singlesignon.php"),
                1,
            ),
        ] {
            for _ in 0..limit {
                guard.reserve(&method, &url, false).unwrap();
            }
            assert_eq!(
                guard.reserve(&method, &url, false),
                Err(NetworkError::ResourceLimit)
            );
        }
        let dashboard = endpoint(&endpoints.webclass, "/webclass/");
        let courses = endpoint(&endpoints.webclass, COURSES_PATH);
        assert_eq!(
            guard.reserve(&Method::GET, &courses, false),
            Err(NetworkError::Policy)
        );
        guard.reserve(&Method::GET, &courses, true).unwrap();
        assert_eq!(
            guard.reserve(&Method::GET, &courses, true),
            Err(NetworkError::ResourceLimit)
        );
        for _ in 0..10 {
            guard.reserve(&Method::GET, &dashboard, false).unwrap();
        }
        assert_eq!(
            guard.reserve(&Method::GET, &dashboard, false),
            Err(NetworkError::ResourceLimit)
        );
        assert_eq!(
            meter.lock().unwrap().webclass,
            WebClassMethodCounts { get: 11, post: 1 }
        );
    }

    #[test]
    fn production_origins_do_not_make_synthetic_paths_valid() {
        let prod = AuthEndpoints::production();
        let meter = CourseRequestMeter::default();
        let guard = CourseCheck::new(&prod, false, meter.clone());
        for (method, url) in [
            (Method::GET, endpoint(&prod.idp, "/idp/sso")),
            (
                Method::GET,
                endpoint(&prod.idp, "/openam/SSORedirect/metaAlias/synthetic-idp"),
            ),
            (Method::POST, endpoint(&prod.hoppii, "/saml/acs")),
            (Method::GET, endpoint(&prod.webclass, "/webclass/after-sso")),
        ] {
            assert_eq!(
                guard.reserve(&method, &url, false),
                Err(NetworkError::Policy)
            );
        }
        assert_eq!(*meter.lock().unwrap(), WebClassRequestCounts::default());
    }
}
