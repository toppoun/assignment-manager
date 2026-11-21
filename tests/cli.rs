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

fn test_home(temp: &tempfile::TempDir) -> PathBuf {
    let path = temp.path().to_path_buf();
    // macOS's default TMPDIR is under the /var symlink, which state paths reject.
    #[cfg(target_os = "macos")]
    let path = path.canonicalize().unwrap();
    path
}

fn local_root(temp: &tempfile::TempDir) -> PathBuf {
    let root = test_home(temp);
    #[cfg(target_os = "macos")]
    let root = root.join("Library/Application Support");
    root.join("AssignmentManagerNotion")
}

fn call(temp: &tempfile::TempDir, args: &[&str]) -> Output {
    #[cfg(target_os = "macos")]
    let variable = "HOME";
    #[cfg(not(target_os = "macos"))]
    let variable = "LOCALAPPDATA";
    Command::new(PROGRAM)
        .env(variable, test_home(temp))
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
    fs::create_dir_all(&root).unwrap();
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
fn trial_write_requires_explicit_flag_and_target() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["notion-trial", "--allow-notion-write"],
        vec!["notion-trial", "--target", TARGET, "--apply"],
        vec![
            "notion-trial",
            "--target",
            TARGET,
            "--dry-run",
            "--allow-notion-write",
        ],
    ] {
        let output = call(&temp, &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("invalid_arguments"));
    }
    assert!(!local_root(&temp).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn trial_cli_defaults_to_dry_run_and_refuses_redirected_apply_before_config() {
    let temp = tempfile::tempdir().unwrap();
    let denied = call(
        &temp,
        &["notion-trial", "--target", TARGET, "--allow-notion-write"],
    );
    assert_eq!(denied.status.code(), Some(2));
    assert!(stderr(&denied).contains("trial_requires_terminal"));
    let preview = call(&temp, &["notion-trial", "--target", TARGET]);
    assert_eq!(preview.status.code(), Some(2));
    assert!(stderr(&preview).contains("configuration_missing"));
    assert!(!local_root(&temp).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn trial_cli_rejects_a_production_target_before_credentials_or_state() {
    let temp = tempfile::tempdir().unwrap();
    let prod = "41d859347e84811ca9bb000b8b646c3f";
    let root = local_root(&temp);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("trial.json"), serde_json::json!({
        "notion":{
            "assignments_data_source_id":prod,
            "courses_data_source_id":"00000000-0000-4000-8000-000000000002",
            "properties":{"title":"title","deadline":"date","course":"course","submission_url":"url","sync_key":"sync","source":"source"}
        },
        "read_bot_id":"00000000-0000-4000-8000-000000000011",
        "write_bot_id":"00000000-0000-4000-8000-000000000012",
        "assignment":{"title":"Synthetic task"}
    }).to_string()).unwrap();
    let output = call(&temp, &["notion-trial", "--target", prod, "init-state"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("trial_target_rejected"));
    assert!(!root.join("trial-targets").exists());
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
    for args in [
        &["sync", "--dry-run", "--fixture"][..],
        &["fixture-apply"],
        &["university-check", "--fixture"],
    ] {
        let output = call(&temp, args);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("fixture_feature_required"));
    }
    assert!(!local_root(&temp).exists());
}

#[test]
fn university_check_requires_the_exact_opt_in_and_moodle_configuration() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        &["university-check", "moodle"][..],
        &["university-check", "webclass", "--allow-university-read"],
        &["university-check", "--fixture", "--allow-university-read"],
        &["--config", "unused.json", "university-check", "--fixture"],
    ] {
        let output = call(&temp, args);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("invalid_arguments"));
    }
    assert!(!local_root(&temp).exists());

    config(&temp);
    let output = call(
        &temp,
        &["university-check", "moodle", "--allow-university-read"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).contains("moodle acquired=0 failures=1 error=setup"));
    assert!(stderr(&output).contains("source_disabled"));
    assert_eq!(fs::read_dir(local_root(&temp)).unwrap().count(), 1);
}

#[cfg(windows)]
#[test]
fn university_moodle_stops_before_credentials_without_crl_evidence() {
    let temp = tempfile::tempdir().unwrap();
    config(&temp);
    let path = local_root(&temp).join("config.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings["moodle"] = serde_json::json!({
        "host": "moodle-crl.synthetic.invalid",
        "path": "/calendar/export_execute.php",
        "scope": "default.course-related"
    });
    fs::write(&path, settings.to_string()).unwrap();
    let check = || {
        call(
            &temp,
            &["university-check", "moodle", "--allow-university-read"],
        )
    };
    for evidence in [
        None,
        Some(serde_json::json!({
            "leaf_certificate": "synthetic-missing-leaf.cer",
            "issuer_certificate": "synthetic-missing-issuer.cer",
            "crl": "synthetic-missing.crl"
        })),
    ] {
        if let Some(evidence) = evidence {
            settings["moodle"]["windows_crl"] = evidence;
            fs::write(&path, settings.to_string()).unwrap();
        }
        let output = check();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stdout(&output).lines().collect::<Vec<_>>(),
            [
                "moodle acquired=0 failures=1 error=fetch",
                "moodle diagnostic=tls_revocation_unconfirmed",
                "result=failed"
            ]
        );
        assert!(stderr(&output).is_empty());
        assert_eq!(fs::read_dir(local_root(&temp)).unwrap().count(), 1);
    }
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn university_fixture_prints_only_counts_and_creates_no_state() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(&temp, &["university-check", "--fixture"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        [
            "moodle acquired=1 failures=0",
            "webclass acquired=9 failures=0",
            "result=success"
        ]
    );
    assert!(stderr(&output).is_empty());
    assert!(!local_root(&temp).exists());
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn cli_fixture_apply_creates_then_resyncs_and_leaves_existing_state_alone() {
    let temp = tempfile::tempdir().unwrap();
    let rejected = call(&temp, &["fixture-apply", "--allow-notion-read"]);
    assert_eq!(rejected.status.code(), Some(2));
    let rejected = call(&temp, &["--config", "unused.json", "fixture-apply"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!local_root(&temp).exists());

    let init = call(&temp, &["init-state", "--fixture"]);
    assert!(init.status.success(), "{}", stderr(&init));
    let state = local_root(&temp).join("fixture/state.sqlite3");
    let before = fs::read(&state).unwrap();
    let output = call(&temp, &["fixture-apply"]);
    assert!(
        output.status.success(),
        "{} {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("pass=initial Created=10 Updated=0 Unchanged=0"));
    assert!(stdout(&output).contains("pass=repeat Created=0 Updated=0 Unchanged=10"));
    assert!(stdout(&output).contains("result=success"));
    assert_eq!(fs::read(&state).unwrap(), before);
    let fixture_apply = local_root(&temp).join("fixture-apply");
    assert_eq!(fs::read_dir(fixture_apply).unwrap().count(), 1);
    assert!(!local_root(&temp).join("targets").exists());
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
        "course_relation_mismatch=0",
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
