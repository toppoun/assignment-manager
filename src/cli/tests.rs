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
    ] {
        assert_eq!(parse(words), Err(CliError::Usage));
    }
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
