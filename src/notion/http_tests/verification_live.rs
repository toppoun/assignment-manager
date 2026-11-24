//! Explicit, opt-in smoke test against the account's verification data source.
use std::{collections::BTreeMap, env, fs, path::PathBuf, sync::atomic::Ordering};

use apple_native_keyring_store::keychain::Store;
use aws_lc_rs::digest::{SHA256, digest};
use keyring_core::api::CredentialStoreApi;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::{
    domain::{Deadline, SubmissionTarget},
    import::{AssignmentCandidate, ExternalRecordKey, OriginKind},
    notion::{
        CourseSetupVerification, DryRunCounts, NotionClient, NotionError, NotionTarget, SyncInput,
        SyncKey, UniversitySource,
        state::StateStore,
        trial::{MOODLE_VERIFICATION_ASSIGNMENTS as ASSIGNMENTS, MOODLE_VERIFICATION_WRITER},
        wire::{page_property, text},
    },
};

const MANUAL_NOTE: &str = "Synthetic differential-sync verification note";
const MANUAL_URL: &str = "https://example.invalid/assignment-manager-verification";

fn credential(service: &str, account: &str) -> NotionClient {
    let store = Store::new().expect("login Keychain available");
    let entry = store
        .build(service, account, None)
        .expect("verification entry");
    let token = Zeroizing::new(
        entry
            .get_secret()
            .expect("verification credential readable"),
    );
    NotionClient::new(std::str::from_utf8(&token).expect("UTF-8 credential")).expect("valid client")
}

fn input(id: &str, updated: bool) -> SyncInput {
    let (label, deadline) = if updated {
        ("updated", "2030-01-02T14:59:59Z")
    } else {
        ("initial", "2030-01-01T14:59:59Z")
    };
    let candidate = AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: "assignment-manager.verification.differential.synthetic".into(),
            id: id.into(),
        },
        title: format!("[合成・差分同期検証 {id}] {label}"),
        deadline: Some(Deadline::DateTime(deadline.parse().unwrap())),
        submission: SubmissionTarget::Unknown,
        submission_url: None,
    };
    let course = ExternalRecordKey {
        origin: OriginKind::Moodle,
        scope: "default.course-related".into(),
        id: "2026-DB".into(),
    };
    let mut input = SyncInput::default();
    input
        .insert(&candidate, UniversitySource::Moodle, Some(&course), None)
        .unwrap();
    input
}

struct Snapshot {
    protected: BTreeMap<String, String>,
    sample: Option<Value>,
}

fn read_snapshot(reader: &NotionClient, target: &NotionTarget, key: &SyncKey) -> Snapshot {
    let schema = reader.retrieve_schema(target).unwrap();
    let mut snapshot = Snapshot {
        protected: BTreeMap::new(),
        sample: None,
    };
    for (id, page) in reader
        .query_data_source_pages(
            &schema.assignments_id,
            &schema.assignments_database,
            &[],
            |_, page| Ok(page.clone()),
        )
        .unwrap()
    {
        if schema.page_key(&page).unwrap().as_ref() == Some(key) {
            assert!(
                snapshot.sample.replace(page).is_none(),
                "unique synthetic key"
            );
        } else {
            let observation = json!([page["last_edited_time"], page["properties"]]);
            let hash = digest(&SHA256, &serde_json::to_vec(&observation).unwrap());
            let hex = hash
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            snapshot.protected.insert(id, hex);
        }
    }
    snapshot
}

fn open_directory() -> PathBuf {
    let path =
        PathBuf::from(env::var("ASSIGNMENT_MANAGER_DIFF_STATE").expect("isolated state path"));
    let path = path
        .canonicalize()
        .expect("existing isolated state directory");
    assert!(
        path.starts_with("/private/tmp"),
        "require temporary verification directory"
    );
    path
}

fn check_manual(sample: &Value, target: &NotionTarget) {
    let props = sample["properties"].as_object().unwrap();
    assert_eq!(props["状態"]["status"]["name"], "進行中");
    assert_eq!(text(&props["メモ"]["rich_text"]).unwrap(), MANUAL_NOTE);
    assert_eq!(
        page_property(sample, &target.properties.submission_url, "url").unwrap(),
        MANUAL_URL
    );
}

