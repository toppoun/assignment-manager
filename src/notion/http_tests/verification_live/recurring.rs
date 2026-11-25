//! The ignored runner accepts only a private temp directory and one synthetic key.
use std::{
    io::{self, IsTerminal, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use security_framework::os::macos::keychain::SecKeychain;

use super::{MANUAL_NOTE, MANUAL_URL, Snapshot, check_manual, read_snapshot, synthetic_input};
use crate::notion::{
    NotionClient, NotionTarget, RecurringMode, RecurringReport, SyncInput,
    apply::recurring::RecurringVerification,
    state::StateStore,
    trial::{
        MOODLE_VERIFICATION_ASSIGNMENTS as ASSIGNMENTS, MOODLE_VERIFICATION_READER,
        MOODLE_VERIFICATION_WRITER,
    },
};

use super::{CredentialStoreApi, Store, Value, Zeroizing, env, fs, json};

#[test]
#[ignore = "opt-in run against the fixed verification Notion database, with one synthetic page"]
fn verification_live_recurring() {
    let phase =
        env::var("ASSIGNMENT_MANAGER_RECURRING_PHASE").expect("explicit verification phase");
    let id = env::var("ASSIGNMENT_MANAGER_RECURRING_ID").expect("explicit synthetic run ID");
    let directory = verification_directory();
    let config: Value =
        serde_json::from_slice(&fs::read("local/notion-verification.json").unwrap()).unwrap();
    let target: NotionTarget = serde_json::from_value(config["notion"].clone()).unwrap();
    let max_updates = usize::from(phase != "policy-changed");
    let run = RecurringVerification::new(&target, &directory, &id, max_updates)
        .expect("fixed verification triplet");
    assert!(
        crate::notion::live_assignment_read_target(&target, true).is_err(),
        "production rejects verification configuration"
    );

    let (result, code) = match phase.as_str() {
        "preflight" => match preflight(&target, &directory, &id, &run) {
            Ok(()) => (event(&phase, "ready"), 0),
            Err(error) => blocked(&phase, error),
        },
        "enable" => match enable(&directory, &id, &run) {
            Ok(()) => (event(&phase, "approved"), 0),
            Err(error) => blocked(&phase, error),
        },
        "disable" => {
            assert_run_id(&directory, &id);
            match run.disable() {
                Ok(()) => {
                    assert!(state(&directory).recurring_approval().unwrap().is_none());
                    (event(&phase, "disabled"), 0)
                }
                Err(error) => blocked(&phase, error),
            }
        }
        "seed" => {
            assert_run_id(&directory, &id);
            match writer(false).and_then(|writer| run.seed_manual(&writer, MANUAL_NOTE, MANUAL_URL))
            {
                Ok(()) => (event(&phase, "seeded"), 0),
                Err(error) => blocked(&phase, error),
            }
        }
        "unapproved" | "create" | "update-partial" | "repeat" | "policy-changed"
        | "disabled-apply" => {
            assert_run_id(&directory, &id);
            apply_phase(&phase, &id, &target, &directory, &run)
        }
        _ => panic!("unknown verification phase"),
    };
    println!("{result}");
    io::stdout().flush().unwrap();
    // Use the actual recurring exit code for subprocess validation.
    std::process::exit(code.into());
}

fn verification_directory() -> PathBuf {
    let path = PathBuf::from(
        env::var("ASSIGNMENT_MANAGER_RECURRING_STATE").expect("isolated state directory"),
    );
    assert_eq!(path.canonicalize().unwrap(), path);
    assert_eq!(path.parent(), Some(Path::new("/private/tmp")));
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("assignment-manager-recurring.")
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    path
}

fn reader(interactive: bool) -> Result<NotionClient, &'static str> {
    isolated_credential(
        &format!("io.assignment-manager.notion-cli.v1.{ASSIGNMENTS}"),
        "notion",
        interactive,
    )
    .map_err(|_| "verification_reader_keychain_unavailable")
}

fn writer(interactive: bool) -> Result<NotionClient, &'static str> {
    isolated_credential(
        &format!(
            "io.assignment-manager.notion-trial.v1.{ASSIGNMENTS}.{MOODLE_VERIFICATION_WRITER}"
        ),
        "trial-write",
        interactive,
    )
    .map_err(|_| "verification_writer_keychain_unavailable")
}

fn isolated_credential(
    service: &str,
    account: &str,
    interactive: bool,
) -> Result<NotionClient, ()> {
    let token = {
        let _suppress =
            if !interactive && SecKeychain::user_interaction_allowed().map_err(|_| ())? {
                Some(SecKeychain::disable_user_interaction().map_err(|_| ())?)
            } else {
                None
            };
        let store = Store::new().map_err(|_| ())?;
        let entry = store.build(service, account, None).map_err(|_| ())?;
        Zeroizing::new(entry.get_secret().map_err(|_| ())?)
    };
    NotionClient::new(std::str::from_utf8(&token).map_err(|_| ())?).map_err(|_| ())
}

