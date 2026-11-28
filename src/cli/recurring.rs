use std::{
    collections::{BTreeMap, HashSet},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{
        NotionClient, PlanHash, RecurringInputs, RecurringMode, RecurringPolicy, RecurringReport,
        SyncInput, live_assignment_read_target,
    },
    observation::SyncScope,
    secured_sync::SecuredSyncClients,
    sync::acquire_moodle_with_diagnostic,
    webclass_browser::{self, ProfileMode},
};

use super::{CliError, Config, config, credentials, notion_diagnosis};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Preview,
    ShowPlan,
    EnableWrites,
    DisableWrites,
    Apply,
    ApplyWithPlanHash(PlanHash),
}

impl Action {
    pub(super) fn parse(args: &[&str]) -> Result<Self, CliError> {
        match args {
            ["--dry-run"] => Ok(Self::Preview),
            ["--dry-run", "--show-plan"] | ["--show-plan", "--dry-run"] => Ok(Self::ShowPlan),
            ["--enable-writes"] => Ok(Self::EnableWrites),
            ["--disable-writes"] => Ok(Self::DisableWrites),
            ["--apply"] => Ok(Self::Apply),
            ["--apply", "--expect-plan-hash", hash] | ["--expect-plan-hash", hash, "--apply"] => {
                hash.parse()
                    .map(Self::ApplyWithPlanHash)
                    .map_err(|_| CliError::Usage)
            }
            _ => Err(CliError::Usage),
        }
    }

    fn approval_mode(self) -> Option<&'static str> {
        match self {
            Self::EnableWrites => Some("enable_writes"),
            Self::DisableWrites => Some("disable_writes"),
            Self::Apply | Self::ApplyWithPlanHash(_) | Self::Preview | Self::ShowPlan => None,
        }
    }
}

pub(super) fn execute(path: Option<&Path>, action: Action) -> u8 {
    let mode = match action {
        Action::Preview => RecurringMode::Preview,
        Action::ShowPlan => RecurringMode::PreviewWithPlan,
        Action::ApplyWithPlanHash(hash) => RecurringMode::ApplyWithPlanHash(hash),
        _ => RecurringMode::Apply,
    };
    match execute_inner(path, action, mode) {
        Ok(exit) => exit,
        Err(error) => match action.approval_mode() {
            Some(approval_mode) => {
                let (status, exit) = if error == "target_busy" {
                    ("busy", 4)
                } else {
                    ("blocked", 2)
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "schema_version": 1, "mode": approval_mode, "status": status, "error": error,
                    })
                );
                exit
            }
            None => emit_report(&RecurringReport::blocked(mode, error)),
        },
    }
}

fn emit_report(report: &RecurringReport) -> u8 {
    println!(
        "{}",
        serde_json::to_string(report).expect("static result fields")
    );
    report.exit_code()
}

