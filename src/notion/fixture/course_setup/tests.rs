use std::{collections::HashMap, path::Path};

use serde_json::{Value, json};

use super::*;
use crate::notion::fixture::{
    ASSIGNMENTS, ASSIGNMENTS_DB, COURSES, COURSES_DB, RULES, RULES_DB, SetupFault, created_page,
    fixture_id, fixture_page, rich, rule_page,
};
use crate::notion::{
    NotionClient,
    course_setup::VERIFICATION_RULES,
    state::{StateStore, test_state_dir},
    trial::{MOODLE_VERIFICATION_ASSIGNMENTS, MOODLE_VERIFICATION_COURSES},
};

fn apply(
    client: &NotionClient,
    input: &CourseSetupInput<'_>,
    directory: &Path,
) -> CourseSetupCounts {
    client
        .apply_course_setup(&setup_target(), input, directory, |_| true)
        .unwrap()
        .unwrap()
}

fn pages(client: &NotionClient, id: &str, db: &str) -> HashMap<String, Value> {
    client
        .query_data_source_pages(id, db, &[], |_, page| Ok(page.clone()))
        .unwrap()
}

fn manual_course(index: usize, name: &str, group: &str, term: &str) -> Value {
    fixture_page(
        index,
        COURSES,
        COURSES_DB,
        json!({
            "title":{"id":"title","type":"title","title":[{"type":"text","text":{"content":name}}]},
            "group":rich("group", group), "term":rich("term", term),
            "manual":{"id":"manual","type":"rich_text","rich_text":[{"type":"text","text":{"content":"keep me"}}]}
        }),
    )
}

