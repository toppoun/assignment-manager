use std::path::Path;

use assignment_manager::{
    notion::{ApplyError, CourseSetupInput, CourseSetupVerification, NotionError},
    sync::acquire_moodle_calendar,
    webclass_browser::{self, ProfileMode},
};
use serde_json::json;

use super::{prepare_moodle_request, secured_moodle_policy};
use crate::cli::{
    CliError, Config, ConfigError, CredentialStatus, config as local_config, credentials,
    notion_diagnosis,
};

pub(in crate::cli) fn execute(
    config: &Config,
    config_path: &Path,
    term: &str,
    expected_plan: Option<&str>,
) -> Result<u8, CliError> {
    if config
        .notion
        .course_rules
        .as_ref()
        .is_none_or(|rules| rules.setup.is_none())
    {
        return Err(ConfigError::Invalid.into());
    }
    let verification = match CourseSetupVerification::new(&config.notion) {
        Ok(target) => Some(target),
        Err(error) if expected_plan.is_some() => return finish_error(error.into()),
        Err(_) => None,
    };
    if !config.webclass {
        println!("webclass=disabled");
        return Err(CliError::Credential(CredentialStatus::Disabled));
    }
    let moodle = config.moodle.as_ref().ok_or_else(|| {
        println!("moodle=disabled");
        CliError::Credential(CredentialStatus::Disabled)
    })?;
    let directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let Some(policy) = secured_moodle_policy(moodle, directory, false)? else {
        return Ok(1);
    };
    let client = credentials::notion_client(&config.target_id()?).map_err(CliError::Credential)?;
    let (mut clients, feed) = prepare_moodle_request(config, moodle, policy)?;
    let writer = if expected_plan.is_some() {
        let target = verification.as_ref().ok_or(ConfigError::Invalid)?;
        Some(credentials::course_setup_writer(target).map_err(CliError::Credential)?)
    } else {
        None
    };
    let setup_directory = verification
        .as_ref()
        .map(|target| {
            Ok::<_, ConfigError>(
                local_config::root()?
                    .join("course-setup-targets")
                    .join(target.rules_id()),
            )
        })
        .transpose()?;
    if let Err(reason) = webclass_browser::check_local_dependencies() {
        println!("webclass diagnostic={reason}");
        return Ok(1);
    }
    let profile = if super::webclass::interactive_terminal() {
        super::webclass::prompt_browser_authentication();
        ProfileMode::Temporary
    } else {
        ProfileMode::Headless
    };
    let check = super::webclass::read_browser_course_list(profile, false);
    let courses = match check {
        Ok(courses) => courses,
        Err(reason) => {
            println!("webclass diagnostic={reason}");
            return Ok(1);
        }
    };
    clients.begin_run();
    let Ok(moodle_client) = clients.moodle() else {
        println!("moodle diagnostic=client_initialization");
        return Ok(1);
    };
    let events = match acquire_moodle_calendar(&moodle_client, &feed) {
        Ok(events) => events,
        Err(error) => {
            println!("moodle diagnostic={}", error.diagnostic.code());
            if let Some(status) = error.diagnostic.http_status() {
                println!("moodle http_status={status}");
            }
            return Ok(1);
        }
    };
    let input = match CourseSetupInput::new(term, &courses, &events, &feed) {
        Ok(input) => input,
        Err(error) => return finish_error(error.into()),
    };
    if let (Some(id), Some(writer), Some(directory)) = (expected_plan, writer, &setup_directory) {
        return match client.apply_verified_course_setup(
            &writer,
            &config.notion,
            &input,
            directory,
            id,
        ) {
            Ok(counts) => {
                println!(
                    "created_courses={} created_rules={} reuse_courses={} reuse_rules={} held={}",
                    counts.create_courses,
                    counts.create_rules,
                    counts.reuse_courses,
                    counts.reuse_rules,
                    counts.held
                );
                println!(
                    "result={} mode=apply",
                    if counts.held == 0 {
                        "success"
                    } else {
                        "partial"
                    }
                );
                Ok(if counts.held == 0 { 0 } else { 3 })
            }
            Err(error) => finish_error(error),
        };
    }
    let report = match setup_directory {
        Some(directory) => client.preview_verified_course_setup(&config.notion, &input, &directory),
        None => client
            .preview_course_setup(&config.notion, &input)
            .map_err(ApplyError::from),
    };
    let report = match report {
        Ok(report) => report,
        Err(error) => return finish_error(error),
    };
    println!(
        "term={term} webclass_courses={} moodle_categories={}",
        report.webclass_courses, report.moodle_categories
    );
    for item in &report.items {
        println!("candidate {}", json!(item));
    }
    for course in &report.existing_courses_without_group_id {
        println!("existing_course_without_group_id {}", json!(course));
    }
    let counts = report.counts;
    println!(
        "create_courses={} create_rules={} reuse_courses={} reuse_rules={} held={}",
        counts.create_courses,
        counts.create_rules,
        counts.reuse_courses,
        counts.reuse_rules,
        counts.held
    );
    if verification.is_some() {
        println!("plan_id={}", report.plan_id);
    }
    println!(
        "result={} mode=preview",
        if counts.held == 0 {
            "success"
        } else {
            "partial"
        }
    );
    Ok(if counts.held == 0 { 0 } else { 3 })
}

fn finish_error(error: ApplyError) -> Result<u8, CliError> {
    match error {
        ApplyError::State(error) => Err(CliError::State(error)),
        ApplyError::PlanChanged => {
            println!("result=stopped issue=plan_changed");
            Ok(3)
        }
        ApplyError::Notion(error) => {
            println!("course_setup diagnostic={}", notion_diagnosis(error));
            Ok(if error == NotionError::MethodDisabled {
                2
            } else {
                1
            })
        }
        _ => {
            println!("result=stopped issue=reconciliation_required");
            Ok(3)
        }
    }
}
