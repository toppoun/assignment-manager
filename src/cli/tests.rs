use super::*;
use serde_json::{Value, json};

fn parse(words: &[&str]) -> Result<Arguments, CliError> {
    Arguments::parse(&words.iter().map(|word| (*word).into()).collect::<Vec<_>>())
}

fn config_json() -> Value {
    json!({
        "notion": {
            "assignments_data_source_id":"00000000-0000-4000-8000-000000000001",
            "courses_data_source_id":"00000000-0000-4000-8000-000000000002",
            "properties":{"title":"title","deadline":"date","course":"course","submission_url":"url","sync_key":"sync","source":"source"}
        },
        "moodle":{"host":"moodle.example.invalid","path":"/calendar/export","scope":"default.course-related"},
        "webclass": true
    })
}

pub(super) fn config() -> Config {
    Config::parse(config_json().to_string().as_bytes()).unwrap()
}

#[test]
fn parses_only_explicit_dry_run_and_named_credential_commands() {
    assert_eq!(
        parse(&["sync", "--dry-run", "--fixture"]).unwrap().command,
        Command::Sync(true)
    );
    assert_eq!(
        parse(&["--config", "local.json", "credentials", "set", "webclass"])
            .unwrap()
            .command,
        Command::Register(Kind::WebClass)
    );
    for words in [
        &["sync"][..],
        &["sync", "--apply"],
        &["sync", "--fixture"],
        &["--config", "some.json", "sync", "--dry-run", "--fixture"],
        &["credentials", "set", "notion", "secret"],
        &["--token", "secret"],
        &["notion-check"],
        &["sync", "--dry-run", "--allow-notion-read"],
    ] {
        assert_eq!(parse(words), Err(CliError::Usage));
    }
}

#[test]
fn accepts_only_explicit_notion_read_and_a_legacy_config() {
    assert_eq!(
        parse(&["notion-check", "--allow-notion-read"])
            .unwrap()
            .command,
        Command::NotionCheck
    );
    assert_eq!(
        parse(&[
            "--config",
            "local.json",
            "sync",
            "--dry-run",
            "--fixture",
            "--allow-notion-read"
        ])
        .unwrap()
        .command,
        Command::NotionFixturePreview
    );
    let mut data = config_json();
    let config = data.as_object_mut().unwrap();
    config.remove("moodle");
    config.remove("webclass");
    let fields = data["notion"]["properties"].as_object_mut().unwrap();
    fields.remove("sync_key");
    fields.remove("source");
    let config = Config::parse(data.to_string().as_bytes()).unwrap();
    assert!(config.notion.properties.sync_key.is_empty());
    assert!(config.notion.properties.source.is_empty());
    assert_eq!(
        notion_diagnosis(NotionError::NotFound),
        "not_found_or_not_shared"
    );
    assert_eq!(notion_diagnosis(NotionError::Forbidden), "forbidden");
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn complete_cli_dry_run_uses_mock_and_leaves_state_unchanged() {
    let mut config = config();
    config.notion = assignment_manager::notion::fixture::target();
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        target: config.target_id().unwrap(),
        directory: root.path().into(),
    };
    let _lock = TargetLock::acquire(&paths.directory).unwrap();
    StateStore::initialize(&paths.state(), &paths.target).unwrap();
    let key = "v1:b7e92444ae37a18bb36c597fd2e6bd2ba0999bff4ece1984c465aba44cb977aa"
        .parse()
        .unwrap();
    let state = StateStore::for_apply(&paths.state(), &paths.target).unwrap();
    assert!(state.reserve_create(&key).unwrap());
    state.mark_unknown(&key).unwrap();
    drop(state);
    let original = std::fs::read(paths.state()).unwrap();
    let (mock, _server) = assignment_manager::notion::fixture::start().unwrap();
    assert_eq!(preview_with_client(&mock, &config, &paths).unwrap(), 3);
    let report = Receipt::read(&paths.receipt()).unwrap().unwrap();
    assert_eq!(report.issue.unwrap().code(), "deferred");
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.counts.create, 9);
    assert_eq!(report.counts.update, 0);
    assert_eq!(std::fs::read(paths.state()).unwrap(), original);
}

#[test]
fn validates_configuration_before_opening_state_or_reading_credentials() {
    assert_eq!(
        config().target_id().unwrap(),
        "00000000-0000-4000-8000-000000000001"
    );
    for (key, value) in [
        ("token", json!("synthetic-private-marker")),
        (
            "moodle",
            json!({"host":"example.invalid","path":"/calendar?token=secret","scope":"default.course-related"}),
        ),
        (
            "course_mappings",
            json!([{"course_key":{"origin":"Manual","scope":"default","id":"a"},"page_id":"bad"}]),
        ),
    ] {
        let mut config = config_json();
        config[key] = value;
        assert!(matches!(
            Config::parse(config.to_string().as_bytes()),
            Err(ConfigError::Invalid)
        ));
    }
}

#[test]
fn diagnoses_unregistered_entries_and_skips_disabled_sources() {
    let mut config = config();
    config.moodle = None;
    let statuses = credentials::diagnose(&config, |kind| {
        assert_ne!(kind, Kind::Moodle);
        CredentialStatus::Missing
    });
    assert_eq!(
        statuses,
        [
            CredentialStatus::Disabled,
            CredentialStatus::Missing,
            CredentialStatus::Missing
        ]
    );
    assert_eq!(
        CliError::Credential(statuses[2]).code(),
        "credential_missing"
    );
}
