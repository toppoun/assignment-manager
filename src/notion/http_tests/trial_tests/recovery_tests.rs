use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use rusqlite::{Connection, params};

use super::*;
use crate::notion::{apply::ApplyReport, state::BindingStatus};

const CHILD_TEST: &str = "notion::http_tests::trial_tests::recovery_tests::trial_recovery_child";
const CHILD_DIRECTORY: &str = "AM_TEST_TRIAL_RECOVERY_DIRECTORY";
const CHILD_ADDRESS: &str = "AM_TEST_TRIAL_RECOVERY_ADDRESS";
const CHILD_CHECKPOINT: &str = "AM_TEST_TRIAL_RECOVERY_CHECKPOINT";
const CHECKPOINT_FILE: &str = "test-checkpoint";

fn assignment_key() -> SyncKey {
    let (input, _) = assignment().input().unwrap();
    input.assignments().first_key_value().unwrap().0.clone()
}

fn approved_run(rows: Vec<Value>) -> Vec<Exchange> {
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(snapshot(rows.clone()));
    script.extend(snapshot(rows));
    script
}

fn create_run(key: &SyncKey) -> Vec<Exchange> {
    let mut script = approved_run(vec![]);
    let key = key.clone();
    script.push(query(vec![], None).check(move |_, body| {
        assert_eq!(
            body["filter"],
            json!({"property":"sync","rich_text":{"equals":key.as_str()}})
        );
    }));
    script
}

fn serve_child(script: Vec<Exchange>) -> (SocketAddr, NotionClient, MockServer) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (client, server) = serve(listener, script, Duration::from_secs(2));
    (address, client, server)
}

struct RecoveryChild(Child);

impl RecoveryChild {
    fn start(address: SocketAddr, dir: &Path, checkpoint: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CHILD_TEST, "--nocapture"])
                .env(CHILD_DIRECTORY, dir)
                .env(CHILD_ADDRESS, address.to_string())
                .env(CHILD_CHECKPOINT, checkpoint)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn finish(mut self, expected: Result<ApplyReport, ApplyError>) {
        wait_until(
            || self.0.try_wait().unwrap().is_some(),
            "restarted trial did not finish",
        );
        let mut stdout = String::new();
        let mut stderr = String::new();
        self.0
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        assert!(self.0.wait().unwrap().success(), "{stdout}\n{stderr}");
        let expected = format!("apply_result={expected:?}");
        assert!(stdout.lines().any(|line| line == expected), "{stdout}");
    }

    fn kill(mut self) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "faulted trial already exited"
        );
        self.0.kill().unwrap();
        assert!(!self.0.wait().unwrap().success());
    }
}

impl Drop for RecoveryChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool, failure: &str) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < Duration::from_secs(8), "{failure}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn trial_recovery_child() {
    let Some(dir) = std::env::var_os(CHILD_DIRECTORY) else {
        return;
    };
    let address = std::env::var(CHILD_ADDRESS).unwrap().parse().unwrap();
    let client = NotionClient::for_mock(address, TOKEN, Duration::from_secs(15)).unwrap();
    let result = client
        .apply_trial(&trial(), &assignment(), Path::new(&dir), |_| true)
        .map(|report| report.unwrap());
    println!("apply_result={result:?}");
}

pub(in crate::notion) fn pause_create_at(checkpoint: &str) {
    let requested = std::env::var(CHILD_CHECKPOINT);
    let crash_in_sqlite =
        checkpoint == "response_received" && requested.as_deref() == Ok("sqlite_transaction");
    if requested.as_deref() != Ok(checkpoint) && !crash_in_sqlite {
        return;
    }
    let dir = PathBuf::from(std::env::var_os(CHILD_DIRECTORY).unwrap());
    if crash_in_sqlite {
        let db = Connection::open(dir.join("state.sqlite3")).unwrap();
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=4; PRAGMA cache_spill=ON;")
            .unwrap();
        let tx = db.unchecked_transaction().unwrap();
        assert_eq!(
            tx.execute(
                "UPDATE bindings SET status='linked', page_id=?1 WHERE sync_key=?2 AND status='intent'",
                params![PAGE_A, assignment_key().as_str()],
            ).unwrap(),
            1
        );
        // Force dirty pages onto disk while the rollback journal is still hot.
        tx.execute_batch(
            "CREATE TABLE uncommitted_test_data (value BLOB);
             INSERT INTO uncommitted_test_data VALUES (zeroblob(262144));",
        )
        .unwrap();
        pause_for_kill(&dir);
    }
    pause_for_kill(&dir);
}

