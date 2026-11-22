use std::{
    io::{self, IsTerminal, Write},
    path::Path,
};

use assignment_manager::{
    notion::{
        ApplyError, MoodleCreateTarget, TrialRole,
        state::{StateStore, TargetLock},
    },
    webclass_auth::synthetic::Scenario,
};

use super::{
    CliError, Config, Paths, config, credentials, fixture_run, initialize, notion_diagnosis,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    Doctor,
    Initialize,
    RegisterWrite,
    Preview(String),
    Apply(String),
    Reconcile(String),
}

impl Action {
    pub(super) fn parse(words: &[&str]) -> Result<Self, CliError> {
        Ok(match words {
            ["doctor"] => Self::Doctor,
            ["init-state"] => Self::Initialize,
            ["credentials", "set", "write"] => Self::RegisterWrite,
            ["--dry-run", "--uid", uid] => Self::Preview((*uid).into()),
            ["--allow-notion-write", "--uid", uid] => Self::Apply((*uid).into()),
            ["reconcile", "--uid", uid] => Self::Reconcile((*uid).into()),
            _ => return Err(CliError::Usage),
        })
    }
}

pub(super) fn execute(path: &Path, writer: &str, action: &Action) -> Result<u8, CliError> {
    if matches!(action, Action::Apply(_))
        && !(io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal())
    {
        println!("result=failed error=trial_requires_terminal");
        return Ok(2);
    }
    let config = Config::read(path)?;
    let target =
        MoodleCreateTarget::new(config.notion, writer).map_err(CliError::MoodlePlanNotion)?;
    let trial = target.trial();
    let id = trial.assignment_id();
    let paths = Paths {
        target: id.into(),
        directory: config::root()?.join("moodle-create-trial").join(id),
    };
    let uid = match action {
        Action::Doctor => {
            let present =
                credentials::moodle_trial_writer_present(trial).map_err(CliError::Credential)?;
            let state = StateStore::read(&paths.state(), id)?;
            println!(
                "writer_entry={} state={} pending={} network=disabled",
                if present { "present" } else { "missing" },
                if state.is_some() { "ready" } else { "missing" },
                state.map_or(0, |state| state.pending)
            );
            return Ok(0);
        }
        Action::Initialize => return initialize(&paths),
        Action::RegisterWrite => {
            let _lock = TargetLock::acquire(&paths.directory)?;
            credentials::register_moodle_trial_writer(trial).map_err(CliError::Credential)?;
            println!("writer_entry=stored");
            return Ok(0);
        }
        Action::Preview(uid) | Action::Apply(uid) | Action::Reconcile(uid) => uid,
    };
    let input = fixture_run::moodle_only(Scenario::MoodleReview)?;
    if input.assignments().len() != 3 {
        return Err(CliError::FixtureFailed);
    }
    let input = input
        .select_fixture_moodle(uid)
        .map_err(CliError::MoodlePlanNotion)?;
    let client = if matches!(action, Action::Apply(_)) {
        credentials::trial_client(trial, TrialRole::Write)
    } else {
        credentials::notion_client(id)
    }
    .map_err(CliError::Credential)?;
    let mappings = &config.course_mappings;
    let directory = &paths.directory;
    let result = match action {
        Action::Preview(_) => client.preview_moodle_create(&target, &input, mappings, directory).map(|preview| {
            let c = preview.counts;
            println!("phase=dry_run selected=1 existing_rows={} create={} update={} unchanged={} conflict={} deferred={} course_relation_mismatch={} pending={} can_create={} other_managed_revision={}",
                preview.existing_rows, c.create, c.update, c.unchanged, c.conflict, c.deferred,
                c.course_relation_mismatch, preview.pending, preview.can_create, preview.other_managed_revision);
            if c.unchanged == 1 && c.course_relation_mismatch == 0 || preview.can_create { 0 } else { 3 }
        }),
        Action::Apply(_) => client.apply_moodle_create(&target, &input, mappings, directory, confirm).map(|report| {
            let Some(report) = report else {
                println!("result=cancelled");
                return 2;
            };
            println!("phase=apply create={} update=0 conflict={} deferred={} pending={}",
                report.counts.create, report.counts.conflict, report.counts.deferred, report.pending);
            if let Some(error) = report.error {
                println!("notion_error={}", notion_diagnosis(error));
            }
            if report.counts.create == 1 && report.pending == 0 { 0 } else { 3 }
        }),
        Action::Reconcile(_) => client.reconcile_moodle_create(&target, &input, mappings, directory).map(|()| {
            println!("phase=reconcile linked=1 notion_writes=0");
            0
        }),
        _ => unreachable!("local trial commands returned before creating a client"),
    };
    match result {
        Ok(code) => Ok(code),
        Err(ApplyError::State(error)) => Err(CliError::State(error)),
        Err(ApplyError::Notion(error)) => Err(CliError::MoodlePlanNotion(error)),
        Err(ApplyError::CreateBlocked) => {
            println!("result=failed error=create_blocked");
            Ok(3)
        }
        Err(ApplyError::PlanChanged) => {
            println!("result=failed error=plan_changed");
            Ok(3)
        }
    }
}

fn confirm() -> bool {
    println!("phase=plan selected=1 create=1 update=0 relation=verified");
    print!("Type CREATE ONE to create the selected synthetic assignment: ");
    let mut answer = String::new();
    io::stdout().flush().is_ok()
        && io::stdin().read_line(&mut answer).is_ok()
        && answer.trim() == "CREATE ONE"
}