fn execute_inner(
    path: Option<&Path>,
    action: Action,
    mode: RecurringMode,
) -> Result<u8, &'static str> {
    let enable = action == Action::EnableWrites;
    if enable
        && (!io::stdin().is_terminal()
            || !io::stdout().is_terminal()
            || !io::stderr().is_terminal())
    {
        return Err("interactive_terminal_required");
    }
    let local =
        Config::read(path.ok_or("configuration_required")?).map_err(|_| "configuration_invalid")?;
    if action == Action::DisableWrites {
        let (_, directory) = production_state(&local)?;
        NotionClient::revoke_production_recurring(&local.notion, &directory)?;
        println!(
            "{}",
            serde_json::json!({"schema_version": 1, "mode": "disable_writes", "status": "disabled"})
        );
        return Ok(0);
    }
    let settings = local
        .recurring_sync
        .as_ref()
        .ok_or("recurring_not_configured")?;
    let feed = match settings.moodle {
        Some(limits) => {
            let moodle = local.moodle.as_ref().ok_or("moodle_not_configured")?;
            Some((
                limits,
                [
                    moodle.host.clone(),
                    moodle.path.clone(),
                    moodle.scope.clone(),
                ],
            ))
        }
        None => None,
    };
    if settings.webclass.is_some() && !local.webclass {
        return Err("webclass_not_configured");
    }
    let (target, directory) = production_state(&local)?;
    let policy = RecurringPolicy::new(&settings.writer_bot_id, feed, settings.webclass)
        .map_err(notion_diagnosis)?;

    if enable {
        let bot = assignment_manager::notion::normalize_data_source_id(&settings.writer_bot_id)
            .ok_or("configuration_invalid")?;
        eprintln!(
            "assignments={} courses={} rules={} writer={}",
            target,
            local.notion.courses_data_source_id,
            local
                .notion
                .course_rules
                .as_ref()
                .expect("verified production target")
                .data_source_id,
            bot
        );
        for (source, limits) in [("moodle", settings.moodle), ("webclass", settings.webclass)] {
            if let Some(limits) = limits {
                eprintln!(
                    "{source}: max_creates={} max_updates={}",
                    limits.max_creates, limits.max_updates
                );
            }
        }
        eprint!("Type ENABLE RECURRING {target} to approve: ");
        io::stderr().flush().map_err(|_| "approval_missing")?;
        let mut approval = String::new();
        if io::stdin().read_line(&mut approval).is_err()
            || approval.trim_end_matches(['\r', '\n']) != format!("ENABLE RECURRING {target}")
        {
            return Err("approval_missing");
        }
        let reader = credentials::notion_client(&target).map_err(|status| status.code())?;
        let writer = credentials::production_writer(&target).map_err(|status| status.code())?;
        reader.approve_production_recurring(&writer, &local.notion, &policy, &directory)?;
        println!(
            "{}",
            serde_json::json!({"schema_version": 1, "mode": "enable_writes", "status": "approved"})
        );
        return Ok(0);
    }
    let reader = credentials::unattended(|| credentials::notion_client(&target))
        .map_err(|status| status.code())?;
    let report = reader.run_production_recurring(
        &local.notion,
        &policy,
        &directory,
        mode,
        || {
            credentials::unattended(|| credentials::production_writer(&target))
                .map_err(|status| status.code())
        },
        |scopes| RecurringInputs {
            moodle: if settings.moodle.is_some() {
                acquire_moodle(&local, &target)
            } else {
                Err("disabled")
            },
            webclass: if settings.webclass.is_some() {
                acquire_webclass(scopes)
            } else {
                Err("disabled")
            },
        },
    );
    Ok(emit_report(&report))
}

fn production_state(config: &Config) -> Result<(String, PathBuf), &'static str> {
    let target =
        live_assignment_read_target(&config.notion, true).map_err(|_| "target_mismatch")?;
    let directory = config::root()
        .map_err(|_| "state_path_unavailable")?
        .join("targets")
        .join(&target);
    Ok((target, directory))
}

fn acquire_moodle(local: &Config, target: &str) -> Result<SyncInput, &'static str> {
    let source = local.moodle.as_ref().ok_or("moodle_not_configured")?;
    if source.windows_crl.is_some() {
        return Err("moodle_policy_invalid");
    }
    let policy = source.policy().map_err(|_| "moodle_policy_invalid")?;
    let url = credentials::unattended(|| credentials::moodle_url(target))
        .map_err(|error| error.code())?;
    policy
        .validate_moodle_url(&url)
        .map_err(|_| "invalid_feed")?;
    let feed = MoodleFeed::new(&source.scope, MoodleExportKind::CourseRelated, &url)
        .map_err(|_| "invalid_feed")?;
    let mut clients = SecuredSyncClients::new(policy);
    clients.begin_run();
    let client = clients.moodle().map_err(|_| "moodle_client_unavailable")?;
    let acquired =
        acquire_moodle_with_diagnostic(&client, &feed).map_err(|error| error.diagnostic.code())?;
    SyncInput::from_acquisitions(Some(&acquired), None).map_err(|_| "invalid_source_data")
}

fn acquire_webclass(scopes: &BTreeMap<String, String>) -> Result<SyncInput, &'static str> {
    if scopes.is_empty() {
        return Ok(SyncInput::default());
    }
    webclass_browser::check_local_dependencies()?;
    let acquired =
        webclass_browser::run_for_confirmed_courses(ProfileMode::Headless, scopes).result?;
    let groups: HashSet<_> = acquired
        .attempts()
        .last()
        .ok_or("webclass_incomplete")?
        .groups()
        .iter()
        .filter_map(|group| match group.observation().scope() {
            SyncScope::WebClassContents(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    if groups.len() != scopes.len() || !scopes.keys().all(|key| groups.contains(key.as_str())) {
        return Err("webclass_incomplete");
    }
    SyncInput::from_acquisitions(None, Some(&acquired)).map_err(|_| "invalid_source_data")
}