#[test]
fn initializes_twelve_courses_and_six_categories_repeats_and_changes_term() {
    let (courses, mut events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let directory = test_state_dir();
    let mut assignment = created_page(&json!({}), 0);
    assignment["properties"]["status"] = json!({"type":"status","status":{"name":"Done"}});
    assignment["properties"]["memo"] = rich("memo", "user memo");
    assignment["body"] = json!(["user content"]);
    let (client, _server) = start_server_with_setup(
        true,
        vec![assignment.clone()],
        Some(SetupFixture::default()),
    )
    .unwrap();
    let plan = CourseSetupCounts {
        create_courses: 12,
        create_rules: 18,
        ..Default::default()
    };
    assert_eq!(
        client
            .preview_course_setup(&setup_target(), &input)
            .map(|preview| preview.counts),
        Ok(plan)
    );
    assert_eq!(
        client.apply_course_setup(&setup_target(), &input, directory.path(), |shown| {
            assert_eq!(shown, plan);
            false
        }),
        Ok(None)
    );
    assert!(pages(&client, COURSES, COURSES_DB).is_empty());
    assert!(!directory.path().join("course-setup.sqlite3").exists());

    assert_eq!(apply(&client, &input, directory.path()), plan);
    assert_eq!(
        apply(&client, &input, directory.path()),
        CourseSetupCounts {
            reuse_rules: 18,
            ..Default::default()
        }
    );
    let first_courses = pages(&client, COURSES, COURSES_DB);
    let first_rules = pages(&client, RULES, RULES_DB);
    assert_eq!(first_courses.len(), 12);
    assert_eq!(first_rules.len(), 18);
    for course in first_courses.values() {
        assert_eq!(course["properties"]["year"]["number"], 2026);
        assert_eq!(
            course["properties"]["term"]["rich_text"][0]["text"]["content"],
            "2026:2"
        );
        assert_eq!(
            course["properties"]["day"]["rich_text"][0]["text"]["content"],
            courses.records()[0].day_of_week
        );
        assert_eq!(
            course["properties"]["period"]["rich_text"][0]["text"]["content"],
            courses.records()[0].class_order
        );
    }
    let mut confirmed = 0;
    let mut unset = 0;
    for rule in first_rules.values() {
        let props = &rule["properties"];
        let links = props["course"]["relation"].as_array().unwrap();
        if props["source"]["select"]["id"] == "hoppii" {
            assert_eq!(props["status"]["select"]["id"], "confirmed");
            assert_eq!(links.len(), 1);
            let linked = &first_courses[links[0]["id"].as_str().unwrap()];
            assert_eq!(
                linked["properties"]["group"]["rich_text"],
                props["external"]["rich_text"]
            );
            confirmed += 1;
        } else {
            assert_eq!(props["source"]["select"]["id"], "moodle");
            assert_eq!(props["status"]["select"]["id"], "unset");
            assert!(links.is_empty());
            unset += 1;
        }
    }
    assert_eq!((confirmed, unset), (12, 6));
    let mut new_event = events[0].clone();
    new_event.categories = Ok(vec!["合成カテゴリ 新学期".into()]);
    events.push(new_event);
    let next = CourseSetupInput::new("2027:1", &courses, &events, &feed).unwrap();
    assert_eq!(
        apply(&client, &next, directory.path()),
        CourseSetupCounts {
            create_courses: 1,
            create_rules: 2,
            reuse_rules: 6,
            ..Default::default()
        }
    );
    let next_courses = pages(&client, COURSES, COURSES_DB);
    let next_rules = pages(&client, RULES, RULES_DB);
    assert_eq!((next_courses.len(), next_rules.len()), (13, 20));
    for (id, page) in first_courses {
        assert_eq!(next_courses[&id], page);
    }
    for (id, page) in first_rules {
        assert_eq!(next_rules[&id], page);
    }
    assert_eq!(
        pages(&client, ASSIGNMENTS, ASSIGNMENTS_DB)
            .into_values()
            .collect::<Vec<_>>(),
        [assignment]
    );
}

#[test]
fn reuses_exact_courses_holds_name_only_matches_and_protects_existing_rules() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let directory = test_state_dir();
    let existing = [
        manual_course(
            0x121,
            "ユーザーが変更した授業名",
            "setup-group-01",
            "2026:2",
        ),
        manual_course(0x822, "合成授業 02", "", "2026:2"),
        manual_course(0x123, "既に確定済み", "setup-group-03", "2026:2"),
    ];
    let confirmed = rule_page(0x30, "hoppii", "default", "setup-group-03", 3);
    let mut excluded = rule_page(0x31, "hoppii", "default", "setup-group-04", 3);
    excluded["properties"]["Status"]["select"]["id"] = json!("excluded");
    excluded["properties"]["Course"]["relation"] = json!([]);
    let (client, _server) = start_server_with_setup(
        true,
        vec![],
        Some(SetupFixture {
            courses: existing.to_vec(),
            rules: vec![confirmed.clone(), excluded.clone()],
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(
        apply(&client, &input, directory.path()),
        CourseSetupCounts {
            create_courses: 8,
            create_rules: 16,
            reuse_courses: 1,
            reuse_rules: 2,
            held: 1,
        }
    );
    let rules = pages(&client, RULES, RULES_DB);
    assert_eq!(rules[confirmed["id"].as_str().unwrap()], confirmed);
    assert_eq!(rules[excluded["id"].as_str().unwrap()], excluded);
    let confirmed_existing = rules
        .values()
        .find(|r| {
            r["properties"]["external"]["rich_text"][0]["text"]["content"] == "setup-group-01"
        })
        .unwrap();
    assert_eq!(
        confirmed_existing["properties"]["course"]["relation"][0]["id"],
        fixture_id(0x121)
    );
    let unresolved = rules
        .values()
        .find(|r| {
            r["properties"]["external"]["rich_text"][0]["text"]["content"] == "setup-group-02"
        })
        .unwrap();
    assert_eq!(unresolved["properties"]["status"]["select"]["id"], "unset");
    assert_eq!(unresolved["properties"]["course"]["relation"], json!([]));
    let courses = pages(&client, COURSES, COURSES_DB);
    for page in existing {
        assert_eq!(courses[page["id"].as_str().unwrap()], page);
    }
    assert_eq!(
        apply(&client, &input, directory.path()),
        CourseSetupCounts {
            reuse_rules: 18,
            held: 1,
            ..Default::default()
        }
    );
}

fn verification_target() -> NotionTarget {
    let mut target = setup_target();
    target.assignments_data_source_id = MOODLE_VERIFICATION_ASSIGNMENTS.into();
    target.courses_data_source_id = MOODLE_VERIFICATION_COURSES.into();
    target.course_rules.as_mut().unwrap().data_source_id = VERIFICATION_RULES.into();
    target
}

fn verified_existing_courses() -> Vec<Value> {
    (1..=6)
        .map(|index| {
            let mut page = manual_course(
                0x900 + index,
                &format!("検証用の既存授業 {index}"),
                &format!("setup-group-{index:02}"),
                "2026:2",
            );
            page["parent"]["data_source_id"] = json!(MOODLE_VERIFICATION_COURSES);
            page
        })
        .collect()
}

#[test]
fn verified_apply_reuses_six_courses_and_creates_only_the_approved_six_and_eighteen() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let target = verification_target();
    let directory = test_state_dir();
    let existing = verified_existing_courses();
    let mut assignment = created_page(
        &json!({"properties":{"course":{"relation":[{"id":existing[0]["id"]}]}}}),
        0,
    );
    assignment["parent"]["data_source_id"] = json!(MOODLE_VERIFICATION_ASSIGNMENTS);
    assignment["properties"]["status"] = json!({"type":"status","status":{"name":"Done"}});
    assignment["properties"]["memo"] = rich("memo", "hand written");
    let (reader, _server) = start_server_with_setup(
        true,
        vec![assignment.clone()],
        Some(SetupFixture {
            courses: existing.clone(),
            verification: true,
            ..Default::default()
        }),
    )
    .unwrap();
    let preview = reader
        .preview_verified_course_setup(&target, &input, directory.path())
        .unwrap();
    let expected = CourseSetupCounts {
        create_courses: 6,
        create_rules: 18,
        reuse_courses: 6,
        ..Default::default()
    };
    assert_eq!(preview.counts, expected);
    assert_eq!(preview.plan_id.len(), 64);
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );

    let mut changed_records = courses.records().to_vec();
    changed_records[6].group_name.push_str(" 改名");
    let changed_records = AcquiredWebClassCourses::from_records(changed_records).unwrap();
    let changed = CourseSetupInput::new("2026:2", &changed_records, &events, &feed).unwrap();
    assert_eq!(
        reader
            .preview_verified_course_setup(&target, &changed, directory.path())
            .unwrap()
            .counts,
        expected
    );
    assert_eq!(
        reader.apply_verified_course_setup(
            &reader,
            &target,
            &changed,
            directory.path(),
            &preview.plan_id
        ),
        Err(ApplyError::PlanChanged)
    );
    assert_eq!(
        pages(&reader, MOODLE_VERIFICATION_COURSES, COURSES_DB).len(),
        6
    );
    assert!(pages(&reader, VERIFICATION_RULES, RULES_DB).is_empty());
    assert!(!directory.path().join("course-setup.sqlite3").exists());

    assert_eq!(
        reader.apply_verified_course_setup(
            &reader,
            &target,
            &input,
            directory.path(),
            &preview.plan_id
        ),
        Ok(expected)
    );
    let all_courses = pages(&reader, MOODLE_VERIFICATION_COURSES, COURSES_DB);
    let all_rules = pages(&reader, VERIFICATION_RULES, RULES_DB);
    assert_eq!((all_courses.len(), all_rules.len()), (12, 18));
    for course in &existing {
        assert_eq!(all_courses[course["id"].as_str().unwrap()], *course);
    }
    let mut confirmed = 0;
    let mut unset = 0;
    for rule in all_rules.values() {
        let props = &rule["properties"];
        let links = props["course"]["relation"].as_array().unwrap();
        if props["source"]["select"]["id"] == "hoppii" {
            assert_eq!(props["status"]["select"]["id"], "confirmed");
            assert_eq!(links.len(), 1);
            assert_eq!(
                all_courses[links[0]["id"].as_str().unwrap()]["properties"]["group"]["rich_text"],
                props["external"]["rich_text"]
            );
            confirmed += 1;
        } else {
            assert_eq!(props["source"]["select"]["id"], "moodle");
            assert_eq!(props["status"]["select"]["id"], "unset");
            assert!(links.is_empty());
            unset += 1;
        }
    }
    assert_eq!((confirmed, unset), (12, 6));
    assert_eq!(
        pages(&reader, MOODLE_VERIFICATION_ASSIGNMENTS, ASSIGNMENTS_DB)
            [assignment["id"].as_str().unwrap()],
        assignment
    );
    assert_eq!(
        reader.apply_verified_course_setup(
            &reader,
            &target,
            &input,
            directory.path(),
            &preview.plan_id
        ),
        Err(ApplyError::PlanChanged)
    );
    let next = reader
        .preview_verified_course_setup(&target, &input, directory.path())
        .unwrap();
    assert_eq!(
        next.counts,
        CourseSetupCounts {
            reuse_rules: 18,
            ..Default::default()
        }
    );
    assert_eq!(
        reader.apply_verified_course_setup(
            &reader,
            &target,
            &input,
            directory.path(),
            &next.plan_id
        ),
        Ok(next.counts)
    );
    assert_eq!(pages(&reader, VERIFICATION_RULES, RULES_DB), all_rules);
}

#[test]
fn verified_apply_stops_when_a_rule_appears_after_both_preflight_reads() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let target = verification_target();
    let directory = test_state_dir();
    let mut external_rule = rule_page(0x80, "hoppii", "default", "setup-group-01", 1);
    external_rule["parent"]["data_source_id"] = json!(VERIFICATION_RULES);
    external_rule["properties"]["Status"]["select"]["id"] = json!("unset");
    external_rule["properties"]["Course"]["relation"] = json!([]);
    let (client, _server) = start_server_with_setup(
        true,
        vec![],
        Some(SetupFixture {
            verification: true,
            late_rule: Some(external_rule.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    let preview = client
        .preview_verified_course_setup(&target, &input, directory.path())
        .unwrap();
    assert_eq!(
        client.apply_verified_course_setup(
            &client,
            &target,
            &input,
            directory.path(),
            &preview.plan_id
        ),
        Err(ApplyError::PlanChanged)
    );
    assert!(pages(&client, MOODLE_VERIFICATION_COURSES, COURSES_DB).is_empty());
    assert_eq!(
        pages(&client, VERIFICATION_RULES, RULES_DB)
            .into_values()
            .collect::<Vec<_>>(),
        [external_rule]
    );
    let state = StateStore::read(
        &directory.path().join("course-setup.sqlite3"),
        VERIFICATION_RULES,
    )
    .unwrap()
    .unwrap();
    assert!(state.bindings.is_empty());
}

#[test]
fn verified_apply_recovers_an_unknown_rule_post_with_a_new_plan() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let target = verification_target();
    let directory = test_state_dir();
    let (client, _server) = start_server_with_setup(
        true,
        vec![],
        Some(SetupFixture {
            verification: true,
            fail_next: Some(SetupFault::DropRuleResponse),
            ..Default::default()
        }),
    )
    .unwrap();
    let first = client
        .preview_verified_course_setup(&target, &input, directory.path())
        .unwrap();
    assert!(
        client
            .apply_verified_course_setup(&client, &target, &input, directory.path(), &first.plan_id)
            .is_err()
    );
    let path = directory.path().join("course-setup.sqlite3");
    assert_eq!(
        StateStore::read(&path, VERIFICATION_RULES)
            .unwrap()
            .unwrap()
            .pending,
        1
    );
    assert_eq!(
        client.apply_verified_course_setup(
            &client,
            &target,
            &input,
            directory.path(),
            &first.plan_id
        ),
        Err(ApplyError::PlanChanged)
    );
    let next = client
        .preview_verified_course_setup(&target, &input, directory.path())
        .unwrap();
    assert_eq!(next.counts.create_rules, 17);
    assert_eq!(next.counts.reuse_rules, 1);
    client
        .apply_verified_course_setup(&client, &target, &input, directory.path(), &next.plan_id)
        .unwrap();
    assert_eq!(
        StateStore::read(&path, VERIFICATION_RULES)
            .unwrap()
            .unwrap()
            .pending,
        0
    );
    assert_eq!(
        pages(&client, MOODLE_VERIFICATION_COURSES, COURSES_DB).len(),
        12
    );
    assert_eq!(pages(&client, VERIFICATION_RULES, RULES_DB).len(), 18);
}

#[test]
fn verified_apply_rejects_a_different_workspace_bot_and_an_unapproved_target() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let directory = test_state_dir();
    let (wrong_bot, _server) =
        start_server_with_setup(true, vec![], Some(SetupFixture::default())).unwrap();
    let expected = "a".repeat(64);
    assert_eq!(
        wrong_bot.apply_verified_course_setup(
            &wrong_bot,
            &setup_target(),
            &input,
            directory.path(),
            &expected
        ),
        Err(ApplyError::Notion(NotionError::MethodDisabled))
    );
    assert_eq!(wrong_bot.read_counts(), (0, 0));
    assert_eq!(
        wrong_bot.apply_verified_course_setup(
            &wrong_bot,
            &verification_target(),
            &input,
            directory.path(),
            &expected
        ),
        Err(ApplyError::Notion(NotionError::CredentialMismatch))
    );
    assert_eq!(wrong_bot.read_counts(), (1, 0));
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn reconciles_partial_creates_and_holds_unknown_absence() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    for (fault, page_count, rule_count, pending) in [
        (SetupFault::RejectRule, 1, 0, 0),
        (SetupFault::DropCourseResponse, 1, 0, 1),
        (SetupFault::DropRuleResponse, 1, 1, 1),
        (SetupFault::DropCourseBeforeCommit, 0, 0, 1),
    ] {
        let directory = test_state_dir();
        let path = directory.path().join("course-setup.sqlite3");
        let (client, _server) = start_server_with_setup(
            true,
            vec![],
            Some(SetupFixture {
                fail_next: Some(fault),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(
            client
                .apply_course_setup(&setup_target(), &input, directory.path(), |_| true)
                .is_err()
        );
        let after_failure = pages(&client, COURSES, COURSES_DB);
        assert_eq!(after_failure.len(), page_count);
        assert_eq!(pages(&client, RULES, RULES_DB).len(), rule_count);
        assert_eq!(
            StateStore::read(&path, RULES).unwrap().unwrap().pending,
            pending
        );
        let recovery = apply(&client, &input, directory.path());
        let absent = matches!(fault, SetupFault::DropCourseBeforeCommit);
        assert_eq!(recovery.create_courses, 11);
        assert_eq!(recovery.create_rules, 18 - rule_count - usize::from(absent));
        assert_eq!(recovery.held, usize::from(absent));
        let recovered_courses = pages(&client, COURSES, COURSES_DB);
        assert_eq!(recovered_courses.len(), 12 - usize::from(absent));
        for (id, page) in after_failure {
            assert_eq!(recovered_courses[&id], page);
        }
        assert_eq!(
            pages(&client, RULES, RULES_DB).len(),
            18 - usize::from(absent)
        );
        assert_eq!(
            StateStore::read(&path, RULES).unwrap().unwrap().pending,
            usize::from(absent)
        );
        assert_eq!(apply(&client, &input, directory.path()).create_rules, 0);
    }
}

#[test]
fn official_notion_client_is_rejected_before_state_or_network_access() {
    let (courses, events, feed) = setup_sources().unwrap();
    let input = CourseSetupInput::new("2026:2", &courses, &events, &feed).unwrap();
    let directory = test_state_dir();
    let client = NotionClient::new("fixture-token").unwrap();
    assert_eq!(
        client.apply_course_setup(&setup_target(), &input, directory.path(), |_| panic!(
            "unexpected approval"
        )),
        Err(ApplyError::Notion(NotionError::MethodDisabled))
    );
    assert_eq!(client.read_counts(), (0, 0));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
