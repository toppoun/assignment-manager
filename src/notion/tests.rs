use super::*;
use crate::{
    domain::SubmissionTarget,
    import::AssignmentCandidateBatch,
    observation::AssignmentObservation,
    sync::{
        AcquiredAssignments, AcquiredWebClassCourses, SourceCourse, WebClassAcquisitionAttempt,
        WebClassReauthSyncError, WebClassSyncError,
    },
    webclass::WebClassCourse,
};

const FEED: &str = "default.course-related";

fn external(origin: OriginKind, scope: &str, id: &str) -> ExternalRecordKey {
    ExternalRecordKey {
        origin,
        scope: scope.into(),
        id: id.into(),
    }
}

fn source_candidate(origin: OriginKind, scope: &str, id: &str, title: &str) -> AssignmentCandidate {
    AssignmentCandidate {
        external_key: external(origin, scope, id),
        title: title.into(),
        deadline: None,
        submission: SubmissionTarget::Unknown,
        submission_url: None,
    }
}

pub(super) fn candidate(id: &str, title: &str) -> AssignmentCandidate {
    source_candidate(OriginKind::Moodle, FEED, id, title)
}

fn key_for(origin: OriginKind, scope: &str, id: &str) -> SyncKey {
    SyncKey::derive(&external(origin, scope, id)).unwrap()
}

pub(super) fn key(id: &str) -> SyncKey {
    key_for(OriginKind::Moodle, FEED, id)
}

fn validated(scope: SyncScope, candidates: Vec<AssignmentCandidate>) -> AcquiredAssignments {
    let observation = AssignmentObservation::try_new(
        scope,
        candidates.iter().map(|c| c.external_key.clone()).collect(),
    )
    .unwrap();
    AcquiredAssignments::try_new(
        AssignmentCandidateBatch::try_new(candidates).unwrap(),
        observation,
    )
    .unwrap()
}

pub(super) fn moodle(candidates: Vec<AssignmentCandidate>) -> MoodleAcquisition {
    MoodleAcquisition {
        assignments: validated(SyncScope::MoodleFeed(FEED.into()), candidates),
        courses: HashMap::new(),
        event_kinds: HashMap::new(),
    }
}

pub(super) fn input(candidates: Vec<AssignmentCandidate>) -> SyncInput {
    SyncInput::from_acquisitions(Some(&moodle(candidates)), None).unwrap()
}

pub(super) fn input_with_course(
    candidates: Vec<AssignmentCandidate>,
) -> (SyncInput, ExternalRecordKey) {
    let mut acquired = moodle(candidates);
    let course_key = external(OriginKind::Moodle, FEED, "course-a");
    for candidate in acquired.assignments.batch().as_slice() {
        acquired.courses.insert(
            candidate.external_key.clone(),
            SourceCourse {
                external_key: course_key.clone(),
                name: "Course A".into(),
            },
        );
    }
    (
        SyncInput::from_acquisitions(Some(&acquired), None).unwrap(),
        course_key,
    )
}

pub(super) fn group(id: &str, tasks: &[(&str, &str)]) -> AcquiredAssignments {
    validated(
        SyncScope::WebClassContents(id.into()),
        tasks
            .iter()
            .map(|(task_id, title)| source_candidate(OriginKind::WebClass, id, task_id, title))
            .collect(),
    )
}

pub(super) fn attempt(
    course_ids: &[&str],
    groups: Vec<AcquiredAssignments>,
    result: Result<(), WebClassSyncError>,
) -> WebClassAcquisitionAttempt {
    WebClassAcquisitionAttempt {
        course_list: Some(
            AcquiredWebClassCourses::from_records(
                course_ids
                    .iter()
                    .map(|id| WebClassCourse {
                        group_id: (*id).into(),
                        group_name: "Synthetic course".into(),
                        year: 2026,
                        semester: "2".into(),
                        day_of_week: "0".into(),
                        class_order: "0".into(),
                        children: String::new(),
                        group_name_en: None,
                    })
                    .collect(),
            )
            .unwrap(),
        ),
        groups,
        result,
    }
}

fn snapshot_row(
    id: &str,
    key: Option<SyncKey>,
    source: UniversitySource,
    title: &str,
) -> NotionAssignment {
    NotionAssignment {
        page_id: id.into(),
        sync_key: key,
        source: Some(source),
        title: title.into(),
        deadline: None,
        course_page_ids: vec![],
    }
}

