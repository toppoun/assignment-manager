mod config;
mod credentials;
#[cfg(feature = "synthetic-https-fixture")]
mod fixture_run;
#[cfg(windows)]
mod moodle_review;
#[cfg(test)]
mod tests;
mod trial;
mod university;

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

use assignment_manager::notion::{
    DryRunCounts, InspectionReport, NotionError,
    state::{RunOutcome, StateError, StateStore, TargetLock, check_path},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use config::{Config, ConfigError};
use credentials::{CredentialStatus, Kind};

const USAGE: &str = "assignment-manager [--config PATH] <doctor | init-state | status | credentials set KIND | notion-check --allow-notion-read | sync --dry-run | university-check moodle --allow-university-read [--require-windows-vpn [--review-local]]>\nKIND: moodle, webclass, notion\nmacOS: notion-trial --target ID [--dry-run | --allow-notion-write | init-state | status | credentials set read|write]\nFixture build: university-check --fixture [--review-local], sync --dry-run --fixture [--allow-notion-read], init-state --fixture, status --fixture";
#[cfg(feature = "synthetic-https-fixture")]
const APPLY_USAGE: &str = "Fixture build: fixture-apply (local synthetic HTTP only)";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Doctor,
    InitState(bool),
    Status(bool),
    Register(Kind),
    Sync(bool),
    NotionCheck,
    NotionFixturePreview,
    FixtureApply,
    UniversityMoodle {
        require_windows_vpn: bool,
        review_local: bool,
    },
    UniversityFixture,
    UniversityReviewFixture,
    NotionTrial(String, trial::Command),
}

impl Command {
    fn uses_fixture(&self) -> bool {
        matches!(
            self,
            Self::InitState(true) | Self::Status(true) | Self::Sync(true) | Self::FixtureApply
        )
    }

    fn needs_fixture_build(&self) -> bool {
        self.uses_fixture()
            || matches!(
                self,
                Self::NotionFixturePreview
                    | Self::UniversityFixture
                    | Self::UniversityReviewFixture
            )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    command: Command,
    config: Option<PathBuf>,
}

impl Arguments {
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let (config, rest) = match args {
            [flag, path, rest @ ..]
                if flag == "--config" && !path.is_empty() && !path.starts_with("--") =>
            {
                (Some(PathBuf::from(path)), rest)
            }
            _ => (None, args),
        };
        let words: Vec<_> = rest.iter().map(String::as_str).collect();
        let command = match words.as_slice() {
            [] | ["--help" | "-h" | "help"] => Command::Help,
            ["doctor"] => Command::Doctor,
            ["init-state"] => Command::InitState(false),
            ["init-state", "--fixture"] => Command::InitState(true),
            ["status"] => Command::Status(false),
            ["status", "--fixture"] => Command::Status(true),
            ["sync", "--dry-run"] => Command::Sync(false),
            ["sync", "--dry-run", "--fixture"] => Command::Sync(true),
            ["fixture-apply"] => Command::FixtureApply,
            [
                "university-check",
                "moodle",
                "--allow-university-read",
                rest @ ..,
            ] if matches!(
                rest,
                [] | ["--require-windows-vpn"] | ["--require-windows-vpn", "--review-local"]
            ) =>
            {
                Command::UniversityMoodle {
                    require_windows_vpn: !rest.is_empty(),
                    review_local: rest.len() == 2,
                }
            }
            ["university-check", "--fixture"] => Command::UniversityFixture,
            ["university-check", "--fixture", "--review-local"] => Command::UniversityReviewFixture,
            ["sync", "--dry-run", "--fixture", "--allow-notion-read"] => {
                Command::NotionFixturePreview
            }
            ["notion-check", "--allow-notion-read"] => Command::NotionCheck,
            ["notion-trial", "--target", id, rest @ ..] => {
                Command::NotionTrial((*id).into(), trial::Command::parse(rest)?)
            }
            ["credentials", "set", kind] => {
                Command::Register(Kind::parse(kind).ok_or(CliError::Usage)?)
            }
            _ => return Err(CliError::Usage),
        };
        if config.is_some()
            && (command.uses_fixture()
                || matches!(
                    command,
                    Command::UniversityFixture | Command::UniversityReviewFixture
                ))
        {
            return Err(CliError::Usage);
        }
        Ok(Self { command, config })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CliError {
    Usage,
    Config(ConfigError),
    State(StateError),
    Credential(CredentialStatus),
    FixtureUnavailable,
    NetworkDisabled,
    #[cfg(not(target_os = "macos"))]
    TrialUnavailable,
    #[cfg(target_os = "macos")]
    TrialTargetRejected,
    #[cfg(target_os = "macos")]
    TrialRequiresTerminal,
    #[cfg(feature = "synthetic-https-fixture")]
    FixtureFailed,
    Receipt,
}

impl CliError {
    fn code(&self) -> &'static str {
        match self {
            Self::Usage => "invalid_arguments",
            Self::Config(ConfigError::Missing) => "configuration_missing",
            Self::Config(ConfigError::Invalid) => "configuration_invalid",
            Self::Config(ConfigError::Unreadable) => "configuration_unreadable",
            Self::State(StateError::Busy) => "target_busy",
            Self::State(StateError::InvalidDatabase) => "state_invalid",
            Self::State(_) => "state_unavailable",
            Self::Credential(CredentialStatus::Disabled) => "source_disabled",
            Self::Credential(CredentialStatus::Missing) => "credential_missing",
            Self::Credential(CredentialStatus::Invalid) => "credential_invalid",
            Self::Credential(CredentialStatus::Cancelled) => "credential_cancelled",
            Self::Credential(_) => "credential_unavailable",
            Self::FixtureUnavailable => "fixture_feature_required",
            Self::NetworkDisabled => "network_disabled",
            #[cfg(not(target_os = "macos"))]
            Self::TrialUnavailable => "trial_requires_macos",
            #[cfg(target_os = "macos")]
            Self::TrialTargetRejected => "trial_target_rejected",
            #[cfg(target_os = "macos")]
            Self::TrialRequiresTerminal => "trial_requires_terminal",
            #[cfg(feature = "synthetic-https-fixture")]
            Self::FixtureFailed => "fixture_failed",
            Self::Receipt => "receipt_unavailable",
        }
    }