fn pause_for_kill(dir: &Path) -> ! {
    fs::write(dir.join(CHECKPOINT_FILE), []).unwrap();
    thread::sleep(Duration::from_secs(15));
    panic!("trial was not killed at checkpoint");
}

fn report(counts: DryRunCounts, pending: usize, recovered: usize) -> ApplyReport {
    ApplyReport {
        counts,
        pending,
        recovered,
        ..ApplyReport::default()
    }
}

fn deferred() -> ApplyReport {
    report(
        DryRunCounts {
            deferred: 1,
            ..DryRunCounts::default()
        },
        1,
        0,
    )
}

fn reconciled() -> ApplyReport {
    report(
        DryRunCounts {
            unchanged: 1,
            ..DryRunCounts::default()
        },
        0,
        1,
    )
}

fn assert_binding(dir: &Path, key: &SyncKey, expected: BindingStatus, page: Option<&str>) {
    let state = StateStore::read(&dir.join("state.sqlite3"), A)
        .unwrap()
        .unwrap();
    assert_eq!(state.bindings.len(), 1);
    let binding = &state.bindings[key];
    assert_eq!(binding.status, expected);
    assert_eq!(binding.page_id.as_deref(), page);
    assert_eq!(
        state.pending,
        usize::from(expected != BindingStatus::Linked)
    );
}

#[test]
fn a_failed_intent_insert_sends_no_post_and_a_fresh_process_can_create_once() {
    let dir = directory();
    let db_path = dir.path().join("state.sqlite3");
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER test_reserve_failure BEFORE INSERT ON bindings WHEN NEW.status='intent'
         BEGIN SELECT RAISE(ABORT, 'synthetic database failure'); END;",
    )
    .unwrap();
    let key = assignment_key();
    let page = task(PAGE_A, Some(&key), &assignment().title);
    let mut script = create_run(&key);
    script.extend(create_run(&key));
    script.push(Exchange::json("POST", "/v1/pages", page.clone()));
    script.extend(approved_run(vec![page]));
    let (address, _, server) = serve_child(script);
    let before = fs::read(&db_path).unwrap();
    RecoveryChild::start(address, dir.path(), "")
        .finish(Err(ApplyError::State(StateError::Database)));
    assert_eq!(fs::read(&db_path).unwrap(), before);
    assert!(
        StateStore::read(&db_path, A)
            .unwrap()
            .unwrap()
            .bindings
            .is_empty()
    );
    db.execute_batch("DROP TRIGGER test_reserve_failure")
        .unwrap();
    RecoveryChild::start(address, dir.path(), "").finish(Ok(report(
        DryRunCounts {
            create: 1,
            ..DryRunCounts::default()
        },
        0,
        0,
    )));
    RecoveryChild::start(address, dir.path(), "").finish(Ok(report(
        DryRunCounts {
            unchanged: 1,
            ..DryRunCounts::default()
        },
        0,
        0,
    )));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn a_kill_after_intent_commit_blocks_post_even_in_a_new_process() {
    let dir = directory();
    let key = assignment_key();
    let mut script = create_run(&key);
    script.extend(approved_run(vec![]));
    let (address, client, server) = serve_child(script);
    let child = RecoveryChild::start(address, dir.path(), "intent_saved");
    wait_until(
        || dir.path().join(CHECKPOINT_FILE).is_file(),
        "intent was not committed",
    );
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    assert_eq!(
        client.apply_trial(&trial(), &assignment(), dir.path(), |_| panic!(
            "a second plan was shown"
        )),
        Err(ApplyError::State(StateError::Busy))
    );
    child.kill();
    RecoveryChild::start(address, dir.path(), "").finish(Ok(deferred()));
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    server.join().unwrap();
}

