use std::path::Path;

use assignment_manager::{
    notion::normalize_data_source_id,
    observation::ObservationValidationError,
    secured_sync::SecuredSyncClients,
    sync::{
        WebClassCourseCheck, WebClassCourseListError, login_and_acquire_webclass_courses,
        observe_webclass_saml_before_consent,
    },
    university_network::NetworkPolicy,
    webclass_auth::{HoseiAuthError, ProtocolReason, SamlProbeStop},
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

pub(in crate::cli) fn check_http_local(path: &Path) -> Result<u8, CliError> {
    let target = credential_namespace(&config::read_bytes(path)?)?;
    credentials::check_webclass_entry(&target).map_err(CliError::Credential)?;
    println!("webclass configuration=ready credential_entry=present network=disabled");
    Ok(finish(true))
}

pub(in crate::cli) fn check_browser_config(explicit: Option<&Path>) -> Result<(), CliError> {
    let default;
    let path = match explicit {
        Some(path) => path,
        None => {
            let Ok(root) = config::root() else {
                return Ok(());
            };
            default = root.join("config.json");
            &default
        }
    };
    match config::read_bytes(path) {
        Ok(bytes) => credential_namespace(&bytes).map(|_| ()),
        Err(ConfigError::Missing) if explicit.is_none() => Ok(()),
        Err(error) => Err(CliError::Config(error)),
    }
}

pub(in crate::cli) fn check_browser_local(explicit: Option<&Path>) -> Result<u8, CliError> {
    check_browser_config(explicit)?;
    #[cfg(any(windows, target_os = "macos"))]
    let dependencies = assignment_manager::webclass_browser::check_local_dependencies();
    #[cfg(not(any(windows, target_os = "macos")))]
    let dependencies = Err("windows_or_macos_required");
    match dependencies {
        Ok(()) => {
            println!("webclass configuration=ready browser=present node=present network=disabled");
            Ok(finish(true))
        }
        Err(reason) => {
            println!("webclass configuration=ready network=disabled diagnostic={reason}");
            Ok(finish(false))
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(in crate::cli) fn browser(keep_profile: bool, course_id: Option<&str>) -> u8 {
    use assignment_manager::webclass_browser::{self, ProfileMode};

    #[cfg(windows)]
    if !crate::cli::windows_dialog::interactive_station() {
        println!("webclass browser diagnostic=interactive_windows_session_required");
        return finish(false);
    }
    #[cfg(windows)]
    let browser = "Edge";
    #[cfg(target_os = "macos")]
    let browser = "Chrome";
    println!(
        "{browser}の大学公式画面で認証し、HOPPIIのWebClassリンクを開いてください。中止する場合は専用の{browser}を閉じてください。"
    );
    let (mode, profile) = if keep_profile {
        (ProfileMode::Retained, "retained")
    } else {
        (ProfileMode::Temporary, "temporary")
    };
    if let Some(course_id) = course_id {
        let check = webclass_browser::run_for_course(mode, course_id);
        println!(
            "webclass browser lms_reached={} courses_get={} contents_get={} profile={profile}",
            check.lms_reached, check.courses_get, check.contents_get,
        );
        report_browser_navigation(check.navigation);
        return match check.result {
            Ok(result) => {
                use assignment_manager::domain::Deadline;

                println!(
                    "webclass course {}",
                    serde_json::json!({"group_id": result.course.group_id, "group_name": result.course.group_name})
                );
                let jst = chrono::FixedOffset::east_opt(9 * 3600)
                    .expect("+09:00 is a valid fixed offset");
                let assignments = result.assignments.batch().as_slice();
                for assignment in assignments {
                    let deadline = assignment.deadline.as_ref().map(|deadline| match deadline {
                        Deadline::DateTime(value) => value.with_timezone(&jst).to_rfc3339(),
                        Deadline::DateOnly(value) => value.to_string(),
                    });
                    println!(
                        "webclass assignment {}",
                        serde_json::json!({
                            "group_id": assignment.external_key.scope,
                            "contents_id": assignment.external_key.id,
                            "title": assignment.title,
                            "deadline": deadline,
                            "submission_url": assignment.submission_url,
                        })
                    );
                }
                println!(
                    "webclass assignments={} validation=passed",
                    assignments.len()
                );
                finish(true)
            }
            Err(reason) => {
                println!("webclass assignments=0 validation=failed diagnostic={reason}");
                finish(false)
            }
        };
    }
    let check = webclass_browser::run(mode);
    println!(
        "webclass browser lms_reached={} courses_get={} profile={profile}",
        check.lms_reached, check.courses_get
    );
    report_browser_navigation(check.navigation);
    match check.result {
        Ok(courses) => {
            for course in courses.records() {
                println!(
                    "webclass course {}",
                    serde_json::json!({
                        "group_id": course.group_id,
                        "group_name": course.group_name,
                        "year": course.year,
                        "semester": course.semester,
                        "day_of_week": course.day_of_week,
                        "class_order": course.class_order,
                    })
                );
            }
            for ((year, semester), count) in course_period_counts(courses.records()) {
                println!(
                    "webclass course_period {}",
                    serde_json::json!({"year": year, "semester": semester, "count": count})
                );
            }
            println!(
                "webclass courses={} validation=passed enrollment=undetermined",
                courses.records().len()
            );
            finish(true)
        }
        Err(reason) => {
            println!("webclass courses=0 validation=failed diagnostic={reason}");
            finish(false)
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn report_browser_navigation(
    navigation: Option<assignment_manager::webclass_browser::BrowserNavigation>,
) {
    if let Some(navigation) = navigation {
        println!(
            "webclass browser_navigation {}",
            serde_json::json!(navigation)
        );
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(in crate::cli) fn browser(_keep_profile: bool, _course_id: Option<&str>) -> u8 {
    println!("webclass browser diagnostic=windows_or_macos_required");
    finish(false)
}

#[cfg(any(windows, target_os = "macos"))]
fn course_period_counts(
    records: &[assignment_manager::webclass::WebClassCourse],
) -> std::collections::BTreeMap<(i32, &str), usize> {
    let mut counts = std::collections::BTreeMap::new();
    for course in records {
        *counts
            .entry((course.year, course.semester.as_str()))
            .or_default() += 1;
    }
    counts
}

#[cfg(all(test, any(windows, target_os = "macos")))]
#[test]
fn course_summary_counts_source_periods_without_normalizing_them() {
    let raw = include_str!("../../../fixtures/webclass/courses_basic.json");
    let mut courses = assignment_manager::webclass::parse_courses(raw).unwrap();
    courses[0].year = 2025;
    courses[1].semester.clear();
    assert_eq!(
        course_period_counts(&courses)
            .into_iter()
            .collect::<Vec<_>>(),
        [((2025, "2"), 1), ((2026, ""), 1), ((2026, "2"), 1)]
    );
}

pub(in crate::cli) fn http_probe(
    path: &Path,
    review_local: bool,
    stop_before_consent: bool,
) -> Result<u8, CliError> {
    if review_local && !can_review() {
        return Ok(finish(false));
    }
    let target = credential_namespace(&config::read_bytes(path)?)?;
    let mut clients = SecuredSyncClients::new(NetworkPolicy::default());
    clients.begin_run();
    let acquired = credentials::with_webclass_credentials(&target, |credentials| {
        if stop_before_consent {
            observe_webclass_saml_before_consent(&clients.authenticator(), credentials)
        } else {
            login_and_acquire_webclass_courses(&clients.authenticator(), credentials)
        }
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
    if let Some(consent) = &check.consent
        && let Ok(summary) = serde_json::to_string(consent)
    {
        println!(
            "webclass consent_get={} consent_observation={summary}",
            u8::from(r.idp.get >= 3)
        );
    }
    let courses = match check.result {
        Err(WebClassCourseListError::Authentication(HoseiAuthError::SamlProbeStopped(result))) => {
            let status = result.status.map(|code| code.as_u16().to_string());
            println!(
                "webclass saml_query={} acs_url={} success_get={} http_status={} saml_form={} form_method={} action_acs={} saml_response={} relay_state={} in_response_to={} stop={:?}",
                comparison(result.query_matches),
                comparison(result.acs_url_matches),
                u8::from(r.idp.get >= 2),
                status.as_deref().unwrap_or("none"),
                match result.form_recognized {
                    Some(true) => "recognized",
                    Some(false) => "unrecognized",
                    None => "undetermined",
                },
                result.form_method.unwrap_or("undetermined"),
                comparison(result.action_matches),
                presence(result.saml_response_present),
                presence(result.relay_state_present),
                comparison(result.in_response_to_matches),
                result.stop,
            );
            return finish(matches!(
                result.stop,
                SamlProbeStop::ActionMatched | SamlProbeStop::ConsentPageObserved
            ));
        }
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
            if let WebClassCourseListError::Authentication(
                HoseiAuthError::ProtocolChanged(step)
                | HoseiAuthError::ProtocolChangedWithReason(step, _)
                | HoseiAuthError::RedirectLimit(step)
                | HoseiAuthError::HttpStatus(step, _)
                | HoseiAuthError::Request(step)
                | HoseiAuthError::Timeout(step),
            ) = error
            {
                println!("webclass authentication_stage={step:?}");
            }
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

fn comparison(result: Option<bool>) -> &'static str {
    match result {
        Some(true) => "match",
        Some(false) => "mismatch",
        None => "undetermined",
    }
}

fn presence(result: Option<bool>) -> &'static str {
    match result {
        Some(true) => "present",
        Some(false) => "absent",
        None => "undetermined",
    }
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