fn moodle_row(task_id: &str, title: &str) -> NotionAssignment {
    snapshot_row(task_id, Some(key(task_id)), UniversitySource::Moodle, title)
}

#[test]
fn notion_deadline_comparison_accepts_only_the_stored_minute_boundary() {
    use super::wire::notion_deadline_matches;

    let source = deadline("2026-11-24T14:59:59Z");
    assert!(notion_deadline_matches(
        &deadline("2026-11-24T14:59:00Z"),
        &source
    ));
    assert!(!notion_deadline_matches(
        &deadline("2026-11-24T15:00:00Z"),
        &source
    ));
    assert!(!notion_deadline_matches(
        &deadline("2026-11-24T14:59:01Z"),
        &source
    ));
    assert!(!notion_deadline_matches(&None, &source));
}

fn deadline(value: &str) -> Option<Deadline> {
    Some(Deadline::DateTime(value.parse().unwrap()))
}

fn action<'a>(plan: &'a [PlannedAssignment], key: &SyncKey) -> &'a PlanAction {
    &plan.iter().find(|item| &item.key == key).unwrap().action
}

#[test]
fn notion_key_v1_is_stable_exact_and_versioned() {
    let stable = key("1001@fixture.invalid");
    assert_eq!(
        stable.as_str(),
        "v1:cb1d2cea6f4e54410e295bd914f21ac6d07c3c1e8451f5c851cb99f3467a414e"
    );
    assert_eq!(stable.as_str().parse(), Ok(stable.clone()));
    assert!("v1:bad".parse::<SyncKey>().is_err());
    assert!(
        stable
            .as_str()
            .replace("v1:", "v2:")
            .parse::<SyncKey>()
            .is_err()
    );
    assert!(stable.as_str().to_uppercase().parse::<SyncKey>().is_err());
    assert_ne!(
        stable,
        key_for(OriginKind::WebClass, FEED, "1001@fixture.invalid")
    );
    assert_ne!(stable, key(" 1001@fixture.invalid"));
    assert_ne!(stable, key("1001@Fixture.invalid"));
    assert_ne!(
        key_for(OriginKind::WebClass, "ab", "c"),
        key_for(OriginKind::WebClass, "a", "bc"),
    );
}

#[test]
fn notion_initial_plan_preserves_same_title_tasks_and_manual_rows_then_converges() {
    let input = input(vec![
        candidate("a", "Same title"),
        candidate("b", "Same title"),
    ]);
    let mut rows = vec![snapshot_row(
        "manual",
        None,
        UniversitySource::Moodle,
        "Same title",
    )];
    let initial = plan_sync(&input, NotionSnapshot::Complete(&rows), &[]);
    assert_eq!(initial.len(), 2);
    for id in ["a", "b"] {
        let PlanAction::Create {
            assignment,
            course_page_id,
        } = action(&initial, &key(id))
        else {
            panic!("expected a create");
        };
        assert_eq!(assignment.title, "Same title");
        assert_eq!(assignment.submission_url, None);
        assert_eq!(course_page_id, &None);
        rows.push(moodle_row(id, &assignment.title));
    }

    let repeat = plan_sync(&input, NotionSnapshot::Complete(&rows), &[]);
    assert_eq!(repeat.len(), 2);
    for id in ["a", "b"] {
        assert_eq!(
            action(&repeat, &key(id)),
            &PlanAction::Unchanged { page_id: id.into() }
        );
    }
}

#[test]
fn notion_updates_only_changed_source_fields_and_preserves_notion_data() {
    let mut task = candidate("a", "Updated");
    task.deadline = deadline("2026-11-27T01:00:00Z");
    let input = input(vec![task.clone()]);
    let mut row = moodle_row("a", "Original");
    row.deadline = deadline("2026-11-26T01:00:00Z");
    let plan = plan_sync(&input, NotionSnapshot::Complete(&[row]), &[]);
    let PlanAction::Update { page_id, patch } = action(&plan, &key("a")) else {
        panic!("expected an update");
    };
    assert_eq!(page_id, "a");
    // Exhaustive destructuring checks the write projection's allowed fields.
    let AssignmentPatch {
        source,
        title,
        deadline,
        course_page_id,
    } = patch;
    assert_eq!(source, &None);
    assert_eq!(title.as_deref(), Some("Updated"));
    assert_eq!(deadline, &Some(task.deadline.clone()));
    assert_eq!(course_page_id, &None);

    let prior_page = serde_json::json!({
        "title": "Original",
        "deadline": "2026-11-26T01:00:00Z",
        "submission_url": "https://example.invalid/submission",
        "status": "done",
        "priority": "high",
        "notes": "manual note",
        "recurrence": "weekly",
        "body": ["manual body"],
        "course": ["manual mapping"],
    });
    let mut applied = prior_page.clone();
    applied["title"] = serde_json::json!(title.as_ref().unwrap());
    applied["deadline"] = serde_json::json!(deadline.as_ref().unwrap());
    for preserved in [
        "submission_url",
        "status",
        "priority",
        "notes",
        "recurrence",
        "body",
        "course",
    ] {
        assert_eq!(applied[preserved], prior_page[preserved]);
    }
}

