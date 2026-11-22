use std::path::Path;

use assignment_manager::notion::{
    ApplyError, MoodleLiveTarget, MoodlePreviewMappings, TrialRole, VerificationTarget,
    state::{StateError, StateStore},
};

use super::{
    CliError, Config, Paths, config, credentials, initialize,
    moodle_integration::{Acquisition, acquire_live_moodle, checked_moodle_input},
    moodle_review, notion_diagnosis,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Create,
    Reconcile,
}

pub(super) fn initialize_state(path: &Path) -> Result<u8, CliError> {
    let config = Config::read(path)?;
    let target = VerificationTarget::moodle_integration(config.notion)
        .map_err(CliError::MoodlePlanNotion)?;
    initialize(&paths(target.assignment_id())?)
}

pub(super) fn execute(
    path: &Path,
    moodle_path: &Path,
    require_windows_vpn: bool,
    action: Action,
) -> Result<u8, CliError> {
    let configuration = Config::read(path)?;
    let mappings = MoodlePreviewMappings::new(&configuration.course_mappings)
        .map_err(CliError::MoodlePlanNotion)?;
    if configuration.course_mappings.is_empty() {
        println!("result=blocked reason=incomplete_course_mappings");
        return Ok(3);
    }
    let target = MoodleLiveTarget::new(configuration.notion).map_err(CliError::MoodlePlanNotion)?;
    let id = target.trial().assignment_id();
    let paths = paths(id)?;
    let Some(state) = StateStore::read(&paths.state(), id)? else {
        println!("result=blocked reason=state_missing");
        return Ok(3);
    };
    let required_pending = usize::from(action == Action::Reconcile);
    if state.pending != required_pending {
        println!(
            "result=blocked reason=state_pending pending={}",
            state.pending
        );
        return Ok(3);
    }
    let client = match action {
        Action::Create => credentials::trial_client(target.trial(), TrialRole::Write),
        Action::Reconcile => credentials::notion_client(id),
    }
    .map_err(CliError::Credential)?;
    let description = match action {
        Action::Create => {
            "Moodleを取得し、検証用Notionへの1件作成の候補を確認します。作成前に別の確認画面が開きます。"
        }
        Action::Reconcile => "Moodleと検証用Notionを読み取り、保留中の作成1件を照合します。",
    };
    let acquired = match acquire_live_moodle(moodle_path, require_windows_vpn, description)? {
        Acquisition::Fetched(acquired) => acquired,
        Acquisition::Stopped(code) => return Ok(code),
    };
    let input = match checked_moodle_input(acquired.as_ref(), &mappings)? {
        Ok(input) => input,
        Err(code) => return Ok(code),
    };
    drop(acquired);
    let result = match action {
        Action::Create => {
            let result = client.create_one_live_moodle(
                &target,
                &input,
                &mappings,
                &paths.directory,
                |choices| {
                    println!(
                        "phase=select candidates={} eligible={}",
                        choices.candidate_count,
                        choices.eligible.len()
                    );
                    match moodle_review::choose_create(choices) {
                        Ok(selection) => selection,
                        Err(()) => {
                            println!("selection diagnostic=selection_window_unavailable");
                            None
                        }
                    }
                },
                |assignment, held| {
                    println!("phase=plan selected=1 create=1 update=0 held={held}");
                    moodle_review::approve_create(assignment, held)
                },
            );
            match result {
                Ok(Some(report)) if report.counts.create == 1 => {
                    println!(
                        "result=success create=1 update=0 held={} pending=0",
                        input.assignments().len() - 1
                    );
                    Ok(0)
                }
                Ok(Some(report)) if report.counts.deferred != 0 || report.pending != 0 => {
                    println!(
                        "result=unconfirmed update=0 held={} pending={}",
                        input.assignments().len() - 1,
                        report.pending
                    );
                    if let Some(error) = report.error {
                        println!("notion_error={}", notion_diagnosis(error));
                    }
                    Ok(3)
                }
                Ok(Some(_)) => {
                    println!(
                        "result=no_eligible_create create=0 update=0 held={}",
                        input.assignments().len()
                    );
                    Ok(0)
                }
                Ok(None) => {
                    println!("result=cancelled");
                    Ok(2)
                }
                Err(error) => report_error(error),
            }
        }
        Action::Reconcile => {
            match client.reconcile_one_live_moodle(&target, &input, &mappings, &paths.directory) {
                Ok(()) => {
                    println!("result=success linked=1 notion_writes=0");
                    Ok(0)
                }
                Err(error) => report_error(error),
            }
        }
    };
    let (operations, retries) = client.read_counts();
    println!("notion_read operations={operations} retries={retries}");
    result
}

fn paths(id: &str) -> Result<Paths, config::ConfigError> {
    Ok(Paths {
        target: id.into(),
        directory: config::root()?.join("targets").join(id),
    })
}

fn report_error(error: ApplyError) -> Result<u8, CliError> {
    let reason = match error {
        ApplyError::Notion(error) => return Err(CliError::MoodlePlanNotion(error)),
        ApplyError::State(StateError::Database | StateError::InvalidTransition) => {
            println!("result=blocked reason=state_record_failed");
            return Ok(3);
        }
        ApplyError::State(error) => return Err(CliError::State(error)),
        ApplyError::PlanChanged => "plan_changed",
        ApplyError::CreateBlocked => "create_or_reconciliation_blocked",
        #[cfg(feature = "synthetic-https-fixture")]
        ApplyError::UpdateBlocked => "update_blocked",
    };
    println!("result=blocked reason={reason}");
    Ok(3)
}