#[test]
fn a_process_killed_after_post_dispatch_reconciles_without_sending_again() {
    let dir = directory();
    let key = assignment_key();
    let (release, hold) = mpsc::channel();
    let received = dir.path().join(CHECKPOINT_FILE);
    let mut script = create_run(&key);
    script.push(
        Exchange::drop_response("POST", "/v1/pages").check(move |_, _| {
            fs::write(received, []).unwrap();
            hold.recv_timeout(Duration::from_secs(10)).unwrap();
        }),
    );
    script.extend(approved_run(vec![task(
        PAGE_A,
        Some(&key),
        &assignment().title,
    )]));
    let (address, _, server) = serve_child(script);
    let child = RecoveryChild::start(address, dir.path(), "");
    wait_until(
        || dir.path().join(CHECKPOINT_FILE).is_file(),
        "POST was not dispatched",
    );
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    child.kill();
    release.send(()).unwrap();
    RecoveryChild::start(address, dir.path(), "").finish(Ok(reconciled()));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn a_lost_post_response_reconciles_zero_duplicate_and_unique_keys_in_new_processes() {
    let dir = directory();
    let key = assignment_key();
    let page = task(PAGE_A, Some(&key), &assignment().title);
    let mut script = create_run(&key);
    script.push(Exchange::drop_response("POST", "/v1/pages"));
    script.extend(approved_run(vec![]));
    script.extend(approved_run(vec![
        page.clone(),
        task(PAGE_B, Some(&key), "Duplicate"),
    ]));
    script.extend(approved_run(vec![page]));
    let (address, _, server) = serve_child(script);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(ApplyReport {
        error: Some(NotionError::Transport),
        ..deferred()
    }));
    assert_binding(dir.path(), &key, BindingStatus::Unknown, None);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(deferred()));
    assert_binding(dir.path(), &key, BindingStatus::Unknown, None);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(report(
        DryRunCounts {
            conflict: 1,
            ..DryRunCounts::default()
        },
        1,
        0,
    )));
    assert_binding(dir.path(), &key, BindingStatus::Unknown, None);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(reconciled()));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn a_kill_after_the_post_response_preserves_the_intent_until_reconciliation() {
    let dir = directory();
    let key = assignment_key();
    let page = task(PAGE_A, Some(&key), &assignment().title);
    let mut script = create_run(&key);
    script.push(Exchange::json("POST", "/v1/pages", page.clone()));
    script.extend(approved_run(vec![page]));
    let (address, _, server) = serve_child(script);
    let child = RecoveryChild::start(address, dir.path(), "response_received");
    wait_until(
        || dir.path().join(CHECKPOINT_FILE).is_file(),
        "POST response was not processed",
    );
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    child.kill();
    RecoveryChild::start(address, dir.path(), "").finish(Ok(reconciled()));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn an_aborted_confirmation_rolls_back_sqlite_and_recovers_on_restart() {
    let dir = directory();
    let key = assignment_key();
    let db_path = dir.path().join("state.sqlite3");
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER test_confirm_failure AFTER UPDATE ON bindings WHEN NEW.status='linked'
         BEGIN SELECT RAISE(ABORT, 'synthetic database failure'); END;",
    )
    .unwrap();
    let page = task(PAGE_A, Some(&key), &assignment().title);
    let mut script = create_run(&key);
    script.push(Exchange::json("POST", "/v1/pages", page.clone()));
    script.extend(approved_run(vec![page]));
    let (address, _, server) = serve_child(script);
    RecoveryChild::start(address, dir.path(), "")
        .finish(Err(ApplyError::State(StateError::Database)));
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    db.execute_batch("DROP TRIGGER test_confirm_failure")
        .unwrap();
    RecoveryChild::start(address, dir.path(), "").finish(Ok(reconciled()));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}

#[test]
fn a_hot_sqlite_journal_rolls_back_uncommitted_confirmation_before_recovery() {
    let dir = directory();
    let key = assignment_key();
    let db_path = dir.path().join("state.sqlite3");
    let journal_path = dir.path().join("state.sqlite3-journal");
    let initial_size = fs::metadata(&db_path).unwrap().len();
    let mut script = create_run(&key);
    let page = task(PAGE_A, Some(&key), &assignment().title);
    script.push(Exchange::json("POST", "/v1/pages", page.clone()));
    script.extend(approved_run(vec![]));
    script.extend(approved_run(vec![page]));
    let (address, client, server) = serve_child(script);
    let child = RecoveryChild::start(address, dir.path(), "sqlite_transaction");
    wait_until(
        || dir.path().join(CHECKPOINT_FILE).is_file(),
        "SQLite did not spill uncommitted changes",
    );
    let journal = fs::read(&journal_path).unwrap();
    assert!(journal.starts_with(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]));
    assert!(fs::metadata(&db_path).unwrap().len() > initial_size);
    child.kill();
    assert_eq!(fs::read(&journal_path).unwrap(), journal);
    let dirty = fs::read(&db_path).unwrap();
    assert_eq!(
        client.preview_trial(&trial(), &assignment(), dir.path()),
        Err(ApplyError::State(StateError::InvalidDatabase))
    );
    assert_eq!(fs::read(&db_path).unwrap(), dirty);
    assert_eq!(fs::read(&journal_path).unwrap(), journal);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(deferred()));
    assert!(!journal_path.exists());
    assert_eq!(fs::metadata(&db_path).unwrap().len(), initial_size);
    assert_binding(dir.path(), &key, BindingStatus::CreateIntent, None);
    RecoveryChild::start(address, dir.path(), "").finish(Ok(reconciled()));
    assert_binding(dir.path(), &key, BindingStatus::Linked, Some(PAGE_A));
    server.join().unwrap();
}