#[test]
fn notion_clears_a_deadline_when_a_current_candidate_explicitly_has_none() {
    let input = input(vec![candidate("a", "Title")]);
    let mut row = moodle_row("a", "Title");
    row.deadline = deadline("2026-11-26T01:00:00Z");
    let plan = plan_sync(&input, NotionSnapshot::Complete(&[row]), &[]);
    assert_eq!(
        action(&plan, &key("a")),
        &PlanAction::Update {
            page_id: "a".into(),
            patch: AssignmentPatch {
                deadline: Some(None),
                ..AssignmentPatch::default()
            },
        }
    );
}

#[test]
fn notion_retry_replaces_successful_scopes_and_defers_earlier_only_candidates() {
    let webclass = WebClassAcquisition::new(
        vec![
            attempt(
                &["a", "b", "c"],
                vec![
                    group("a", &[("shared", "Old"), ("old-only", "Omitted on retry")]),
                    group("b", &[("shared", "Earlier group")]),
                ],
                Err(WebClassSyncError::AuthExpired),
            ),
            attempt(
                &["a", "b", "c"],
                vec![group("a", &[("shared", "New")])],
                Err(WebClassSyncError::Fetch),
            ),
        ],
        Err(WebClassReauthSyncError::Sync(WebClassSyncError::Fetch)),
    );
    let input = SyncInput::from_acquisitions(None, Some(&webclass)).unwrap();
    let shared = key_for(OriginKind::WebClass, "a", "shared");
    let skipped = key_for(OriginKind::WebClass, "c", "never-fetched");
    let rows = [
        snapshot_row(
            "existing",
            Some(shared.clone()),
            UniversitySource::WebClass,
            "Old",
        ),
        snapshot_row(
            "skipped",
            Some(skipped.clone()),
            UniversitySource::WebClass,
            "Keep",
        ),
    ];
    let plan = plan_sync(&input, NotionSnapshot::Complete(&rows), &[]);
    assert_eq!(
        webclass.result(),
        Err(WebClassReauthSyncError::Sync(WebClassSyncError::Fetch))
    );
    assert_eq!(plan.len(), 4);
    assert_eq!(input.assignments().len(), 1);
    let normalized_course = &input.assignments()[&shared].course_key;
    assert_eq!(
        normalized_course.as_ref(),
        Some(&external(
            OriginKind::WebClass,
            DEFAULT_COURSE_SOURCE_SCOPE,
            "a"
        ))
    );
    assert_eq!(
        action(&plan, &shared),
        &PlanAction::Update {
            page_id: "existing".into(),
            patch: AssignmentPatch {
                title: Some("New".into()),
                ..AssignmentPatch::default()
            },
        }
    );
    for (scope, task) in [("a", "old-only"), ("b", "shared")] {
        assert_eq!(
            action(&plan, &key_for(OriginKind::WebClass, scope, task)),
            &PlanAction::Deferred(DeferredReason::EarlierAttempt)
        );
    }
    assert_eq!(
        action(&plan, &skipped),
        &PlanAction::Deferred(DeferredReason::NoValidatedCandidate)
    );
}

#[test]
fn notion_partial_success_uses_only_completed_groups_and_keeps_other_sources_independent() {
    let webclass = WebClassAcquisition::new(
        vec![attempt(
            &["a", "b"],
            vec![group("a", &[("task", "Confirmed")])],
            Err(WebClassSyncError::AuthExpired),
        )],
        Err(WebClassReauthSyncError::CredentialsUnavailable),
    );
    let moodle = moodle(vec![candidate("own", "Moodle")]);
    let input = SyncInput::from_acquisitions(Some(&moodle), Some(&webclass)).unwrap();
    let b = key_for(OriginKind::WebClass, "b", "task");
    let rows = [snapshot_row(
        "b",
        Some(b.clone()),
        UniversitySource::WebClass,
        "Preserved",
    )];
    let plan = plan_sync(&input, NotionSnapshot::Complete(&rows), &[]);
    for id in [key("own"), key_for(OriginKind::WebClass, "a", "task")] {
        assert!(matches!(action(&plan, &id), PlanAction::Create { .. }));
    }
    assert_eq!(
        action(&plan, &b),
        &PlanAction::Deferred(DeferredReason::NoValidatedCandidate)
    );
}

