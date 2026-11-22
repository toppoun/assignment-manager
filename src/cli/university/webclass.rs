use assignment_manager::{
    observation::ObservationValidationError,
    secured_sync::SecuredSyncClients,
    sync::{AcquiredWebClassCourses, WebClassCourseListError, login_and_acquire_webclass_courses},
    university_network::NetworkPolicy,
    webclass_auth::HoseiAuthError,
    webclass_http::WebClassHttpError,
};
use serde_json::error::Category;

use crate::cli::{CliError, Config, CredentialStatus, credentials};

use super::finish;

#[cfg(windows)]
mod review;

pub(in crate::cli) fn check(config: &Config, review_local: bool) -> Result<u8, CliError> {
    if review_local && !can_review() {
        return Ok(finish(false));
    }
    if !config.webclass {
        return Err(CliError::Credential(CredentialStatus::Disabled));
    }
    let mut clients = SecuredSyncClients::new(NetworkPolicy::default());
    clients.begin_run();
    let acquired = credentials::with_webclass_credentials(&config.target_id()?, |credentials| {
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
    let university = SyntheticUniversity::start(Scenario::BothSuccess);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let acquired = login_and_acquire_webclass_courses(
        &clients.authenticator(),
        HoseiCredentials::new(UNIVERSITY_ID, UNIVERSITY_PASSWORD),
    );
    drop((clients, university));
    report(acquired, review_local)
}

fn report(
    acquired: Result<AcquiredWebClassCourses, WebClassCourseListError>,
    review_local: bool,
) -> u8 {
    let courses = match acquired {
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