#[test]
#[ignore = "writes one dedicated synthetic assignment to the real verification data source"]
fn verification_live_differential() {
    let phase = env::var("ASSIGNMENT_MANAGER_DIFF_PHASE").expect("explicit verification phase");
    assert!(matches!(
        phase.as_str(),
        "create-loss" | "recover-create" | "update-loss" | "repeat"
    ));
    let id = env::var("ASSIGNMENT_MANAGER_DIFF_ID").expect("explicit synthetic run ID");
    assert!(id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let directory = open_directory();
    let config: Value =
        serde_json::from_slice(&fs::read("local/notion-verification.json").unwrap()).unwrap();
    let target: NotionTarget = serde_json::from_value(config["notion"].clone()).unwrap();
    CourseSetupVerification::new(&target).expect("fixed verification data sources");
    let reader = credential(
        &format!("io.assignment-manager.notion-cli.v1.{ASSIGNMENTS}"),
        "notion",
    );
    let writer = credential(
        &format!(
            "io.assignment-manager.notion-trial.v1.{ASSIGNMENTS}.{MOODLE_VERIFICATION_WRITER}"
        ),
        "trial-write",
    );
    let updated = matches!(phase.as_str(), "update-loss" | "repeat");
    let input = input(&id, updated);
    let key = input.assignments().keys().next().unwrap();
    let baseline_path = directory.join("protected.json");
    let state_path = directory.join("state.sqlite3");
    let before = read_snapshot(&reader, &target, key);
    if phase == "create-loss" {
        assert!(before.sample.is_none(), "create a fresh synthetic key");
        assert!(!baseline_path.exists() && !state_path.exists());
        fs::write(
            &baseline_path,
            serde_json::to_vec(&before.protected).unwrap(),
        )
        .unwrap();
    } else {
        assert!(before.sample.is_some());
        assert_eq!(
            serde_json::to_vec(&before.protected).unwrap(),
            fs::read(&baseline_path).unwrap()
        );
    }
    if updated {
        check_manual(before.sample.as_ref().unwrap(), &target);
    }
    let state = StateStore::read(&state_path, ASSIGNMENTS)
        .unwrap()
        .unwrap_or_default();
    let before_pending = state.pending;
    let (inspection, plan, _) = reader
        .inspect_live_assignment_dry_run(&input, &target, &[], state, false)
        .unwrap();
    assert_eq!(inspection.unresolved_creates, 0);
    let (create, update, unchanged, recovered) = match phase.as_str() {
        "create-loss" => (1, 0, 0, 0),
        "recover-create" => (0, 0, 1, 1),
        "update-loss" => (0, 1, 0, 0),
        "repeat" => (0, 0, 1, 0),
        _ => unreachable!(),
    };
    assert_eq!(before_pending, recovered);
    assert_eq!(inspection.recoverable_creates, recovered);
    assert_eq!(
        plan.counts_for(&input),
        DryRunCounts {
            create,
            update,
            unchanged,
            ..Default::default()
        }
    );
    if phase == "update-loss" {
        let properties = plan.requests()[0].body()["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 2);
        assert!(properties.contains_key(&target.properties.title));
        assert!(properties.contains_key(&target.properties.deadline));
    }
    if phase.ends_with("-loss") {
        writer.lose_verification_ack.store(true, Ordering::Relaxed);
    }
    let report = reader
        .apply_verified_assignments(&writer, &target, &input, &plan, &directory)
        .unwrap();
    assert_eq!(
        writer.verification_writes.load(Ordering::Relaxed),
        create + update
    );
    assert_eq!(report.recovered, recovered);
    if phase == "create-loss" {
        assert_eq!(report.error, Some(NotionError::Timeout));
        assert_eq!(report.pending, 1);
        assert_eq!(report.counts.deferred, 1);
    } else {
        assert_eq!(report.error, None);
        assert_eq!(report.pending, 0);
        assert_eq!(report.counts, plan.counts_for(&input));
    }
    let after = read_snapshot(&reader, &target, key);
    assert_eq!(after.protected, before.protected);
    if let Some(previous) = before.sample {
        assert_eq!(previous["id"], after.sample.as_ref().unwrap()["id"]);
    }
    if updated {
        check_manual(after.sample.as_ref().unwrap(), &target);
    }
    let details = reader.verify_synced_assignments(&target, &input).unwrap();
    assert_eq!(details.matched, 1);
    assert!(details.field_mismatches.is_empty());
    assert_eq!(details.deadlines_at_minute_precision, 1);
    println!(
        "phase={phase} sent={} recovered={} pending={} unchanged_protected={} unique_sample=1 sample_page={}",
        create + update,
        report.recovered,
        report.pending,
        after.protected.len(),
        after.sample.unwrap()["id"].as_str().unwrap(),
    );
}
