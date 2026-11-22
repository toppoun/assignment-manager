use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

use assignment_manager::notion::{CourseCreationTrial, CourseTrialError, TrialRole};
use serde::Deserialize;

use super::{CliError, Config, config, credentials, notion_diagnosis};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Action {
    CheckLocal,
    Preview,
    Create,
    Verify,
}

impl Action {
    pub(super) fn parse(word: &str) -> Result<Self, CliError> {
        match word {
            "check-local" => Ok(Self::CheckLocal),
            "--dry-run" => Ok(Self::Preview),
            "--allow-notion-create" => Ok(Self::Create),
            "--verify" => Ok(Self::Verify),
            _ => Err(CliError::Usage),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    categories: Vec<String>,
}

pub(super) fn execute(path: &Path, action: Action) -> Result<u8, CliError> {
    // Keep all three stages on the same fixed input and journal in this workspace.
    let expected = Path::new("local").join("notion-moodle-live-preview.json");
    let canonical = |file: &Path| {
        file.canonicalize()
            .map_err(|_| CliError::Config(config::ConfigError::Unreadable))
    };
    if canonical(path)? != canonical(&expected)? {
        return Err(CliError::Config(config::ConfigError::Invalid));
    }
    if action == Action::Create
        && !(io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal())
    {
        println!("result=failed error=trial_requires_terminal");
        return Ok(2);
    }
    let directory = expected.parent().expect("fixed local path");
    let journal = directory.join("notion-moodle-courses-run.journal");
    let ready = directory.join("notion-moodle-live-preview.ready.json");
    let original = config::read_bytes(&expected)?;
    let configuration = Config::parse(&original)?;
    if !configuration.course_mappings.is_empty() {
        return Err(CliError::Config(config::ConfigError::Invalid));
    }
    let manifest: Manifest = serde_json::from_slice(&config::read_bytes(
        &directory.join("notion-moodle-courses.json"),
    )?)
    .map_err(|_| CliError::Config(config::ConfigError::Invalid))?;
    let approved = manifest.categories.len();
    let trial = CourseCreationTrial::new(configuration.notion, manifest.categories)
        .map_err(CliError::MoodlePlanNotion)?;
    if action == Action::CheckLocal {
        let present = credentials::moodle_trial_writer_present(trial.target())
            .map_err(CliError::Credential)?;
        let started = journal.try_exists().map_err(|_| CliError::Receipt)?;
        println!(
            "configuration=ready approved={approved} writer_entry={} attempt_journal_present={started} network=disabled",
            if present { "present" } else { "missing" }
        );
        return Ok(if present && !started { 0 } else { 3 });
    }

    if action == Action::Create {
        let client = credentials::trial_client(trial.target(), TrialRole::Write)
            .map_err(CliError::Credential)?;
        return match trial.create(&client, &journal, || confirm(approved)) {
            Ok(true) => {
                println!("result=success confirmed={approved} verification_required=true");
                Ok(0)
            }
            Ok(false) => {
                println!("result=cancelled");
                Ok(2)
            }
            Err(error) => report_error(error),
        };
    }

    let (client, role) = if action == Action::Preview {
        (
            credentials::trial_client(trial.target(), TrialRole::Write),
            TrialRole::Write,
        )
    } else {
        (
            credentials::notion_client(trial.target().assignment_id()),
            TrialRole::Read,
        )
    };
    let client = client.map_err(CliError::Credential)?;
    let snapshot = trial
        .inspect(&client, role)
        .map_err(CliError::MoodlePlanNotion)?;
    let found: Vec<_> = snapshot.existing_ordinals().collect();
    let missing = approved - found.len();
    let started = journal.try_exists().map_err(|_| CliError::Receipt)?;
    if action == Action::Preview {
        let can_create = found.is_empty() && !started;
        println!(
            "phase=preview writer_id=verified schema=verified complete=true total={} approved={approved} existing_exact={} missing={missing} attempt_journal_present={started} can_create={can_create} notion_writes=0",
            snapshot.total_courses(),
            found.len(),
        );
        return Ok(if can_create { 0 } else { 3 });
    }
    println!(
        "phase=verify existing_exact={} missing={missing} verified_ordinals={found:?} notion_writes=0",
        found.len(),
    );
    if missing != 0 {
        println!("result=incomplete");
        return Ok(3);
    }
    let mappings = match trial.verified_mappings(&snapshot, &journal) {
        Ok(mappings) => mappings,
        Err(error) => return report_error(error),
    };
    if config::read_bytes(&expected)? != original {
        return Err(CliError::Config(config::ConfigError::Invalid));
    }
    let mut staged: serde_json::Value =
        serde_json::from_slice(&original).map_err(|_| CliError::Receipt)?;
    staged["course_mappings"] = serde_json::to_value(&mappings).map_err(|_| CliError::Receipt)?;
    let bytes = serde_json::to_vec_pretty(&staged).map_err(|_| CliError::Receipt)?;
    Config::parse(&bytes)?;
    let mut output = tempfile::NamedTempFile::new_in(directory).map_err(|_| CliError::Receipt)?;
    output
        .write_all(&bytes)
        .and_then(|()| output.as_file().sync_all())
        .map_err(|_| CliError::Receipt)?;
    output
        .persist_noclobber(ready)
        .map_err(|_| CliError::Receipt)?;
    println!(
        "result=success total={} baseline_unchanged=true mappings_prepared={} live_configuration_changed=false",
        snapshot.total_courses(),
        mappings.len()
    );
    Ok(0)
}

fn confirm(approved: usize) -> bool {
    println!("phase=plan create={approved} update=0 fields=title");
    print!(
        "Type CREATE SIX COURSES to add the six approved courses to the verification data source: "
    );
    let mut answer = String::new();
    io::stdout().flush().is_ok()
        && io::stdin().read_line(&mut answer).is_ok()
        && answer.trim() == "CREATE SIX COURSES"
}

fn report_error(error: CourseTrialError) -> Result<u8, CliError> {
    let diagnostic = match error {
        CourseTrialError::Notion(error) => return Err(CliError::MoodlePlanNotion(error)),
        CourseTrialError::Journal => "course_journal_unavailable",
        CourseTrialError::AlreadyStarted => "course_batch_already_started",
        CourseTrialError::PlanChanged => "course_plan_changed",
        CourseTrialError::ResultUnconfirmed(error) => {
            println!("notion_error={}", notion_diagnosis(error));
            "course_create_unconfirmed"
        }
    };
    println!("result=failed error={diagnostic}");
    Ok(3)
}
