use super::*;

struct Fixture {
    target: MoodleCreateTarget,
    input: SyncInput,
    mappings: [CourseMapping; 1],
    before: Value,
    after: Value,
    properties: Value,
}

impl Fixture {
    fn new() -> Self {
        let (target, original, mappings, mut before) = create_fixture();
        let input = acquire_moodle_fixture(Scenario::MoodleTrialUpdate);
        let (key, updated) = input.assignments().first_key_value().unwrap();
        assert_eq!(input.assignments().len(), 1);
        let previous = original
            .assignments()
            .get(key)
            .expect("same Moodle UID and sync key");
        assert_eq!(updated.course_key, previous.course_key);
        assert_eq!(updated.source, previous.source);
        assert_eq!(updated.submission_url, previous.submission_url);
        assert_ne!(updated.title, previous.title);
        assert_ne!(updated.deadline, previous.deadline);
        let Some(Deadline::DateTime(due)) = updated.deadline else {
            panic!("fixture deadline")
        };

        before["id"] = json!(PAGE_A);
        before["properties"]["種別"] =
            json!({"id":"category","type":"select","select":{"name":"Manual"}});
        before["children"] = json!([{"type":"paragraph","payload":"Manual body"}]);
        let mut after = before.clone();
        after["last_edited_time"] = json!("2026-11-21T00:00:00Z");
        after["properties"]["タイトル（改名済）"]["title"] = json!([text(&updated.title)]);
        after["properties"]["期限"]["date"] =
            json!({"start":due.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
        let properties = json!({
            "title":{"title":after["properties"]["タイトル（改名済）"]["title"]},
            "due":{"date":after["properties"]["期限"]["date"]},
        });
        Self {
            target,
            input,
            mappings,
            before,
            after,
            properties,
        }
    }

    fn key(&self) -> &SyncKey {
        self.input.assignments().first_key_value().unwrap().0
    }

    fn writer_baseline(&self, rows: Vec<Value>) -> Vec<Exchange> {
        let mut script = vec![bot(WRITE_BOT)];
        script.extend(create_snapshot(rows.clone()));
        script.extend(create_snapshot(rows));
        script
    }

    fn apply(
        &self,
        client: &NotionClient,
        dir: &Path,
        approve: impl FnOnce() -> bool,
    ) -> Result<Option<ApplyReport>, ApplyError> {
        client.apply_moodle_update(&self.target, &self.input, &self.mappings, dir, approve)
    }

    fn preview(
        &self,
        client: &NotionClient,
        dir: &Path,
    ) -> Result<MoodleUpdatePreview, ApplyError> {
        client.preview_moodle_update(&self.target, &self.input, &self.mappings, dir)
    }
}

#[test]
fn update_trial_changes_only_title_and_deadline_and_reopens_the_same_binding() {
    let fixture = Fixture::new();
    let dir = apply_state();
    open_state(dir.path())
        .confirm(fixture.key(), PAGE_A)
        .unwrap();
    let path = dir.path().join("state.sqlite3");
    let bytes_before = std::fs::read(&path).unwrap();
    let phase3d = task(PAGE_D, Some(&key("phase3d")), "Previous trial");
    let original_rows = vec![phase3d.clone(), fixture.before.clone()];
    let updated_rows = vec![phase3d, fixture.after.clone()];
    let mut script = vec![bot(READ_BOT)];
    script.extend(create_snapshot(original_rows.clone()));
    script.extend(fixture.writer_baseline(original_rows));
    script.push(get_task(fixture.before.clone()));
    script.push(patch_task(
        fixture.after.clone(),
        fixture.properties.clone(),
    ));
    script.push(bot(READ_BOT));
    script.extend(create_snapshot(updated_rows.clone()));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(updated_rows));
    let (client, server) = mock(script);

    let before = fixture.preview(&client, dir.path()).unwrap();
    assert_eq!((before.counts.create, before.counts.update), (0, 1));
    assert!(before.can_update);
    let applied = fixture
        .apply(&client, dir.path(), || {
            assert!(matches!(
                TargetLock::acquire(dir.path()),
                Err(StateError::Busy)
            ));
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!((applied.counts.create, applied.counts.update), (0, 1));
    let after = fixture.preview(&client, dir.path()).unwrap();
    assert_eq!(
        (
            after.counts.create,
            after.counts.update,
            after.counts.unchanged
        ),
        (0, 0, 1)
    );
    assert_eq!(after.counts.course_relation_mismatch, 0);
    assert!(!after.can_update);
    assert_eq!(before.existing_rows, after.existing_rows);
    assert_eq!(before.other_managed_revision, after.other_managed_revision);
    assert_eq!(
        fixture.apply(&client, dir.path(), || panic!("no update to approve")),
        Err(ApplyError::UpdateBlocked)
    );

    for property in [
        "状態",
        "優先度",
        "メモ",
        "種別",
        "繰り返し",
        "提出URL",
        "授業",
        "同期キー",
        "取得元",
    ] {
        assert_eq!(
            fixture.before["properties"][property],
            fixture.after["properties"][property]
        );
    }
    assert_eq!(fixture.before["children"], fixture.after["children"]);
    let reopened = open_state(dir.path())
        .binding(fixture.key())
        .unwrap()
        .unwrap();
    assert_eq!(reopened.page_id.as_deref(), Some(PAGE_A));
    assert_eq!(reopened.status, state::BindingStatus::Linked);
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    server.join().unwrap();
}

#[test]
fn update_trial_rejects_a_collision_missing_page_or_different_relation() {
    let fixture = Fixture::new();
    let dir = apply_state();
    open_state(dir.path())
        .confirm(fixture.key(), PAGE_A)
        .unwrap();
    let path = dir.path().join("state.sqlite3");
    let bytes_before = std::fs::read(&path).unwrap();
    let mut duplicate = fixture.before.clone();
    duplicate["id"] = json!(PAGE_B);
    let mut other_relation = fixture.before.clone();
    other_relation["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut script = vec![bot(WRITE_BOT)];
    script.extend(snapshot(vec![fixture.before.clone(), duplicate.clone()]));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![duplicate]));
    script.push(bot(WRITE_BOT));
    script.extend(create_snapshot(vec![]));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![other_relation]));
    let (client, server) = mock(script);
    for _ in 0..4 {
        assert_eq!(
            fixture.apply(&client, dir.path(), || panic!(
                "unsafe plan offered for approval"
            )),
            Err(ApplyError::UpdateBlocked)
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    server.join().unwrap();
}

#[test]
fn update_trial_stops_if_the_page_changes_after_the_second_snapshot() {
    let fixture = Fixture::new();
    let dir = apply_state();
    open_state(dir.path())
        .confirm(fixture.key(), PAGE_A)
        .unwrap();
    let mut edited = fixture.before.clone();
    edited["last_edited_time"] = json!("2026-11-21T00:00:00Z");
    edited["properties"]["メモ"]["rich_text"] = json!([text("Changed before PATCH")]);
    let mut script = fixture.writer_baseline(vec![fixture.before.clone()]);
    script.push(get_task(edited));
    let (client, server) = mock(script);
    let report = fixture
        .apply(&client, dir.path(), || true)
        .unwrap()
        .unwrap();
    assert_eq!(report.counts.update, 0);
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.error, Some(NotionError::Conflict));
    server.join().unwrap();
}

#[test]
fn update_trial_does_not_repeat_a_patch_with_an_unknown_result() {
    let fixture = Fixture::new();
    let dir = apply_state();
    open_state(dir.path())
        .confirm(fixture.key(), PAGE_A)
        .unwrap();
    let path = dir.path().join("state.sqlite3");
    let bytes_before = std::fs::read(&path).unwrap();
    let mut script = fixture.writer_baseline(vec![fixture.before.clone()]);
    script.push(get_task(fixture.before.clone()));
    script.push(Exchange::error(
        "PATCH",
        format!("/v1/pages/{PAGE_A}"),
        500,
        "",
    ));
    script.push(get_task(fixture.before.clone()));
    let (client, server) = mock(script);
    let report = fixture
        .apply(&client, dir.path(), || true)
        .unwrap()
        .unwrap();
    assert_eq!(report.counts.update, 0);
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.error, Some(NotionError::Server(500)));
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    server.join().unwrap();
}