    fn exit(&self) -> u8 {
        match self {
            Self::Usage
            | Self::Config(_)
            | Self::Credential(_)
            | Self::FixtureUnavailable
            | Self::NetworkDisabled => 2,
            #[cfg(not(target_os = "macos"))]
            Self::TrialUnavailable => 2,
            #[cfg(target_os = "macos")]
            Self::TrialTargetRejected | Self::TrialRequiresTerminal => 2,
            Self::State(StateError::Busy) => 4,
            _ => 1,
        }
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<StateError> for CliError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AcquisitionStatus {
    Success,
    Partial,
    Failed,
}

impl AcquisitionStatus {
    fn code(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DryRunIssue {
    Acquisition,
    SchemaMismatch,
    NotionRead,
    GetOnlySnapshotUnavailable,
    Conflict,
    Deferred,
}

impl DryRunIssue {
    fn code(self) -> &'static str {
        match self {
            Self::Acquisition => "acquisition",
            Self::SchemaMismatch => "schema_mismatch",
            Self::NotionRead => "notion_read",
            Self::GetOnlySnapshotUnavailable => "snapshot_unavailable_get_only",
            Self::Conflict => "conflict",
            Self::Deferred => "deferred",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    finished_at: DateTime<Utc>,
    moodle: AcquisitionStatus,
    webclass: AcquisitionStatus,
    counts: DryRunCounts,
    outcome: RunOutcome,
    issue: Option<DryRunIssue>,
}

impl Receipt {
    #[cfg(feature = "synthetic-https-fixture")]
    fn new(
        moodle: AcquisitionStatus,
        webclass: AcquisitionStatus,
        counts: DryRunCounts,
        error: Option<NotionError>,
    ) -> Self {
        use AcquisitionStatus::{Failed, Success};
        let (outcome, issue) = match error {
            Some(NotionError::SchemaMismatch) => {
                (RunOutcome::Failed, Some(DryRunIssue::SchemaMismatch))
            }
            Some(_) => (RunOutcome::Failed, Some(DryRunIssue::NotionRead)),
            None => match (moodle, webclass) {
                (Failed, Failed) => (RunOutcome::Failed, Some(DryRunIssue::Acquisition)),
                (Success, Success) if counts.conflict != 0 => {
                    (RunOutcome::Partial, Some(DryRunIssue::Conflict))
                }
                (Success, Success) if counts.deferred != 0 => {
                    (RunOutcome::Partial, Some(DryRunIssue::Deferred))
                }
                (Success, Success) => (RunOutcome::Success, None),
                _ => (RunOutcome::Partial, Some(DryRunIssue::Acquisition)),
            },
        };
        Self {
            finished_at: Utc::now(),
            moodle,
            webclass,
            counts,
            outcome,
            issue,
        }
    }

    fn display(&self) {
        println!("moodle={}", self.moodle.code());
        println!("webclass={}", self.webclass.code());
        let c = self.counts;
        println!(
            "Create={} Update={} Unchanged={} Conflict={} Deferred={}",
            c.create, c.update, c.unchanged, c.conflict, c.deferred
        );
        println!("course_relation_mismatch={}", c.course_relation_mismatch);
        println!(
            "result={} issue={}",
            self.outcome.code(),
            self.issue.map_or("none", DryRunIssue::code)
        );
    }

    fn read(path: &Path) -> Result<Option<Self>, CliError> {
        check_path(path)?;
        let file = match File::open(path) {
            Ok(file) if file.metadata().is_ok_and(|meta| meta.is_file()) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            _ => return Err(CliError::Receipt),
        };
        let mut data = Vec::new();
        file.take(4_097)
            .read_to_end(&mut data)
            .map_err(|_| CliError::Receipt)?;
        if data.len() > 4_096 {
            return Err(CliError::Receipt);
        }
        serde_json::from_slice(&data)
            .map(Some)
            .map_err(|_| CliError::Receipt)
    }

    #[cfg(feature = "synthetic-https-fixture")]
    fn write(&self, path: &Path) -> Result<(), CliError> {
        check_path(path)?;
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or(CliError::Receipt)?)
            .map_err(|_| CliError::Receipt)?;
        serde_json::to_writer(temp.as_file_mut(), self).map_err(|_| CliError::Receipt)?;
        temp.as_file().sync_all().map_err(|_| CliError::Receipt)?;
        temp.persist(path).map_err(|_| CliError::Receipt)?;
        Ok(())
    }
}

struct Paths {
    target: String,
    directory: PathBuf,
}

impl Paths {
    fn state(&self) -> PathBuf {
        self.directory.join("state.sqlite3")
    }
    fn receipt(&self) -> PathBuf {
        self.directory.join("last-dry-run.json")
    }
}

fn initialize(paths: &Paths) -> Result<u8, CliError> {
    let _lock = TargetLock::acquire(&paths.directory)?;
    StateStore::initialize(&paths.state(), &paths.target)?;
    println!("state=ready");
    Ok(0)
}

fn status(paths: &Paths) -> Result<u8, CliError> {
    match StateStore::read(&paths.state(), &paths.target)? {
        Some(state) => {
            println!("state=ready pending={}", state.pending);
            let time = |value: Option<DateTime<Utc>>| {
                value.map_or_else(|| "none".into(), |v| v.to_rfc3339())
            };
            println!(
                "last_fetch={} last_apply={}",
                time(state.last_fetch),
                time(state.last_apply)
            );
            if let Some(run) = state.last_run {
                println!(
                    "last_state_run={} at={} deferred={} issue={}",
                    run.outcome.code(),
                    run.finished_at.to_rfc3339(),
                    run.deferred,
                    run.issue.map_or("none", |issue| issue.code())
                );
            }
        }
        None => println!("state=missing pending=0"),
    }
    if let Some(receipt) = Receipt::read(&paths.receipt())? {
        println!("last_dry_run={}", receipt.finished_at.to_rfc3339());
        receipt.display();
    } else {
        println!("last_dry_run=none");
    }
    Ok(0)
}

fn check_credentials(config: &Config, target: &str) -> Result<(), CliError> {
    let values = credentials::diagnose(config, |kind| credentials::lookup(config, target, kind));
    for (kind, status) in Kind::ALL.into_iter().zip(values) {
        println!("{}={}", kind.code(), status.code());
    }
    #[cfg(target_os = "macos")]
    if values.contains(&CredentialStatus::Unavailable) {
        return Err(CliError::Credential(CredentialStatus::Unavailable));
    }
    values
        .into_iter()
        .find(|status| !status.ok())
        .map_or(Ok(()), |failed| Err(CliError::Credential(failed)))
}

fn notion_diagnosis(error: NotionError) -> &'static str {
    match error {
        NotionError::Configuration => "configuration_invalid",
        NotionError::Authentication => "authentication_failed",
        NotionError::CredentialMismatch => "credential_mismatch",
        NotionError::Forbidden => "forbidden",
        NotionError::NotFound => "not_found_or_not_shared",
        NotionError::SchemaMismatch => "schema_mismatch",
        NotionError::Timeout => "timeout",
        NotionError::RateLimited => "rate_limited",
        NotionError::Overloaded => "overloaded",
        NotionError::Transport => "transport_failed",
        NotionError::IncompleteSnapshot => "incomplete_read",
        NotionError::MethodDisabled => "request_not_allowed",
        NotionError::Server(_) => "server_error",
        NotionError::Rejected(_) | NotionError::Conflict => "read_rejected",
        NotionError::InvalidResponse
        | NotionError::UnsupportedProperty
        | NotionError::InvalidWrite => "invalid_response",
    }
}

fn display_inspection(inspection: &InspectionReport) {
    println!("{inspection}");
    if let Some(error) = inspection.error {
        println!("notion_error={}", notion_diagnosis(error));
    }
}

fn check_notion(config: &Config, paths: &Paths) -> Result<u8, CliError> {
    let _lock = TargetLock::acquire(&paths.directory)?;
    let client = credentials::notion_client(&paths.target).map_err(CliError::Credential)?;
    let inspection = client.inspect(
        &config.notion,
        &config.sample_assignment_page_ids,
        &config.course_mappings,
    );
    display_inspection(&inspection);
    match (inspection.error, inspection.snapshot) {
        (None, Some(snapshot)) if snapshot.duplicate_keys != 0 => {
            println!("result=partial issue=conflict");
            Ok(3)
        }
        (None, Some(_)) => {
            println!("result=success scope=complete_snapshot");
            Ok(0)
        }
        _ => {
            println!("result=failed");
            Ok(1)
        }
    }
}

#[cfg(feature = "synthetic-https-fixture")]
fn preview_notion(config: &Config, paths: &Paths) -> Result<u8, CliError> {
    let _lock = TargetLock::acquire(&paths.directory)?;
    let client = credentials::notion_client(&paths.target).map_err(CliError::Credential)?;
    preview_with_client(&client, config, paths)
}

#[cfg(feature = "synthetic-https-fixture")]
fn preview_with_client(
    client: &assignment_manager::notion::NotionClient,
    config: &Config,
    paths: &Paths,
) -> Result<u8, CliError> {
    let state = StateStore::read(&paths.state(), &paths.target)?;
    let acquired =
        fixture_run::acquire(assignment_manager::webclass_auth::synthetic::Scenario::BothSuccess)?;
    let (inspection, mut plan) = client.inspect_dry_run(
        &acquired.input,
        &config.notion,
        &config.sample_assignment_page_ids,
        &config.course_mappings,
    );
    display_inspection(&inspection);
    let unconfigured = acquired
        .input
        .assignments()
        .values()
        .filter_map(|assignment| assignment.course_key.as_ref())
        .filter(|key| {
            !config
                .course_mappings
                .iter()
                .any(|mapping| &mapping.course_key == *key)
        })
        .count();
    println!("course_mapping_unconfigured={unconfigured}");
    if let Some(state) = &state {
        plan.defer_unreconciled(state);
    }
    let report = acquired.receipt(&plan);
    report.write(&paths.receipt())?;
    report.display();
    Ok(match report.outcome {
        RunOutcome::Success => 0,
        RunOutcome::Partial => 3,
        RunOutcome::Failed => 1,
    })
}

#[cfg(feature = "synthetic-https-fixture")]
fn run_fixture(command: Command, root: PathBuf) -> Result<u8, CliError> {
    if command == Command::FixtureApply {
        return fixture_run::apply(&root.join("fixture-apply"));
    }
    let target = assignment_manager::notion::fixture::target()
        .validated_id()
        .map_err(|_| CliError::FixtureFailed)?;
    let paths = Paths {
        target,
        directory: root.join("fixture"),
    };
    match command {
        Command::InitState(true) => initialize(&paths),
        Command::Status(true) => status(&paths),
        Command::Sync(true) => {
            let _lock = TargetLock::acquire(&paths.directory)?;
            let state = StateStore::read(&paths.state(), &paths.target)?;
            let (inspection, report) = fixture_run::dry_run(state.as_ref())?;
            display_inspection(&inspection);
            report.display();
            report.write(&paths.receipt())?;
            Ok(match report.outcome {
                RunOutcome::Success => 0,
                RunOutcome::Partial => 3,
                RunOutcome::Failed => 1,
            })
        }
        _ => Err(CliError::Usage),
    }
}

#[cfg(not(feature = "synthetic-https-fixture"))]
fn run_fixture(_: Command, _: PathBuf) -> Result<u8, CliError> {
    Err(CliError::FixtureUnavailable)
}

fn execute(args: Arguments) -> Result<u8, CliError> {
    if args.command == Command::Help {
        println!("{USAGE}");
        #[cfg(feature = "synthetic-https-fixture")]
        println!("{APPLY_USAGE}");
        return Ok(0);
    }
    // Fail before accessing local state when this is an ordinary binary.
    if args.command.needs_fixture_build() && !cfg!(feature = "synthetic-https-fixture") {
        return Err(CliError::FixtureUnavailable);
    }
    if let Command::NotionTrial(id, command) = args.command {
        return trial::execute(&id, command, args.config);
    }
    #[cfg(feature = "synthetic-https-fixture")]
    if args.command == Command::UniversityFixture {
        use assignment_manager::webclass_auth::synthetic::Scenario;
        return Ok(university::fixture(Scenario::BothSuccess));
    }
    #[cfg(feature = "synthetic-https-fixture")]
    if args.command == Command::UniversityReviewFixture {
        return Ok(university::review_fixture());
    }
    if let Command::UniversityMoodle {
        require_windows_vpn,
        review_local,
    } = args.command
    {
        let path = match args.config {
            Some(path) => path,
            None => config::root()?.join("config.json"),
        };
        let directory = path.parent().unwrap_or(Path::new("."));
        return university::moodle(
            &Config::read(&path)?,
            directory,
            require_windows_vpn,
            review_local,
        );
    }
    let root = config::root()?;
    if args.command.uses_fixture() {
        return run_fixture(args.command, root);
    }
    let config = Config::read(&args.config.unwrap_or_else(|| root.join("config.json")))?;
    let target = config.target_id()?;
    let directory = root.join("targets").join(&target);
    let paths = Paths { target, directory };
    match args.command {
        Command::Doctor => {
            println!("configuration=ready");
            // Run both local diagnostics even if one reports a problem.
            let credential = check_credentials(&config, &paths.target);
            let database = StateStore::read(&paths.state(), &paths.target);
            match &database {
                Ok(Some(state)) => println!("state=ready pending={}", state.pending),
                Ok(None) => println!("state=missing pending=0"),
                Err(_) => println!("state=invalid"),
            }
            println!("network=disabled");
            #[cfg(target_os = "macos")]
            if let Err(error @ CliError::Credential(CredentialStatus::Unavailable)) = credential {
                return Err(error);
            }
            database?;
            credential?;
            Ok(0)
        }
        Command::InitState(false) => initialize(&paths),
        Command::Status(false) => status(&paths),
        Command::NotionCheck => check_notion(&config, &paths),
        #[cfg(feature = "synthetic-https-fixture")]
        Command::NotionFixturePreview => preview_notion(&config, &paths),
        Command::Register(kind) => {
            if !kind.enabled(&config) {
                return Err(CliError::Credential(CredentialStatus::Disabled));
            }
            let _lock = TargetLock::acquire(&paths.directory)?;
            credentials::register(&config, &paths.target, kind).map_err(CliError::Credential)?;
            println!("{}=stored", kind.code());
            Ok(0)
        }
        Command::Sync(false) => {
            let _lock = TargetLock::acquire(&paths.directory)?;
            check_credentials(&config, &paths.target)?;
            Err(CliError::NetworkDisabled)
        }
        _ => Err(CliError::Usage),
    }
}

pub fn main() -> ExitCode {
    let result = std::env::args_os()
        .skip(1)
        .map(|word| word.into_string().map_err(|_| CliError::Usage))
        .collect::<Result<Vec<_>, _>>()
        .and_then(|words| Arguments::parse(&words))
        .and_then(execute);
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("result=failed error={}", error.code());
            if error == CliError::Usage {
                eprintln!("{USAGE}");
            }
            ExitCode::from(error.exit())
        }
    }
}
