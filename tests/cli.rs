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
    let mut command = Command::new(PROGRAM);
    #[cfg(windows)]
    {
        // This suite checks CLI argument and config handling. The browser fixture
        // suite is the only test entry point that starts an installed Edge.
        for variable in [
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramFiles",
            "TMP",
            "TEMP",
        ] {
            command.env(variable, temp.path());
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    // Isolate the Node.js lookup and keep production browsers from starting.
    command.env("PATH", test_home(temp));
    command
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

#[cfg(target_os = "macos")]
#[test]
fn recurring_cli_reports_blocked_as_versioned_json_without_credentials_or_state() {
    let temp = tempfile::tempdir().unwrap();
    for (argument, mode, error) in [
        ("--apply", "apply", "configuration_required"),
        ("--dry-run", "preview", "configuration_required"),
        (
            "--disable-writes",
            "disable_writes",
            "configuration_required",
        ),
        (
            "--enable-writes",
            "enable_writes",
            "interactive_terminal_required",
        ),
    ] {
        let output = call(&temp, &["sync", "recurring", argument]);
        assert_eq!(output.status.code(), Some(2));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["mode"], mode);
        assert_eq!(report["error"], error);
        assert!(stderr(&output).is_empty());
    }
    assert!(!local_root(&temp).exists());
}

#[cfg(target_os = "macos")]
fn recurring_config(path: &Path, [assignments, courses, rules]: [&str; 3]) -> serde_json::Value {
    let config = serde_json::json!({
        "notion": {
            "assignments_data_source_id": assignments,
            "courses_data_source_id": courses,
            "properties": {
                "title":"title","deadline":"date","course":"course","submission_url":"url",
                "sync_key":"sync","source":"source"
            },
            "course_rules": {
                "data_source_id": rules,
                "course_term":"term","source":"source","scope":"scope","external_id":"id",
                "term":"term","course":"course","status":"status","setup":{"group_id":"gid"}
            }
        },
        "webclass": false,
        "moodle":{"host":"moodle.fixture.invalid","path":"/calendar/export","scope":"default.course-related"}
    });
    fs::write(path, config.to_string()).unwrap();
    config
}

#[cfg(target_os = "macos")]
#[test]
fn recurring_cli_disables_offline_when_policy_is_removed_and_respects_target_lock() {
    use assignment_manager::notion::state::StateStore;
    let temp = tempfile::tempdir().unwrap();
    let path = test_home(&temp).join("config.json");
    let assignments = "41d85934-7e84-811c-a9bb-000b8b646c3f";
    recurring_config(
        &path,
        [
            assignments,
            "41d85934-7e84-815d-bab8-000b9dbad959",
            "42285934-7e84-8183-8e04-000ba36d1080",
        ],
    );
    let run = |action| {
        let output = call(
            &temp,
            &[
                "--config",
                path.to_str().unwrap(),
                "sync",
                "recurring",
                action,
            ],
        );
        assert!(stderr(&output).is_empty());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        (output.status.code().unwrap(), report)
    };
    let (code, report) = run("--apply");
    assert_eq!(code, 2);
    assert_eq!(report["error"], "recurring_not_configured");
    let (code, report) = run("--disable-writes");
    assert_eq!(code, 2);
    assert_eq!(report["error"], "state_missing_or_invalid");
    assert!(!local_root(&temp).exists());

    let directory = local_root(&temp).join("targets").join(assignments);
    fs::create_dir_all(&directory).unwrap();
    StateStore::initialize(&directory.join("state.sqlite3"), assignments).unwrap();
    let lock = TargetLock::acquire(&directory).unwrap();
    let (code, report) = run("--disable-writes");
    assert_eq!(code, 4);
    assert_eq!(report["status"], "busy");
    assert_eq!(report["error"], "target_busy");
    drop(lock);
    for _ in 0..2 {
        let (code, report) = run("--disable-writes");
        assert_eq!(code, 0);
        assert_eq!(report["mode"], "disable_writes");
        assert_eq!(report["status"], "disabled");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn recurring_cli_rejects_verification_target_and_has_no_live_test_action() {
    let temp = tempfile::tempdir().unwrap();
    let path = test_home(&temp).join("verification.json");
    let mut config = recurring_config(
        &path,
        [
            "41e85934-7e84-817e-b1d2-000b68726503",
            "41e85934-7e84-81df-902b-000bbece9bf7",
            "42285934-7e84-81a9-9770-000be500f527",
        ],
    );
    config["recurring_sync"] = serde_json::json!({
        "writer_bot_id": "00000000-0000-4000-8000-000000000001",
        "moodle": {"max_creates": 5, "max_updates": 3}
    });
    fs::write(&path, config.to_string()).unwrap();
    for action in ["--dry-run", "--apply", "--disable-writes"] {
        let output = call(
            &temp,
            &[
                "--config",
                path.to_str().unwrap(),
                "sync",
                "recurring",
                action,
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["error"], "target_mismatch");
    }
    let output = call(&temp, &["sync", "recurring", "verification_live_recurring"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("invalid_arguments"));
    assert!(!local_root(&temp).exists());
}

#[cfg(all(windows, feature = "synthetic-https-fixture"))]
#[test]
fn moodle_trial_rejects_redirected_input_before_opening_config_or_state() {
    let temp = tempfile::tempdir().unwrap();
    for command in ["--allow-notion-write", "--allow-notion-update"] {
        let output = call(
            &temp,
            &[
                "--config",
                "missing-fixture-config.json",
                "notion-moodle-trial",
                "--writer",
                "00000000-0000-4000-8000-000000000012",
                command,
                "--uid",
                "review-submit@example.invalid",
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stdout(&output).trim(),
            "result=failed error=trial_requires_terminal"
        );
    }
    assert!(!local_root(&temp).exists());
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn moodle_fixture_plan_only_prints_counts_and_creates_no_local_state() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(&temp, &["sync", "moodle", "--dry-run", "--fixture"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output).trim_end(),
        "result=success moodle=fixture candidates=3 create=3 update=0 unchanged=0 errors=0 conflict=0 deferred=0 course_relation_mismatch=0"
    );
    assert_eq!(stderr(&output), "");
    assert!(!local_root(&temp).exists());
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
#[cfg(target_os = "macos")]
fn live_course_setup_requires_read_flags_and_rejects_apply_before_opening_local_state() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = [
        "--config",
        "missing.json",
        "sync",
        "--setup-courses",
        "--term",
        "2026:2",
    ];
    for flags in [
        &["--allow-university-read"][..],
        &["--allow-notion-read"][..],
        &["--allow-university-read", "--allow-notion-read", "--apply"][..],
        &[
            "--allow-university-read",
            "--allow-notion-read",
            "--apply",
            "--allow-notion-write",
        ][..],
    ] {
        let result = call(&temp, &[prefix.as_slice(), flags].concat());
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(&result).contains("invalid_arguments"));
    }
    let read = ["--allow-university-read", "--allow-notion-read"];
    let result = call(&temp, &[prefix.as_slice(), &read].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(stderr(&result).contains("configuration_missing"));

    let config_path = temp.path().join("verification.json");
    fs::write(&config_path, serde_json::json!({
        "notion":{
            "assignments_data_source_id":TARGET,
            "courses_data_source_id":"00000000-0000-4000-8000-000000000002",
            "properties":{"title":"title","deadline":"date","course":"course","submission_url":"url","sync_key":"sync","source":"source"},
            "course_rules":{
                "data_source_id":"00000000-0000-4000-8000-000000000003",
                "source":"origin","scope":"scope","external_id":"external","term":"term","course":"destination","status":"status",
                "course_term":"course-term","setup":{"group_id":"group"}
            }
        },
        "webclass":true
    }).to_string()).unwrap();
    let mut prefix = prefix;
    prefix[1] = config_path.to_str().unwrap();
    let result = call(&temp, &[prefix.as_slice(), &read].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(stdout(&result).contains("moodle=disabled"));
    assert!(stderr(&result).contains("source_disabled"));
    let id = "a".repeat(64);
    let apply = [
        "--allow-university-read",
        "--allow-notion-read",
        "--apply",
        "--allow-notion-write",
        "--expect-plan",
        &id,
    ];
    let result = call(&temp, &[prefix.as_slice(), &apply].concat());
    assert_eq!(result.status.code(), Some(2));
    assert!(stdout(&result).contains("course_setup diagnostic=request_not_allowed"));
    assert!(!local_root(&temp).exists());
}

#[test]
#[cfg(target_os = "macos")]
fn live_dry_run_requires_both_read_flags_and_rejects_other_targets() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("other-target.json");
    fs::write(&path, serde_json::json!({
        "notion": {
            "assignments_data_source_id": TARGET,
            "courses_data_source_id": "00000000-0000-4000-8000-000000000002",
            "properties": {"title":"title","deadline":"date","course":"course","submission_url":"url","sync_key":"sync","source":"source"},
            "course_rules": {
                "data_source_id": "00000000-0000-4000-8000-000000000003",
                "source":"origin","scope":"scope","external_id":"external","term":"term","course":"destination","status":"status",
                "course_term":"course-term","setup":{"group_id":"group"}
            }
        },
        "webclass": true
    }).to_string()).unwrap();
    let prefix = ["--config", path.to_str().unwrap(), "sync", "--dry-run"];
    for flags in [
        &["--allow-university-read"][..],
        &["--allow-notion-read"][..],
        &["--allow-university-read", "--allow-notion-read", "--apply"][..],
    ] {
        let result = call(&temp, &[prefix.as_slice(), flags].concat());
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(&result).contains("invalid_arguments"));
    }
    let result = call(
        &temp,
        &[
            prefix.as_slice(),
            &["--allow-university-read", "--allow-notion-read"],
        ]
        .concat(),
    );
    assert_eq!(result.status.code(), Some(2));
    assert!(stdout(&result).contains("verification_target diagnostic=request_not_allowed"));
    assert!(!local_root(&temp).exists());
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
        &[
            "university-check",
            "webclass",
            "--courses-only",
            "--fixture",
        ],
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
        &["university-check", "moodle", "--require-windows-vpn"],
        &["university-check", "moodle", "--review-local"],
        &[
            "university-check",
            "moodle",
            "--allow-university-read",
            "--review-local",
        ],
        &["university-check", "webclass", "--allow-university-read"],
        &["university-check", "webclass", "--courses-only"],
        &[
            "university-check",
            "webclass",
            "--courses-only",
            "--fixture",
            "--allow-university-read",
        ],
        &[
            "--config",
            "unused.json",
            "university-check",
            "webclass",
            "--courses-only",
            "--fixture",
        ],
        &["university-check", "--fixture", "--allow-university-read"],
        &["--config", "unused.json", "university-check", "--fixture"],
        &[
            "--config",
            "unused.json",
            "university-check",
            "--fixture",
            "--review-local",
        ],
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
    let path = local_root(&temp).join("config.json");
    let mut disabled: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    disabled["webclass"] = serde_json::Value::Bool(false);
    fs::write(path, disabled.to_string()).unwrap();
    let webclass = [
        "university-check",
        "webclass",
        "--courses-only",
        "--allow-university-read",
    ];
    for flags in [
        &[][..],
        &["--keep-profile"][..],
        &["--http-probe"][..],
        &["--headless"][..],
    ] {
        let output = call(&temp, &[webclass.as_slice(), flags].concat());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("source_disabled"));
    }
    let output = call(
        &temp,
        &[
            "university-check",
            "webclass",
            "--course-id",
            "fixture-course-002",
            "--allow-university-read",
            "--keep-profile",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("source_disabled"));
    assert_eq!(fs::read_dir(local_root(&temp)).unwrap().count(), 1);
    #[cfg(windows)]
    let browser_profile = "AssignmentManagerWebClassEdge";
    #[cfg(target_os = "macos")]
    let browser_profile = "Library/Application Support/AssignmentManagerWebClassChrome";
    #[cfg(any(windows, target_os = "macos"))]
    assert!(!test_home(&temp).join(browser_profile).exists());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn webclass_authentication_does_not_open_a_browser_without_a_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(
        &temp,
        &[
            "university-check",
            "webclass",
            "--authenticate",
            "--allow-university-read",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("interactive_terminal_required"));
    assert!(!local_root(&temp).exists());
}

#[cfg(feature = "synthetic-https-fixture")]
#[test]
fn webclass_course_check_reports_only_counts_without_creating_local_state() {
    let temp = tempfile::tempdir().unwrap();
    let output = call(
        &temp,
        &[
            "university-check",
            "webclass",
            "--courses-only",
            "--fixture",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        [
            "webclass requests hoppii_get=6 hoppii_post=1 idp_get=2 idp_post=3 lms_get=4 lms_post=1 courses_get=1 total=17",
            "webclass courses=3 validation=passed",
            "result=success",
        ]
    );
    assert!(stderr(&output).is_empty());
    assert!(!local_root(&temp).exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn webclass_local_check_rejects_extra_config_fields_without_echoing_them() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("course-check.json");
    fs::write(
        &path,
        serde_json::json!({"credential_namespace": TARGET, "password":"synthetic-private-value"})
            .to_string(),
    )
    .unwrap();
    let base = [
        "--config",
        path.to_str().unwrap(),
        "university-check",
        "webclass",
        "--courses-only",
        "--check-local",
    ];
    for flags in [&[][..], &["--http-probe"][..]] {
        let output = call(&temp, &[base.as_slice(), flags].concat());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("configuration_invalid"));
        assert!(!stderr(&output).contains("synthetic-private-value"));
    }
    assert!(!local_root(&temp).exists());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn webclass_browser_local_check_looks_for_executables_without_running_them() {
    let temp = tempfile::tempdir().unwrap();
    let args = [
        "university-check",
        "webclass",
        "--courses-only",
        "--check-local",
    ];
    config(&temp);

    #[cfg(windows)]
    {
        let result = call(&temp, &args);
        assert_eq!(result.status.code(), Some(1));
        assert!(stdout(&result).contains("diagnostic=edge_unavailable"));
        let edge = test_home(&temp).join("Microsoft/Edge/Application/msedge.exe");
        fs::create_dir_all(edge.parent().unwrap()).unwrap();
        fs::write(edge, "synthetic browser executable path").unwrap();
    }

    let result = call(&temp, &args);
    assert_eq!(result.status.code(), Some(1));
    assert!(stderr(&result).is_empty());
    let status = stdout(&result);
    #[cfg(target_os = "macos")]
    if status.contains("diagnostic=chrome_unavailable") {
        return;
    }
    assert!(status.contains("network=disabled diagnostic=browser_runtime_unavailable"));

    #[cfg(windows)]
    let node = test_home(&temp).join("node.exe");
    #[cfg(target_os = "macos")]
    let node = test_home(&temp).join("node");
    fs::write(&node, "synthetic Node.js executable path").unwrap();
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&node, fs::Permissions::from_mode(0o600)).unwrap();
        let result = call(&temp, &args);
        assert_eq!(result.status.code(), Some(1));
        assert!(stdout(&result).contains("diagnostic=browser_runtime_unavailable"));
        fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let result = call(&temp, &args);
    assert!(result.status.success(), "{}", stderr(&result));
    assert_eq!(
        stdout(&result).lines().collect::<Vec<_>>(),
        [
            "webclass configuration=ready browser=present node=present network=disabled",
            "result=success",
        ]
    );
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
            &[
                "university-check",
                "moodle",
                "--allow-university-read",
                "--require-windows-vpn",
                "--review-local",
            ],
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
fn cli_course_setup_previews_then_applies_only_to_the_loopback_fixture() {
    let temp = tempfile::tempdir().unwrap();
    let args = ["sync", "--setup-courses", "--fixture", "--term", "2026:2"];
    let preview = call(&temp, &args);
    assert!(preview.status.success(), "{}", stderr(&preview));
    assert!(stdout(&preview).contains(
        "pass=preview create_courses=12 create_rules=18 reuse_courses=0 reuse_rules=0 held=0"
    ));
    let apply = call(&temp, &[args.as_slice(), &["--apply"]].concat());
    assert!(apply.status.success(), "{}", stderr(&apply));
    assert!(stdout(&apply).contains("pass=initial create_courses=12 create_rules=18"));
    assert!(stdout(&apply).contains(
        "pass=repeat create_courses=0 create_rules=0 reuse_courses=0 reuse_rules=18 held=0"
    ));
    assert!(!local_root(&temp).exists());
    let real = call(
        &temp,
        &["sync", "--setup-courses", "--term", "2026:2", "--apply"],
    );
    assert_eq!(real.status.code(), Some(2));
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
        "course_rules=complete rows=4 confirmed=4",
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
