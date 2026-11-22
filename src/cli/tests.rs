use super::*;
use serde_json::{Value, json};

fn parse(words: &[&str]) -> Result<Arguments, CliError> {
    Arguments::parse(&words.iter().map(|word| (*word).into()).collect::<Vec<_>>())
}

#[cfg(windows)]
#[test]
fn integration_cli_requires_explicit_dual_read_permissions_and_network_route() {
    let base = [
        "--config",
        "live-notion.json",
        "sync",
        "moodle",
        "--dry-run",
        "--moodle-config",
        "moodle.json",
    ];
    for (route, vpn) in [("--require-windows-vpn", true), ("--on-campus", false)] {
        let args = parse(
            &[
                base.as_slice(),
                &["--allow-university-read", route, "--allow-notion-read"],
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            args.command,
            Command::MoodleIntegration {
                moodle_config: PathBuf::from("moodle.json"),
                require_windows_vpn: vpn
            }
        );
    }
    for tail in [
        &["--require-windows-vpn", "--allow-notion-read"][..],
        &["--allow-university-read", "--allow-notion-read"],
        &["--allow-university-read", "--require-windows-vpn"],
        &[
            "--allow-university-read",
            "--require-windows-vpn",
            "--allow-notion-write",
        ],
    ] {
        assert_eq!(
            parse(&[base.as_slice(), tail].concat()),
            Err(CliError::Usage)
        );
    }
}

#[cfg(windows)]
#[test]
fn live_moodle_cli_separates_local_init_explicit_single_create_and_read_only_recovery() {
    let prefix = ["--config", "live.json", "sync", "moodle"];
    assert_eq!(
        parse(&[prefix.as_slice(), &["--init-state"]].concat())
            .unwrap()
            .command,
        Command::MoodleLiveInit,
    );
    let flags = [
        "--moodle-config",
        "moodle.json",
        "--allow-university-read",
        "--require-windows-vpn",
        "--allow-notion-read",
    ];
    let create = [prefix.as_slice(), &["--create-one"], flags.as_slice()].concat();
    assert_eq!(parse(&create), Err(CliError::Usage));
    let mut approved = create;
    approved.push("--allow-notion-create");
    assert_eq!(
        parse(&approved).unwrap().command,
        Command::MoodleLive {
            moodle_config: PathBuf::from("moodle.json"),
            require_windows_vpn: true,
            action: moodle_live::Action::Create,
        },
    );
    approved.extend(["--uid", "synthetic@example.invalid"]);
    assert_eq!(parse(&approved), Err(CliError::Usage));
    let mut recover = [prefix.as_slice(), &["--reconcile-one"], flags.as_slice()].concat();
    assert!(matches!(
        parse(&recover).unwrap().command,
        Command::MoodleLive {
            action: moodle_live::Action::Reconcile,
            ..
        }
    ));
    recover.push("--allow-notion-create");
    assert_eq!(parse(&recover), Err(CliError::Usage));
}

#[cfg(all(windows, feature = "synthetic-https-fixture"))]
#[test]
fn moodle_create_trial_cli_requires_a_single_uid_and_has_no_bulk_mode() {
    let base = [
        "--config",
        "test.json",
        "notion-moodle-trial",
        "--writer",
        "bot",
    ];
    assert_eq!(
        parse(&[base.as_slice(), &["--allow-notion-write"]].concat()),
        Err(CliError::Usage)
    );
    assert_eq!(
        parse(
            &[
                base.as_slice(),
                &["--allow-notion-write", "--uid", "a", "--uid", "b"]
            ]
            .concat()
        ),
        Err(CliError::Usage)
    );
    let selected =
        parse(&[base.as_slice(), &["--allow-notion-write", "--uid", "a"]].concat()).unwrap();
    assert_eq!(
        selected.command,
        Command::MoodleCreateTrial {
            writer: "bot".into(),
            action: moodle_trial::Action::Apply("a".into())
        }
    );
}

#[cfg(all(windows, feature = "synthetic-https-fixture"))]
#[test]
fn update_trial_accepts_only_the_dedicated_uid_and_separate_commands() {
    let base = [
        "--config",
        "test.json",
        "notion-moodle-trial",
        "--writer",
        "bot",
    ];
    for (flag, action) in [
        ("--dry-run-update", moodle_trial::Action::PreviewUpdate),
        ("--allow-notion-update", moodle_trial::Action::ApplyUpdate),
    ] {
        let parsed = parse(
            &[
                base.as_slice(),
                &[flag, "--uid", "review-submit@example.invalid"],
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            parsed.command,
            Command::MoodleCreateTrial {
                writer: "bot".into(),
                action
            }
        );
        assert_eq!(
            parse(
                &[
                    base.as_slice(),
                    &[flag, "--uid", "review-close@example.invalid"]
                ]
                .concat()
            ),
            Err(CliError::Usage)
        );
    }
    assert_eq!(
        parse(&[base.as_slice(), &["--allow-notion-update"]].concat()),
        Err(CliError::Usage)
    );
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
fn moodle_preview_requires_explicit_fixture_and_complete_notion_selection() {
    assert_eq!(
        parse(&["sync", "moodle", "--dry-run", "--fixture"])
            .unwrap()
            .command,
        Command::MoodleFixturePlan
    );
    let real = [
        "--config",
        "verification.json",
        "sync",
        "moodle",
        "--dry-run",
        "--fixture",
        "--allow-notion-read",
        "--target",
        "assignments",
        "--courses",
        "courses",
        "--reader",
        "bot",
    ];
    assert!(matches!(
        parse(&real).unwrap().command,
        Command::MoodleNotionPlan { .. }
    ));
    for invalid in [
        &real[2..],
        &real[..real.len() - 2],
        &["sync", "moodle", "--dry-run"],
        &["sync", "moodle", "--fixture", "--allow-university-read"],
        &[
            "sync",
            "moodle",
            "--dry-run",
            "--fixture",
            "--allow-notion-write",
        ],
    ] {
        assert_eq!(parse(invalid), Err(CliError::Usage));
    }
    #[cfg(not(feature = "synthetic-https-fixture"))]
    assert_eq!(
        execute(parse(&real).unwrap()),
        Err(CliError::FixtureUnavailable)
    );
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

#[test]
fn reads_dry_run_receipts_written_before_course_relation_diagnostics() {
    let receipt: Receipt = serde_json::from_value(json!({
        "finished_at":"2026-11-20T00:00:00Z",
        "moodle":"success",
        "webclass":"success",
        "counts":{"create":0,"update":1,"unchanged":2,"conflict":0,"deferred":0},
        "outcome":"success",
        "issue":null
    }))
    .unwrap();
    assert_eq!(receipt.counts.update, 1);
    assert_eq!(receipt.counts.course_relation_mismatch, 0);
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn complete_cli_dry_run_uses_mock_and_leaves_state_unchanged() {
    let mut config = config();
    config.notion = assignment_manager::notion::fixture::target();
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().to_path_buf();
    #[cfg(target_os = "macos")]
    let directory = directory.canonicalize().unwrap();
    let paths = Paths {
        target: config.target_id().unwrap(),
        directory,
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
