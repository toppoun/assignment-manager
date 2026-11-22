use std::path::Path;

use assignment_manager::{
    notion::normalize_data_source_id,
    observation::ObservationValidationError,
    secured_sync::SecuredSyncClients,
    sync::{WebClassCourseCheck, WebClassCourseListError, login_and_acquire_webclass_courses},
    university_network::NetworkPolicy,
    webclass_auth::{HoseiAuthError, ProtocolReason},
    webclass_http::WebClassHttpError,
};
use serde::Deserialize;
use serde_json::error::Category;

use crate::cli::{CliError, Config, ConfigError, CredentialStatus, config, credentials};

use super::finish;

#[cfg(windows)]
mod review;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CourseCheckConfig {
    credential_namespace: String,
}

fn credential_namespace(bytes: &[u8]) -> Result<String, CliError> {
    if let Ok(config) = serde_json::from_slice::<CourseCheckConfig>(bytes) {
        return normalize_data_source_id(&config.credential_namespace)
            .ok_or(CliError::Config(ConfigError::Invalid));
    }
    let config = Config::parse(bytes)?;
    if !config.webclass {
        return Err(CliError::Credential(CredentialStatus::Disabled));
    }
    Ok(config.target_id()?)
}

pub(in crate::cli) fn check_local(path: &Path) -> Result<u8, CliError> {
    let target = credential_namespace(&config::read_bytes(path)?)?;
    credentials::check_webclass_entry(&target).map_err(CliError::Credential)?;
    println!("webclass configuration=ready credential_entry=present network=disabled");
    Ok(finish(true))
}

pub(in crate::cli) fn check(path: &Path, review_local: bool) -> Result<u8, CliError> {
    if review_local && !can_review() {
        return Ok(finish(false));
    }
    let target = credential_namespace(&config::read_bytes(path)?)?;
    let mut clients = SecuredSyncClients::new(NetworkPolicy::default());
    clients.begin_run();
    let acquired = credentials::with_webclass_credentials(&target, |credentials| {
        login_and_acquire_webclass_courses(&clients.authenticator(), credentials)
    })
    .map_err(CliError::Credential)?;
    drop(clients);
    Ok(report(acquired, review_local))
}

#[cfg(feature = "synthetic-https-fixture")]
pub(in crate::cli) fn fixture(review_local: bool) -> u8 {
    use assignment_manager::webclass_auth::{
        HoseiCredentials,
        synthetic::{Scenario, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD},
    };

    if review_local && !can_review() {
        return finish(false);
    }
    let university = SyntheticUniversity::start(Scenario::WebClassCourses);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let acquired = login_and_acquire_webclass_courses(
        &clients.authenticator(),
        HoseiCredentials::new(UNIVERSITY_ID, UNIVERSITY_PASSWORD),
    );
    drop((clients, university));
    report(acquired, review_local)
}

fn report(check: WebClassCourseCheck, review_local: bool) -> u8 {
    let r = check.requests;
    println!(
        "webclass requests hoppii_get={} hoppii_post={} idp_get={} idp_post={} lms_get={} lms_post={} courses_get={} total={}",
        r.hoppii.get,
        r.hoppii.post,
        r.idp.get,
        r.idp.post,
        r.webclass.get,
        r.webclass.post,
        r.courses_get,
        u16::from(r.hoppii.get)
            + u16::from(r.hoppii.post)
            + u16::from(r.idp.get)
            + u16::from(r.idp.post)
            + u16::from(r.webclass.get)
            + u16::from(r.webclass.post)
    );
    let courses = match check.result {
        Ok(courses) if courses.records().is_empty() => {
            println!("webclass courses=0 validation=empty");
            return finish(false);
        }
        Ok(courses) => courses,
        Err(error) => {
            println!(
                "webclass courses=0 validation=failed diagnostic={}",
                diagnostic(error)
            );
            return finish(false);
        }
    };
    println!(
        "webclass courses={} validation=passed",
        courses.records().len()
    );
    #[cfg(windows)]
    if review_local && review::show(courses.records()).is_err() {
        println!("review diagnostic=review_window_unavailable");
        return finish(false);
    }
    #[cfg(not(windows))]
    let _ = review_local;
    finish(true)
}

fn can_review() -> bool {
    #[cfg(windows)]
    if crate::cli::windows_dialog::interactive_station() {
        return true;
    } else {
        println!("review diagnostic=review_window_unavailable");
    }
    #[cfg(not(windows))]
    println!("review diagnostic=review_windows_required");
    false
}

fn diagnostic(error: WebClassCourseListError) -> &'static str {
    match error {
        WebClassCourseListError::Authentication(HoseiAuthError::ProtocolChangedWithReason(
            _,
            ProtocolReason::RequestNotAllowed,
        )) => "authentication_request_blocked",
        WebClassCourseListError::Authentication(HoseiAuthError::ConsentRequired) => {
            "consent_required"
        }
        WebClassCourseListError::Authentication(HoseiAuthError::RequestLimit) => "request_limit",
        WebClassCourseListError::Authentication(
            HoseiAuthError::ProtocolChanged(_)
            | HoseiAuthError::ProtocolChangedWithReason(_, _)
            | HoseiAuthError::RedirectLimit(_),
        ) => "authentication_protocol",
        WebClassCourseListError::Authentication(_) => "authentication",
        WebClassCourseListError::Fetch(WebClassHttpError::AuthExpired) => "auth_expired",
        WebClassCourseListError::Fetch(WebClassHttpError::IncompleteResponse) => {
            "incomplete_response"
        }
        WebClassCourseListError::Fetch(
            WebClassHttpError::UnexpectedResponse | WebClassHttpError::BodyDecode,
        ) => "response_format",
        WebClassCourseListError::Fetch(_) => "fetch",
        WebClassCourseListError::Parse(error) => match error.category {
            Category::Eof => "incomplete_json",
            Category::Data => "response_format",
            _ => "json_parse",
        },
        WebClassCourseListError::Limit => "acquisition_limit",
        WebClassCourseListError::Validation(ObservationValidationError::EmptyIdentity) => {
            "empty_group_id"
        }
        WebClassCourseListError::Validation(ObservationValidationError::DuplicateIdentity) => {
            "duplicate_group_id"
        }
        WebClassCourseListError::Validation(_) => "validation",
    }
}