#[test]
fn notion_defers_earlier_groups_when_retry_discovery_fails() {
    let webclass = WebClassAcquisition::new(
        vec![
            attempt(
                &["a", "b"],
                vec![group("a", &[("task", "Old")])],
                Err(WebClassSyncError::AuthExpired),
            ),
            WebClassAcquisitionAttempt {
                course_list: None,
                groups: vec![],
                result: Err(WebClassSyncError::AuthExpired),
            },
        ],
        Err(WebClassReauthSyncError::Sync(
            WebClassSyncError::AuthExpired,
        )),
    );
    let input = SyncInput::from_acquisitions(None, Some(&webclass)).unwrap();
    let plan = plan_sync(&input, NotionSnapshot::Complete(&[]), &[]);
    assert_eq!(plan.len(), 1);
    assert_eq!(
        plan[0].action,
        PlanAction::Deferred(DeferredReason::EarlierAttempt)
    );
}

#[test]
fn notion_duplicate_or_ambiguous_target_blocks_affected_keys() {
    let input = input(vec![
        candidate("a", "New"),
        candidate("b", "New"),
        candidate("c", "New"),
    ]);
    let mut duplicate = moodle_row("a", "Duplicated");
    duplicate.page_id = "duplicate-a".into();
    let manual = snapshot_row("b", None, UniversitySource::Moodle, "Manual");
    let rows = [
        moodle_row("a", "Original"),
        duplicate,
        moodle_row("b", "Original"),
        manual,
    ];
    let plan = plan_sync(&input, NotionSnapshot::Complete(&rows), &[]);
    assert_eq!(
        action(&plan, &key("a")),
        &PlanAction::Conflict(ConflictReason::DuplicateSyncKey)
    );
    assert_eq!(
        action(&plan, &key("b")),
        &PlanAction::Conflict(ConflictReason::InvalidOrRepeatedPage)
    );
    assert!(matches!(
        action(&plan, &key("c")),
        PlanAction::Create { .. }
    ));
}

#[test]
fn notion_defers_unverified_snapshots_and_missing_source_candidates() {
    let input = input(vec![candidate("a", "Title")]);
    let incomplete = plan_sync(&input, NotionSnapshot::Incomplete, &[]);
    assert_eq!(
        incomplete[0].action,
        PlanAction::Deferred(DeferredReason::IncompleteSnapshot)
    );
    let existing = [moodle_row("previous", "Keep")];
    let empty = SyncInput::from_acquisitions(None, None).unwrap();
    let plan = plan_sync(&empty, NotionSnapshot::Complete(&existing), &[]);
    assert_eq!(
        plan[0].action,
        PlanAction::Deferred(DeferredReason::NoValidatedCandidate)
    );
}

#[test]
fn notion_plans_relations_only_for_exact_unambiguous_mappings() {
    const MAPPED: &str = "00000000-0000-4000-8000-00000000000a";
    const OTHER: &str = "00000000-0000-4000-8000-00000000000b";
    let (input, course_key) = input_with_course(vec![candidate("a", "Title")]);
    let rows = [moodle_row("a", "Title")];
    let mut mappings = vec![CourseMapping {
        course_key: external(OriginKind::Moodle, "other-feed", "course-a"),
        page_id: OTHER.into(),
    }];
    let unmapped = plan_sync(&input, NotionSnapshot::Complete(&rows), &mappings);
    assert_eq!(
        unmapped[0].action,
        PlanAction::Unchanged {
            page_id: "a".into()
        }
    );
    let created = plan_sync(&input, NotionSnapshot::Complete(&[]), &mappings);
    assert!(matches!(
        created[0].action,
        PlanAction::Create {
            course_page_id: None,
            ..
        }
    ));

    mappings.extend(
        [MAPPED.to_owned(), MAPPED.replace('-', "").to_uppercase()].map(|page_id| CourseMapping {
            course_key: course_key.clone(),
            page_id,
        }),
    );
    let mapped = plan_sync(&input, NotionSnapshot::Complete(&rows), &mappings);
    assert_eq!(
        mapped[0].action,
        PlanAction::Update {
            page_id: "a".into(),
            patch: AssignmentPatch {
                course_page_id: Some(MAPPED.into()),
                ..AssignmentPatch::default()
            }
        }
    );
    let created = plan_sync(&input, NotionSnapshot::Complete(&[]), &mappings);
    assert!(
        matches!(&created[0].action, PlanAction::Create { course_page_id: Some(id), .. } if id == MAPPED)
    );

    for invalid_or_conflicting in [OTHER, "invalid"] {
        mappings.push(CourseMapping {
            course_key: course_key.clone(),
            page_id: invalid_or_conflicting.into(),
        });
        for snapshot in [&rows[..], &[]] {
            let ambiguous = plan_sync(&input, NotionSnapshot::Complete(snapshot), &mappings);
            assert_eq!(
                ambiguous[0].action,
                PlanAction::Conflict(ConflictReason::AmbiguousCourseMapping)
            );
            mappings.reverse();
            assert_eq!(
                plan_sync(&input, NotionSnapshot::Complete(snapshot), &mappings),
                ambiguous
            );
            mappings.reverse();
        }
        mappings.pop();
    }
}

