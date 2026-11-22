#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;

use super::CliError;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Preview,
    Apply,
    Initialize,
    Status,
    RegisterRead,
    RegisterWrite,
}

impl Command {
    pub(super) fn parse(words: &[&str]) -> Result<Self, CliError> {
        Ok(match words {
            [] | ["--dry-run"] => Self::Preview,
            ["--allow-notion-write"] => Self::Apply,
            ["init-state"] => Self::Initialize,
            ["status"] => Self::Status,
            ["credentials", "set", "read"] => Self::RegisterRead,
            ["credentials", "set", "write"] => Self::RegisterWrite,
            _ => return Err(CliError::Usage),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn execute(_: &str, _: Command, _: Option<PathBuf>) -> Result<u8, CliError> {
    Err(CliError::TrialUnavailable)
}

#[cfg(target_os = "macos")]
pub(super) use macos::execute;

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        io::{self, IsTerminal, Write},
        path::PathBuf,
    };

    use assignment_manager::{
        domain::Deadline,
        notion::{
            ApplyError, ApplyPreview, ApplyReport, NotionError, NotionTarget, PlanAction,
            TrialAssignment, TrialRole, TrialTarget, state::TargetLock,
        },
    };
    use serde::Deserialize;

    use super::{CliError, Command};
    use crate::cli::{
        Paths,
        config::{self, ConfigError},
        credentials, initialize, notion_diagnosis, status,
    };

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        notion: NotionTarget,
        read_bot_id: String,
        write_bot_id: String,
        assignment: TrialAssignment,
    }

    pub(in crate::cli) fn execute(
        selected: &str,
        command: Command,
        config_path: Option<PathBuf>,
    ) -> Result<u8, CliError> {
        if command == Command::Apply
            && !(io::stdin().is_terminal()
                && io::stdout().is_terminal()
                && io::stderr().is_terminal())
        {
            return Err(CliError::TrialRequiresTerminal);
        }
        let root = config::root()?;
        let path = config_path.unwrap_or_else(|| root.join("trial.json"));
        let config: Config = serde_json::from_slice(&config::read_bytes(&path)?)
            .map_err(|_| ConfigError::Invalid)?;
        let target = TrialTarget::new(
            config.notion,
            selected,
            &config.read_bot_id,
            &config.write_bot_id,
        )
        .map_err(|error| match error {
            NotionError::MethodDisabled => CliError::TrialTargetRejected,
            _ => CliError::Config(ConfigError::Invalid),
        })?;
        let paths = Paths {
            target: target.assignment_id().to_owned(),
            directory: root.join("trial-targets").join(target.assignment_id()),
        };
        let role = match command {
            Command::Initialize => return initialize(&paths),
            Command::Status => return status(&paths),
            Command::Preview | Command::RegisterRead => TrialRole::Read,
            Command::Apply | Command::RegisterWrite => TrialRole::Write,
        };
        if matches!(command, Command::RegisterRead | Command::RegisterWrite) {
            let _lock = TargetLock::acquire(&paths.directory)?;
            credentials::register_trial(&target, role).map_err(CliError::Credential)?;
            println!("trial_credential=stored");
            return Ok(0);
        }

        let client = credentials::trial_client(&target, role).map_err(CliError::Credential)?;
        let report = if command == Command::Apply {
            client.apply_trial(&target, &config.assignment, &paths.directory, confirm)
        } else {
            client
                .preview_trial(&target, &config.assignment, &paths.directory)
                .map(Some)
        };
        match report {
            Ok(Some(report)) => Ok(display_result(&report, command == Command::Apply)),
            Ok(None) => {
                println!("result=cancelled");
                Ok(2)
            }
            Err(ApplyError::State(error)) => Err(CliError::State(error)),
            Err(ApplyError::PlanChanged) => {
                println!("result=partial issue=plan_changed");
                Ok(3)
            }
            Err(ApplyError::CreateBlocked) => {
                println!("result=failed error=create_blocked");
                Ok(3)
            }
            #[cfg(feature = "synthetic-https-fixture")]
            Err(ApplyError::UpdateBlocked) => {
                println!("result=failed error=update_blocked");
                Ok(3)
            }
            Err(ApplyError::Notion(error)) => {
                println!("result=failed notion_error={}", notion_diagnosis(error));
                Ok(1)
            }
        }
    }

    fn confirm(preview: ApplyPreview<'_>) -> bool {
        display_counts("plan", &preview.summary());
        for entry in preview.assignments() {
            match &entry.action {
                PlanAction::Create {
                    assignment,
                    course_page_id,
                } => println!(
                    "create title={:?} deadline={} source=Moodle set_relation={}",
                    assignment.title,
                    deadline(assignment.deadline.as_ref()),
                    course_page_id.is_some()
                ),
                PlanAction::Update { patch, .. } => println!(
                    "update new_title={:?} deadline_change={} source_change={:?} fill_relation={}",
                    patch.title,
                    patch
                        .deadline
                        .as_ref()
                        .map_or_else(|| "unchanged".into(), |value| deadline(value.as_ref())),
                    patch.source,
                    patch.course_page_id.is_some()
                ),
                _ => {}
            }
        }
        print!("Type APPLY to approve this trial plan: ");
        let mut reply = String::new();
        io::stdout().flush().is_ok()
            && io::stdin().read_line(&mut reply).is_ok()
            && reply.trim() == "APPLY"
    }

    fn deadline(value: Option<&Deadline>) -> String {
        match value {
            Some(Deadline::DateOnly(date)) => date.to_string(),
            Some(Deadline::DateTime(time)) => time.to_rfc3339(),
            None => "empty".into(),
        }
    }

    fn display_counts(phase: &str, report: &ApplyReport) {
        let counts = report.counts;
        println!(
            "phase={phase} target=trial create={} update={} unchanged={} conflict={} deferred={} course_relation_mismatch={} recovered={} pending={} manual_candidates={}",
            counts.create,
            counts.update,
            counts.unchanged,
            counts.conflict,
            counts.deferred,
            counts.course_relation_mismatch,
            report.recovered,
            report.pending,
            report.manual_candidates
        );
    }

    fn display_result(report: &ApplyReport, applied: bool) -> u8 {
        display_counts(if applied { "apply" } else { "dry_run" }, report);
        if let Some(error) = report.error {
            let diagnosis = if error == NotionError::Conflict {
                "conflict"
            } else {
                notion_diagnosis(error)
            };
            println!("notion_error={diagnosis}");
        }
        if report.error.is_some()
            || report.counts.deferred != 0
            || report.counts.conflict != 0
            || report.pending != 0
        {
            println!("result=partial");
            3
        } else {
            println!("result=success");
            0
        }
    }
}