#[test]
fn update_trial_stops_on_a_partially_written_result() {
    let fixture = Fixture::new();
    let dir = apply_state();
    open_state(dir.path())
        .confirm(fixture.key(), PAGE_A)
        .unwrap();
    let path = dir.path().join("state.sqlite3");
    let bytes_before = std::fs::read(&path).unwrap();
    let mut partial = fixture.after.clone();
    partial["properties"]["期限"] = fixture.before["properties"]["期限"].clone();
    let mut script = fixture.writer_baseline(vec![fixture.before.clone()]);
    script.push(get_task(fixture.before.clone()));
    script.push(patch_task(partial.clone(), fixture.properties.clone()));
    script.push(get_task(partial.clone()));
    script.push(bot(READ_BOT));
    script.extend(snapshot(vec![partial.clone()]));
    script.push(bot(WRITE_BOT));
    script.extend(snapshot(vec![partial]));
    let (client, server) = mock(script);
    let report = fixture
        .apply(&client, dir.path(), || true)
        .unwrap()
        .unwrap();
    assert_eq!(report.counts.update, 0);
    assert_eq!(report.counts.deferred, 1);
    assert_eq!(report.error, Some(NotionError::InvalidResponse));
    let partial_preview = fixture.preview(&client, dir.path()).unwrap();
    assert_eq!(
        (partial_preview.counts.create, partial_preview.counts.update),
        (0, 1)
    );
    assert!(!partial_preview.can_update);
    assert_eq!(
        fixture.apply(&client, dir.path(), || panic!(
            "partial result is not a fresh update"
        )),
        Err(ApplyError::UpdateBlocked)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    server.join().unwrap();
}