#[test]
fn notion_preserves_populated_course_relations_and_diagnoses_mismatches() {
    const MAPPED: &str = "00000000-0000-4000-8000-00000000000a";
    const OTHER: &str = "00000000-0000-4000-8000-00000000000b";
    let (input, course_key) = input_with_course(vec![candidate("a", "Title")]);
    let mappings = [CourseMapping {
        course_key,
        page_id: MAPPED.into(),
    }];
    let mut manual = snapshot_row("manual", None, UniversitySource::Moodle, "Title");
    manual.course_page_ids = vec![OTHER.into()];

    for (links, mismatch) in [
        (&[MAPPED][..], false),
        (&[OTHER][..], true),
        (&[MAPPED, OTHER][..], true),
    ] {
        let mut row = moodle_row("a", "Title");
        row.course_page_ids = links.iter().map(|id| (*id).into()).collect();
        let rows = [row, manual.clone()];
        let plan = plan_sync(&input, NotionSnapshot::Complete(&rows), &mappings);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, key("a"));
        assert_eq!(
            plan[0].action,
            PlanAction::Unchanged {
                page_id: "a".into()
            }
        );
        assert_eq!(plan[0].course_relation_mismatch, mismatch, "{links:?}");
    }
}

#[test]
fn notion_rejects_invalid_acquisitions_before_planning() {
    let empty_title = moodle(vec![candidate("a", "  ")]);
    assert_eq!(
        SyncInput::from_acquisitions(Some(&empty_title), None).unwrap_err(),
        InvalidSourceData::EmptyTitle
    );
    let mut task = candidate("a", "Valid title");
    task.submission_url = Some("https://example.invalid/?secret=synthetic-private".into());
    let unknown_url = moodle(vec![task]);
    let error = SyncInput::from_acquisitions(Some(&unknown_url), None).unwrap_err();
    assert_eq!(error, InvalidSourceData::UnexpectedSubmissionUrl);
    assert!(!format!("{error:?} {error}").contains("synthetic-private"));
    assert_eq!(
        SyncKey::derive(&external(OriginKind::Manual, FEED, "a")),
        Err(InvalidSourceData::UnexpectedOrigin)
    );
    let malformed_groups = WebClassAcquisition::new(
        vec![attempt(
            &["a"],
            vec![group("b", &[("id", "Invalid mapping")])],
            Ok(()),
        )],
        Ok(()),
    );
    assert_eq!(
        SyncInput::from_acquisitions(None, Some(&malformed_groups)).unwrap_err(),
        InvalidSourceData::InvalidScope
    );
}

#[test]
fn notion_plan_is_deterministic_with_shuffled_source_and_snapshot_rows() {
    let tasks = vec![
        candidate("a", "New A"),
        candidate("b", "New B"),
        candidate("c", "New C"),
    ];
    let input1 = input(tasks.clone());
    let input2 = input(tasks.into_iter().rev().collect());
    let mut rows = vec![
        moodle_row("a", "Old A"),
        moodle_row("b", "New B"),
        moodle_row("d", "Existing"),
    ];
    let first = plan_sync(&input1, NotionSnapshot::Complete(&rows), &[]);
    rows.reverse();
    let reversed = plan_sync(&input2, NotionSnapshot::Complete(&rows), &[]);
    assert_eq!(first, reversed);
    assert!(first.windows(2).all(|pair| pair[0].key < pair[1].key));
}
