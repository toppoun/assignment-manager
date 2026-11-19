use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use assignment_manager::notion::state::TargetLock;

const TARGET: &str = "00000000-0000-4000-8000-000000000001";
const PROGRAM: &str = env!("CARGO_BIN_EXE_assignment-manager");

fn local_root(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("AssignmentManagerNotion")
}

fn call(temp: &tempfile::TempDir, args: &[&str]) -> Output {
    Command::new(PROGRAM)
        .env("LOCALAPPDATA", temp.path())
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}

fn config(temp: &tempfile::TempDir) {
    let root = local_root(temp);
    fs::create_dir(&root).unwrap();
    fs::write(root.join("config.json"), serde_json::json!({
        "notion":{
            "assignments_data_source_id":TARGET,
            "courses_data_source_id":"00000000-0000-4000-8000-000000000002",
            "properties":{"title":"title","deadline":"date","course":"course","submission_url":"url","sync_key":"sync","source":"source"}
        },
        "webclass":true
    }).to_string()).unwrap();
}

#[test]
fn invalid_arguments_and_configuration_are_reported_without_echoing_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(&temp, &["sync", "--token", "synthetic-secret-marker"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("invalid_arguments"));
    assert!(!stderr(&output).contains("synthetic-secret-marker"));
    assert!(!local_root(&temp).exists());
    let output = call(&temp, &["doctor"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("configuration_missing"));
    config(&temp);
    fs::write(
        local_root(&temp).join("config.json"),
        br#"{"token":"synthetic-secret-marker"}"#,
    )
    .unwrap();
    let output = call(&temp, &["doctor"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("configuration_invalid"));
    assert!(!stderr(&output).contains("synthetic-secret-marker"));
}

#[test]
fn hold_test_lock_child() {
    let Some(directory) = std::env::var_os("ASSIGNMENT_MANAGER_TEST_LOCK_CHILD") else {
        return;
    };
    let directory = Path::new(&directory);
    let _lock = TargetLock::acquire(directory).unwrap();
    fs::write(directory.join("test-ready"), []).unwrap();
    thread::sleep(Duration::from_secs(8));
}

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_second_process_is_rejected_and_a_killed_process_releases_the_lock() {
    let temp = tempfile::tempdir().unwrap();
    config(&temp);
    let directory = local_root(&temp).join("targets").join(TARGET);
    let mut child = TestChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hold_test_lock_child", "--nocapture"])
            .env("ASSIGNMENT_MANAGER_TEST_LOCK_CHILD", &directory)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !directory.join("test-ready").is_file() {
        assert!(start.elapsed() < Duration::from_secs(4));
        thread::sleep(Duration::from_millis(15));
    }
    let result = call(&temp, &["init-state"]);
    assert_eq!(result.status.code(), Some(4));
    assert!(stderr(&result).contains("target_busy"));
    assert!(!directory.join("state.sqlite3").exists());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(directory.join("run.lock").is_file());
    let result = call(&temp, &["init-state"]);
    assert!(result.status.success(), "{}", stderr(&result));
    let result = call(&temp, &["status"]);
    assert!(result.status.success(), "{}", stderr(&result));
    assert!(stdout(&result).contains("state=ready pending=0"));
}

#[cfg(not(feature = "synthetic-https-fixture"))]
#[test]
fn the_default_binary_refuses_fixture_network_and_apply() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(&temp, &["sync", "--dry-run", "--fixture"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("fixture_feature_required"));
    assert!(!local_root(&temp).exists());
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn cli_fixture_exercises_both_sources_and_never_changes_the_state_database() {
    use assignment_manager::notion::{SyncKey, fixture, state::StateStore};
    let temp = tempfile::tempdir().unwrap();
    let first = call(&temp, &["sync", "--dry-run", "--fixture"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let directory = local_root(&temp).join("fixture");
    let state_path = directory.join("state.sqlite3");
    assert!(!state_path.exists());
    for line in [
        "moodle=success",
        "webclass=success",
        "Create=10 Update=0 Unchanged=0 Conflict=0 Deferred=0",
        "result=success",
    ] {
        assert!(stdout(&first).contains(line), "{}", stdout(&first));
    }
    let created = call(&temp, &["init-state", "--fixture"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let target = fixture::target().validated_id().unwrap();
    let key: SyncKey = "v1:b7e92444ae37a18bb36c597fd2e6bd2ba0999bff4ece1984c465aba44cb977aa"
        .parse()
        .unwrap();
    let state = StateStore::for_apply(&state_path, &target).unwrap();
    assert!(state.reserve_create(&key).unwrap());
    state.mark_unknown(&key).unwrap();
    drop(state);
    let before = fs::read(&state_path).unwrap();
    let dry_run = call(&temp, &["sync", "--dry-run", "--fixture"]);
    assert_eq!(dry_run.status.code(), Some(3), "{}", stderr(&dry_run));
    assert!(
        stdout(&dry_run).contains("Create=9 Update=0 Unchanged=0 Conflict=0 Deferred=1"),
        "{}",
        stdout(&dry_run)
    );
    assert_eq!(fs::read(&state_path).unwrap(), before);

    let status = call(&temp, &["status", "--fixture"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("state=ready pending=1"));
    assert!(stdout(&status).contains("last_fetch=none last_apply=none"));
    assert!(stdout(&status).contains("result=partial issue=deferred"));
    let receipt = fs::read_to_string(directory.join("last-dry-run.json")).unwrap();
    let output = format!(
        "{}{}{}{}{}",
        stdout(&first),
        stderr(&first),
        stdout(&dry_run),
        stdout(&status),
        receipt
    );
    for private in [
        "student-test",
        "synthetic-password",
        "1001@cms.example.edu",
        "fixture-course-001",
        "fixture-notion-token",
        "synthetic-secret",
        "v1:",
    ] {
        assert!(!output.contains(private));
    }
}