fn preflight(
    target: &NotionTarget,
    directory: &Path,
    id: &str,
    run: &RecurringVerification<'_>,
) -> Result<(), &'static str> {
    assert!(
        fs::read_dir(directory).unwrap().next().is_none(),
        "fresh isolated directory"
    );
    let interactive = io::stdin().is_terminal();
    let reader = reader(interactive)?;
    let writer = writer(interactive)?;
    reader
        .verify_bot(MOODLE_VERIFICATION_READER)
        .map_err(|_| "verification_reader_identity_failed")?;
    writer
        .check_verification_assignment_writer(target)
        .map_err(|_| "verification_writer_identity_failed")?;
    writer
        .retrieve_schema(target)
        .map_err(|_| "verification_writer_schema_unreadable")?;
    let before = read_snapshot(&reader, target, run.key());
    assert!(before.sample.is_none(), "synthetic key must be unused");
    fs::write(
        directory.join("baseline.json"),
        serde_json::to_vec(&before.protected).unwrap(),
    )
    .unwrap();
    StateStore::initialize(&directory.join("state.sqlite3"), ASSIGNMENTS).unwrap();
    fs::write(directory.join("run-id"), id).unwrap();
    Ok(())
}

fn enable(directory: &Path, id: &str, run: &RecurringVerification<'_>) -> Result<(), &'static str> {
    assert_run_id(directory, id);
    if env::var("ASSIGNMENT_MANAGER_RECURRING_APPROVAL").as_deref()
        != Ok(&format!("ENABLE VERIFICATION RECURRING {id}"))
    {
        return Err("approval_missing");
    }
    run.approve(&reader(false)?, &writer(false)?)
}

fn assert_run_id(directory: &Path, id: &str) {
    assert_eq!(fs::read_to_string(directory.join("run-id")).unwrap(), id);
}

fn state(directory: &Path) -> StateStore {
    StateStore::for_apply(&directory.join("state.sqlite3"), ASSIGNMENTS).unwrap()
}

fn event(phase: &str, status: &str) -> Value {
    json!({"schema_version": 1, "phase": phase, "status": status})
}

fn blocked(phase: &str, error: &'static str) -> (Value, u8) {
    (
        json!({"schema_version": 1, "phase": phase, "status": "blocked", "error": error}),
        2,
    )
}

fn apply_phase(
    phase: &str,
    id: &str,
    target: &NotionTarget,
    directory: &Path,
    run: &RecurringVerification<'_>,
) -> (Value, u8) {
    assert!(!io::stdin().is_terminal(), "exercise noninteractive apply");
    let reader = match reader(false) {
        Ok(reader) => reader,
        Err(error) => return report_json(&RecurringReport::blocked(RecurringMode::Apply, error)),
    };
    let before = read_snapshot(&reader, target, run.key());
    let baseline = fs::read(directory.join("baseline.json")).unwrap();
    check_protected(&before, &baseline);
    if matches!(phase, "unapproved" | "create") {
        assert!(
            before.sample.is_none(),
            "the sample must be unused at creation"
        );
    } else {
        let binding = state(directory)
            .binding(run.key())
            .unwrap()
            .expect("only allow the sample created by this run");
        let page = before
            .sample
            .as_ref()
            .expect("the created sample still exists");
        assert_eq!(binding.page_id.as_deref(), page["id"].as_str());
        check_manual(page, target);
    }
    let changed = matches!(
        phase,
        "update-partial" | "repeat" | "policy-changed" | "disabled-apply"
    );
    let (label, deadline) = if changed {
        ("updated", "2030-01-02T14:59:59Z")
    } else {
        ("initial", "2030-01-01T14:59:59Z")
    };
    let input = || {
        synthetic_input(
            RecurringVerification::sample(id),
            format!("[合成・定期同期検証 {id}] {label}"),
            deadline,
        )
    };
    let can_write = matches!(phase, "create" | "update-partial" | "repeat");
    let (mut value, code) = report_json(&run.run(
        &reader,
        || {
            assert!(can_write, "blocked phase must not load the Writer");
            writer(false)
        },
        input(),
        if phase == "update-partial" {
            Err("reauth_required")
        } else {
            Ok(SyncInput::default())
        },
    ));
    let after = read_snapshot(&reader, target, run.key());
    check_protected(&after, &baseline);
    let writes = match phase {
        "create" => (1, 0),
        "update-partial" => (0, 1),
        _ => (0, 0),
    };
    let successful = matches!(value["status"].as_str(), Some("success" | "partial"))
        && value["moodle"]["application"] == "applied";
    if successful {
        assert_eq!(value["moodle"]["applied"]["create"], writes.0);
        assert_eq!(value["moodle"]["applied"]["update"], writes.1);
        let details = reader.verify_synced_assignments(target, &input()).unwrap();
        assert_eq!(details.matched, 1);
        assert!(details.field_mismatches.is_empty());
        if changed {
            check_manual(after.sample.as_ref().unwrap(), target);
        }
    }
    if writes == (0, 0) || phase == "create" && !successful {
        assert!(before.sample == after.sample, "no mutation in this phase");
    }
    if let (Some(previous), Some(current)) = (&before.sample, &after.sample) {
        assert_eq!(previous["id"], current["id"]);
    }
    value["verification"] = json!({
        "protected_unchanged": after.protected.len(),
        "sample_page": after.sample.as_ref().map(|page| &page["id"]),
    });
    (value, code)
}

fn report_json(report: &RecurringReport) -> (Value, u8) {
    (serde_json::to_value(report).unwrap(), report.exit_code())
}

fn check_protected(snapshot: &Snapshot, baseline: &[u8]) {
    assert!(
        serde_json::to_vec(&snapshot.protected).unwrap() == baseline,
        "unrelated verification pages were changed"
    );
}
