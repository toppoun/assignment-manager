use std::collections::HashSet;

mod recurring;

use super::*;
use crate::{
    import::{DEFAULT_COURSE_SOURCE_SCOPE, ExternalRecordKey, OriginKind},
    moodle::parse_calendar,
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::tests::{attempt, group, moodle},
    sync::{AcquiredWebClassCourses, SourceCourse, WebClassAcquisition},
};

#[test]
fn read_only_setup_reports_twelve_webclass_courses_and_six_legacy_moodle_candidates() {
    let ids: Vec<_> = (0..13).map(|index| format!("group-{index}")).collect();
    let mut records = attempt(
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![],
        Ok(()),
    )
    .course_list()
    .unwrap()
    .records()
    .to_vec();
    for (index, course) in records.iter_mut().enumerate() {
        course.group_name = format!("WebClass course {index}");
        course.day_of_week = "月".into();
        course.class_order = "2".into();
    }
    records[1].group_name = "データ処理 Ａ".into();
    records[12].semester = "1".into();
    let webclass = AcquiredWebClassCourses::from_records(records).unwrap();
    let categories = [
        "データ処理 A",
        "カテゴリ1",
        "カテゴリ2",
        "カテゴリ3",
        "カテゴリ4",
        "カテゴリ5",
    ];
    let mut ics = String::from("BEGIN:VCALENDAR\nVERSION:2.0\n");
    for (index, category) in categories.into_iter().chain([categories[0]]).enumerate() {
        use std::fmt::Write;
        writeln!(ics, "BEGIN:VEVENT\nUID:{index}@example.invalid\nSUMMARY:受験可能期間の開始\nDTSTART:20261101T000000Z\nCATEGORIES:{category}\nEND:VEVENT").unwrap();
    }
    ics.push_str("END:VCALENDAR\n");
    let events = parse_calendar(&ics).unwrap();
    let feed = MoodleFeed::new(
        FEED,
        MoodleExportKind::CourseRelated,
        "https://fixture.invalid/calendar",
    )
    .unwrap();
    let input = CourseSetupInput::new(TERM, &webclass, &events, &feed).unwrap();

    let mut target = rules_target();
    target.course_rules.as_mut().unwrap().setup = Some(CourseSetupProperties {
        group_id: "group-id".into(),
        year: None,
        day: None,
        period: None,
    });
    let mut script = schemas();
    script[1] = Exchange::json(
        "GET",
        format!("/v1/data_sources/{C}"),
        source(
            C,
            C_DB,
            json!({
                "授業名": {"id":"title","type":"title","title":{}},
                "学期": {"id":"course-term","type":"rich_text","rich_text":{}},
                "HOPPII group_id": {"id":"group-id","type":"rich_text","rich_text":{}}
            }),
        ),
    );
    let existing = |index, label, term, group_id| {
        let mut page = course(index, term);
        page["properties"]["授業名"] = json!({"id":"title","type":"title","title":[text(label)]});
        page["properties"]["HOPPII group_id"] = rich("group-id", group_id);
        page
    };
    let mut legacy: Vec<_> = categories
        .iter()
        .enumerate()
        .map(|(index, label)| existing(index + 1, label, "", ""))
        .collect();
    legacy.push(existing(0, "Renamed in Notion", TERM, "group-0"));
    script.extend([rules_query(vec![], None), courses_query(legacy)]);
    let (client, server) = mock(script);
    let preview = client.preview_course_setup(&target, &input).unwrap();
    assert_eq!(
        (preview.webclass_courses, preview.moodle_categories),
        (12, 6)
    );
    assert_eq!(preview.items.len(), 18);
    assert_eq!(preview.counts.create_courses, 10);
    assert_eq!(preview.counts.reuse_courses, 1);
    assert_eq!(preview.counts.held, 1);
    assert_eq!(preview.counts.create_rules, 18);
    assert_eq!(preview.existing_courses_without_group_id.len(), 6);

    let output = json!(preview);
    let exact = &output["items"][0];
    assert_eq!(exact["course_action"], "reuse");
    assert_eq!(exact["proposed_rule"]["course_page_id"], fake_id(0x100));
    assert_eq!(exact["day_of_week"], "月");
    let ambiguous = &output["items"][1];
    assert_eq!(ambiguous["course_action"], "hold");
    assert_eq!(ambiguous["proposed_rule"]["status"], "未設定");
    assert!(ambiguous["proposed_rule"]["course_page_id"].is_null());
    assert_eq!(
        ambiguous["existing_course_candidates"][0]["matching_moodle_categories"],
        json!(["データ処理 A"])
    );
    assert_eq!(
        ambiguous["other_source_name_matches"],
        json!(["データ処理 A"])
    );
    for moodle in output["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["source"] == "Moodle")
    {
        assert_eq!(moodle["course_action"], "none");
        assert_eq!(moodle["proposed_rule"]["status"], "未設定");
        assert!(moodle["proposed_rule"]["course_page_id"].is_null());
        assert_eq!(
            moodle["existing_course_candidates"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    server.join().unwrap();
}

const RULES: &str = "00000000-0000-4000-8000-00000000000c";
const RULES_DB: &str = "00000000-0000-4000-8000-00000000000d";
const TERM: &str = "2026:2";
const FEED: &str = "default.course-related";

#[test]
fn production_assignment_read_scope_requires_the_exact_triplet() {
    use crate::notion::trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES};

    let mut target = rules_target();
    target.assignments_data_source_id = PRODUCTION_ASSIGNMENTS.into();
    target.courses_data_source_id = PRODUCTION_COURSES.into();
    assert_eq!(
        live_assignment_read_target(&target, true),
        Err(NotionError::MethodDisabled)
    );
    target.course_rules.as_mut().unwrap().data_source_id =
        "42285934-7e84-8183-8e04-000ba36d1080".into();
    assert_eq!(
        live_assignment_read_target(&target, true).as_deref(),
        Ok(PRODUCTION_ASSIGNMENTS)
    );
    assert_eq!(
        live_assignment_read_target(&target, false),
        Err(NotionError::MethodDisabled)
    );
    assert!(matches!(
        CourseSetupVerification::new(&target),
        Err(NotionError::MethodDisabled)
    ));
    target.courses_data_source_id = C.into();
    assert_eq!(
        live_assignment_read_target(&target, true),
        Err(NotionError::MethodDisabled)
    );
}

#[test]
fn production_writer_diagnostics_identify_the_bot_and_each_data_source() {
    use crate::notion::{
        ProductionWriterCheckError,
        trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES, PRODUCTION_RULES},
    };

    let mut target = rules_target();
    target.assignments_data_source_id = PRODUCTION_ASSIGNMENTS.into();
    target.courses_data_source_id = PRODUCTION_COURSES.into();
    target.course_rules.as_mut().unwrap().data_source_id = PRODUCTION_RULES.into();
    let assignments = format!("/v1/data_sources/{PRODUCTION_ASSIGNMENTS}");
    let courses = format!("/v1/data_sources/{PRODUCTION_COURSES}");
    let bot = "/v1/users/me";
    let writer = fake_id(0x500);
    let not_found = |path: &str| Exchange::error("GET", path, 404, "");
    let (client, server) = mock(vec![
        not_found(bot),
        trial_tests::bot(&writer),
        not_found(&assignments),
        not_found(&courses),
        trial_tests::bot(&writer),
        Exchange::json("GET", &assignments, json!({"object":"data_source"})),
        not_found(&courses),
    ]);
    let check = || {
        client
            .production_assignment_writer_bot_id(&target)
            .unwrap_err()
    };
    let expected = |stage, endpoint: &str| ProductionWriterCheckError {
        stage,
        endpoint: Some(endpoint.into()),
        error: NotionError::NotFound,
    };
    assert_eq!(check(), vec![expected("bot_identity", bot)]);
    assert_eq!(
        check(),
        vec![
            expected("assignments_data_source", &assignments),
            expected("courses_data_source", &courses),
        ],
    );
    assert_eq!(check(), vec![expected("courses_data_source", &courses)]);
    server.join().unwrap();
}

#[test]
fn production_assignment_preview_reads_rules_and_holds_manual_duplicates() {
    use crate::notion::trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES};

    let production_rules = "42285934-7e84-8183-8e04-000ba36d1080";
    let destination = fake_id(0x100);
    let mut course_page = course(0, TERM);
    course_page["properties"]["授業名"] =
        json!({"id":"title","type":"title","title":[text("Synthetic course")]});
    let rule_pages = vec![
        rule(0, "moodle-option", FEED, "Moodle category", &destination),
        rule(
            1,
            "hoppii-option",
            DEFAULT_COURSE_SOURCE_SCOPE,
            "fixture-group",
            &destination,
        ),
    ];
    let rule_snapshot = || {
        let mut script = schemas();
        script.extend([
            rules_query(rule_pages.clone(), None),
            courses_query(vec![course_page.clone()]),
        ]);
        script
    };
    let mut script = rule_snapshot();
    script.extend(rule_snapshot());
    script.extend([
        query(vec![task(MANUAL, None, "Moodle task 0")], None),
        courses_query(vec![course_page]),
    ]);
    for exchange in &mut script {
        for (from, to) in [
            (A, PRODUCTION_ASSIGNMENTS),
            (C, PRODUCTION_COURSES),
            (RULES, production_rules),
        ] {
            exchange.path = exchange.path.replace(from, to);
            exchange.body = exchange.body.replace(from, to);
        }
    }
    let (client, server) = mock(script);
    let mut target = rules_target();
    target.assignments_data_source_id = PRODUCTION_ASSIGNMENTS.into();
    target.courses_data_source_id = PRODUCTION_COURSES.into();
    target.course_rules.as_mut().unwrap().data_source_id = production_rules.into();

    assert_eq!(
        client.live_webclass_scopes(&target, true).unwrap(),
        std::collections::BTreeMap::from([("fixture-group".into(), TERM.into())]),
    );
    let input = mixed_input(&[("fixture-group", "2")], &["Moodle category"]);
    let (inspection, plan, rules) = client
        .inspect_live_assignment_dry_run(&input, &target, &[], Default::default(), true)
        .unwrap();
    assert!(inspection.relation_target_verified);
    assert_eq!(inspection.snapshot.unwrap().manual, 1);
    assert_eq!(rules.len(), 2);
    assert_eq!(
        plan.counts_for(&input),
        DryRunCounts {
            create: 1,
            deferred: 1,
            ..Default::default()
        }
    );
    assert!(
        plan.plan()
            .iter()
            .any(|entry| entry.action == PlanAction::Deferred(DeferredReason::ManualCandidate))
    );
    server.join().unwrap();
}

#[test]
fn production_first_sync_creates_only_approved_rows_preserves_manual_and_repeats_without_writes() {
    use crate::notion::{
        state::{StateStore, test_state_dir},
        trial::{PRODUCTION_ASSIGNMENTS, PRODUCTION_COURSES, PRODUCTION_RULES},
    };

    let input = mixed_input(&[], &["Moodle category"; 34]);
    let candidates = input.assignments().len();
    let destination = fake_id(0x100);
    let writer_bot = fake_id(0x500);
    let mut target = rules_target();
    target.assignments_data_source_id = PRODUCTION_ASSIGNMENTS.into();
    target.courses_data_source_id = PRODUCTION_COURSES.into();
    target.course_rules.as_mut().unwrap().data_source_id = PRODUCTION_RULES.into();
    let mut manual = task(MANUAL, None, "");
    manual["properties"]["タイトル（改名済）"]["title"] = json!([]);
    manual["properties"]["状態"]["status"]["name"] = json!("完了");
    let schema_pair = || schemas().into_iter().take(2);
    let status_schema =
        || Exchange::json("GET", format!("/v1/data_sources/{A}"), inspection_schema());
    let manual_snapshot = |row| {
        let mut script: Vec<_> = schema_pair().collect();
        script.push(status_schema());
        script.push(query(vec![row], None).check(|_, body| {
            assert_eq!(
                body["filter"],
                json!({"property":"sync","rich_text":{"is_empty":true}})
            );
        }));
        script
    };
    let rules_snapshot = || {
        let mut script = schemas();
        script.extend([
            rules_query(
                vec![rule(
                    0,
                    "moodle-option",
                    FEED,
                    "Moodle category",
                    &destination,
                )],
                None,
            ),
            courses_query(vec![course(0, TERM)]),
        ]);
        script
    };
    let mut script = vec![trial_tests::bot(&writer_bot)];
    script.extend(schema_pair());
    script.extend(manual_snapshot(manual.clone()));
    script.extend(rules_snapshot());
    script.push(query(vec![manual.clone()], None));
    // A changed plan must stop before any state DB creation or Notion write.
    script.extend(manual_snapshot(manual.clone()));
    script.extend(schema_pair());
    script.extend(rules_snapshot());
    let mut possible_duplicate = manual.clone();
    possible_duplicate["id"] = json!(PAGE_C);
    possible_duplicate["properties"]["タイトル（改名済）"]["title"] =
        json!([text("Moodle task 0")]);
    script.push(query(vec![manual.clone(), possible_duplicate], None));
    script.extend(manual_snapshot(manual.clone()));
    script.extend(schema_pair());
    script.extend(rules_snapshot());
    script.push(query(vec![manual.clone()], None));
    let mut acquired: Vec<_> = input.assignments().iter().collect();
    acquired.sort_by_key(|(key, _)| *key);
    let mut rows = vec![manual.clone()];
    for (index, (key, source)) in acquired.into_iter().enumerate() {
        let mut row = task(&fake_id(0x600 + index), Some(key), &source.title);
        row["properties"]["授業"]["relation"] = json!([{"id":destination}]);
        row["properties"]["提出URL"]["url"] = Value::Null;
        rows.push(row.clone());
        script.push(query(vec![], None));
        script.push(Exchange::json("POST", "/v1/pages", row).check(|_, body| {
            assert_eq!(body["parent"]["data_source_id"], PRODUCTION_ASSIGNMENTS);
            let fields = body["properties"].as_object().unwrap();
            assert_eq!(fields.len(), 4);
            assert!(
                ["title", "sync", "orig", "R%3Ael"]
                    .iter()
                    .all(|id| fields.contains_key(*id))
            );
        }));
    }
    script.extend(manual_snapshot(manual.clone()));
    script.extend(rules_snapshot());
    script.push(status_schema());
    script.push(query(rows.clone(), None));
    script.extend(rules_snapshot());
    script.push(query(rows.clone(), None));
    script.extend(manual_snapshot(manual));
    script.extend(schema_pair());
    script.extend(rules_snapshot());
    script.push(query(rows, None));
    for exchange in &mut script {
        for (from, to) in [
            (A, PRODUCTION_ASSIGNMENTS),
            (C, PRODUCTION_COURSES),
            (RULES, PRODUCTION_RULES),
        ] {
            exchange.path = exchange.path.replace(from, to);
            exchange.body = exchange.body.replace(from, to);
        }
    }
    let (client, server) = mock(script);
    let writer = client
        .check_production_assignment_writer(&target, &writer_bot)
        .unwrap();
    let protected = client.protected_production_assignment(&target).unwrap();
    let directory = test_state_dir();
    let state_path = directory.path().join("state.sqlite3");
    let plan = client.dry_run(&input, &target, &[]);
    assert_eq!(plan.counts_for(&input).create, candidates);
    assert!(!state_path.exists());
    let apply = |plan| {
        client.apply_production_first_sync(
            &writer,
            &target,
            &input,
            plan,
            directory.path(),
            &protected,
        )
    };
    assert_eq!(apply(&plan), Err(ApplyError::PlanChanged));
    assert!(!state_path.exists());
    assert_eq!(apply(&plan).unwrap().counts.create, candidates);
    let report = client
        .verify_production_assignments(&target, &input, &protected)
        .unwrap();
    assert_eq!(report.matched, candidates);
    assert!(report.field_mismatches.is_empty());
    let mut repeat = client.dry_run(&input, &target, &[]);
    repeat.defer_unreconciled(
        &StateStore::read(&state_path, PRODUCTION_ASSIGNMENTS)
            .unwrap()
            .unwrap(),
    );
    assert!(repeat.requests().is_empty());
    assert_eq!(apply(&repeat).unwrap().counts.unchanged, candidates);
    server.join().unwrap();
}

fn rules_target() -> NotionTarget {
    let mut target = target();
    target.course_rules = Some(CourseRulesTarget {
        data_source_id: RULES.into(),
        course_term: "course-term".into(),
        source: "source".into(),
        scope: "scope".into(),
        external_id: "external".into(),
        term: "term".into(),
        course: "destination".into(),
        status: "status".into(),
        setup: None,
    });
    target
}

fn rules_schema() -> Value {
    source(
        RULES,
        RULES_DB,
        json!({
            "元の授業名":{"id":"title","type":"title","title":{}},
            "取得元":{"id":"source","type":"select","select":{"options":[
                {"id":"hoppii-option","name":"HOPPII"},{"id":"moodle-option","name":"Moodle"}
            ]}},
            "スコープ":{"id":"scope","type":"rich_text","rich_text":{}},
            "外部識別子":{"id":"external","type":"rich_text","rich_text":{}},
            "学期":{"id":"term","type":"rich_text","rich_text":{}},
            "対応する授業":{"id":"destination","type":"relation","relation":{
                "data_source_id":C,"database_id":C_DB,"type":"single_property"
            }},
            "設定状態":{"id":"status","type":"select","select":{"options":[
                {"id":"unset","name":"未設定"},{"id":"confirmed","name":"確定"},
                {"id":"excluded","name":"対象外"}
            ]}}
        }),
    )
}

fn schemas() -> Vec<Exchange> {
    vec![
        Exchange::json("GET", format!("/v1/data_sources/{A}"), inspection_schema()),
        Exchange::json(
            "GET",
            format!("/v1/data_sources/{C}"),
            source(
                C,
                C_DB,
                json!({
                    "学期":{"id":"course-term","type":"rich_text","rich_text":{}}
                }),
            ),
        ),
        Exchange::json("GET", format!("/v1/data_sources/{RULES}"), rules_schema()),
    ]
}

fn rules_query(rows: Vec<Value>, cursor: Option<&str>) -> Exchange {
    Exchange::json(
        "POST",
        format!("/v1/data_sources/{RULES}/query"),
        list(rows, cursor),
    )
}

fn courses_query(courses: Vec<Value>) -> Exchange {
    Exchange::json(
        "POST",
        format!("/v1/data_sources/{C}/query"),
        list(courses, None),
    )
}

fn rich(id: &str, value: &str) -> Value {
    json!({"id":id,"type":"rich_text","rich_text":[text(value)]})
}

fn fake_id(index: usize) -> String {
    format!("00000000-0000-4000-8000-{index:012x}")
}

fn course(index: usize, term: &str) -> Value {
    page(
        &fake_id(0x100 + index),
        C,
        C_DB,
        json!({"学期":rich("course-term", term)}),
    )
}

fn rule(index: usize, source: &str, scope: &str, external_id: &str, to: &str) -> Value {
    page(
        &fake_id(0x200 + index),
        RULES,
        RULES_DB,
        json!({
            "元の授業名":{"id":"title","type":"title","title":[text("Display name unrelated to identity")]},
            "取得元":{"id":"source","type":"select","select":{"id":source}},
            "スコープ":rich("scope",scope),
            "外部識別子":rich("external",external_id),
            "学期":rich("term",TERM),
            "対応する授業":{"id":"destination","type":"relation","relation":[{"id":to}],"has_more":false},
            "設定状態":{"id":"status","type":"select","select":{"id":"confirmed"}}
        }),
    )
}

#[test]
fn verification_apply_creates_once_and_reads_back_the_same_input() {
    use crate::notion::{
        course_setup::VERIFICATION_RULES,
        state::{StateStore, test_state_dir},
        trial::{
            MOODLE_VERIFICATION_ASSIGNMENTS as ASSIGNMENTS, MOODLE_VERIFICATION_COURSES as COURSES,
            MOODLE_VERIFICATION_WRITER,
        },
    };

    let input = mixed_input(&[], &["Moodle course"]);
    let key = input.assignments().keys().next().unwrap().clone();
    let destination = fake_id(0x100);
    let mut created = task(PAGE_A, Some(&key), "Moodle task 0");
    created["properties"]["授業"]["relation"] = json!([{"id": destination}]);
    created["properties"]["提出URL"]["url"] = Value::Null;
    let mut edited = created.clone();
    edited["properties"]["メモ"]["rich_text"] = json!([text("Edited in Notion")]);
    edited["properties"]["状態"]["status"]["name"] = json!("In progress");
    let after_window = mixed_input(&[], &[]);
    let mut updated = mixed_input(&[], &["Moodle course"]);
    let changed = updated.assignments.get_mut(&key).unwrap();
    changed.title = "Updated title".into();
    changed.deadline = Some(Deadline::DateTime("2026-12-01T14:59:59Z".parse().unwrap()));
    let mut changed_page = edited.clone();
    changed_page["properties"]["タイトル（改名済）"]["title"] = json!([text("Updated title")]);
    changed_page["properties"]["期限"]["date"] = json!({"start":"2026-12-01T14:59:00Z"});
    let mut with_new = mixed_input(&[], &["Moodle course", "Moodle course"]);
    with_new.assignments.insert(key.clone(), changed.clone());
    let new_key = with_new
        .assignments()
        .keys()
        .find(|candidate| *candidate != &key)
        .unwrap();
    let mut created_b = task(PAGE_B, Some(new_key), "Moodle task 1");
    created_b["properties"]["授業"]["relation"] = json!([{"id": destination}]);
    let baseline = |rows| {
        let mut script = schemas();
        script.extend([
            rules_query(
                vec![rule(
                    0,
                    "moodle-option",
                    FEED,
                    "Moodle course",
                    &destination,
                )],
                None,
            ),
            courses_query(vec![course(0, TERM)]),
            query(rows, None),
        ]);
        script
    };
    let apply_reads = |rows| {
        let mut script = vec![trial_tests::bot(MOODLE_VERIFICATION_WRITER)];
        script.extend(schemas().into_iter().take(2));
        script.extend(baseline(rows));
        script
    };
    let mut script = baseline(vec![]);
    script.extend(apply_reads(vec![]));
    script.push(query(vec![], None));
    script.push(
        Exchange::json("POST", "/v1/pages", created.clone()).check(|_, body| {
            assert_eq!(body["parent"]["data_source_id"], ASSIGNMENTS);
            let props = body["properties"].as_object().unwrap();
            assert_eq!(props.len(), 4);
            assert!(!props.contains_key("url"));
            assert!(!props.contains_key("private"));
        }),
    );
    let mut readback = baseline(vec![created.clone()]);
    readback.insert(
        readback.len() - 1,
        Exchange::error("POST", format!("/v1/data_sources/{A}/query"), 400, ""),
    );
    readback.insert(
        readback.len() - 2,
        Exchange::json("GET", format!("/v1/data_sources/{A}"), inspection_schema()),
    );
    script.extend(readback);
    script.extend(baseline(vec![created.clone()]));
    script.extend(apply_reads(vec![created.clone()]));
    script.extend(apply_reads(vec![created.clone()]));
    script.extend(baseline(vec![edited.clone()]));
    script.extend(baseline(vec![edited.clone()]));
    script.extend(baseline(vec![edited.clone()]));
    script.extend(apply_reads(vec![edited.clone()]));
    let task_path = format!("/v1/pages/{PAGE_A}");
    script.push(Exchange::json("GET", &task_path, edited));
    script.push(Exchange::drop_response("PATCH", &task_path).check(|_, body| {
        assert_eq!(
            body["properties"],
            json!({"title":{"title":[text("Updated title")]}, "due":{"date":{"start":"2026-12-01T14:59:59Z"}}})
        );
    }));
    script.push(Exchange::json("GET", &task_path, changed_page.clone()));
    script.extend(baseline(vec![changed_page.clone()]));
    script.extend(apply_reads(vec![changed_page.clone()]));
    script.push(query(vec![], None));
    script.push(Exchange::drop_response("POST", "/v1/pages"));
    script.extend(baseline(vec![changed_page.clone()]));
    script.extend(apply_reads(vec![changed_page.clone()]));
    let mut duplicate = created_b.clone();
    duplicate["id"] = json!(PAGE_C);
    script.extend(baseline(vec![
        changed_page.clone(),
        created_b.clone(),
        duplicate.clone(),
    ]));
    script.extend(apply_reads(vec![
        changed_page.clone(),
        created_b.clone(),
        duplicate,
    ]));
    let mut failed_read = apply_reads(vec![]);
    failed_read.pop().unwrap();
    failed_read.push(Exchange::error(
        "POST",
        format!("/v1/data_sources/{A}/query"),
        403,
        "",
    ));
    script.extend(failed_read);
    script.extend(baseline(vec![changed_page.clone(), created_b.clone()]));
    script.extend(apply_reads(vec![changed_page, created_b]));
    for exchange in &mut script {
        if exchange.method == "GET" && exchange.path == format!("/v1/data_sources/{C}") {
            let mut schema: Value = serde_json::from_str(&exchange.body).unwrap();
            schema["properties"]["授業名"] = json!({"id":"title","type":"title","title":{}});
            schema["properties"]["HOPPII group_id"] =
                json!({"id":"group","type":"rich_text","rich_text":{}});
            exchange.body = schema.to_string();
        }
        for (from, to) in [(A, ASSIGNMENTS), (C, COURSES), (RULES, VERIFICATION_RULES)] {
            exchange.path = exchange.path.replace(from, to);
            exchange.body = exchange.body.replace(from, to);
        }
    }
    let (client, server) = mock(script);
    let mut target = rules_target();
    target.assignments_data_source_id = ASSIGNMENTS.into();
    target.courses_data_source_id = COURSES.into();
    let rules = target.course_rules.as_mut().unwrap();
    rules.data_source_id = VERIFICATION_RULES.into();
    rules.setup = Some(CourseSetupProperties {
        group_id: "group".into(),
        year: None,
        day: None,
        period: None,
    });
    let directory = test_state_dir();
    let plan = client.dry_run(&input, &target, &[]);
    assert_eq!(
        plan.counts_for(&input).create,
        1,
        "{plan} {:?}",
        plan.plan()
    );
    let apply =
        |plan| client.apply_verified_assignments(&client, &target, &input, plan, directory.path());
    assert_eq!(apply(&plan).unwrap().counts.create, 1);
    let verified = client.verify_synced_assignments(&target, &input).unwrap();
    assert_eq!(verified.matched, 1);
    assert_eq!(verified.urls_present, 0);
    assert_eq!(verified.statuses.get("Done"), Some(&1));
    assert!(verified.field_mismatches.is_empty());
    let repeat = client.dry_run(&input, &target, &[]);
    assert_eq!(repeat.counts_for(&input).unchanged, 1);
    assert_eq!(apply(&repeat).unwrap().counts.unchanged, 1);
    assert_eq!(apply(&plan), Err(ApplyError::PlanChanged));

    let outside = client.dry_run(&after_window, &target, &[]);
    assert!(outside.requests().is_empty());
    assert_eq!(
        outside.plan()[0].action,
        PlanAction::Deferred(DeferredReason::NoValidatedCandidate)
    );
    let manual_edit = client.dry_run(&input, &target, &[]);
    assert_eq!(manual_edit.counts_for(&input).unchanged, 1);
    assert!(manual_edit.requests().is_empty());

    let changed = client.dry_run(&updated, &target, &[]);
    assert_eq!(changed.counts_for(&updated).update, 1);
    assert_eq!(
        changed.requests()[0].body()["properties"],
        json!({
            "title": {"title": [text("Updated title")]},
            "due": {"date": {"start": "2026-12-01T14:59:59Z"}}
        })
    );
    let apply_input = |input, plan| {
        client.apply_verified_assignments(&client, &target, input, plan, directory.path())
    };
    assert_eq!(apply_input(&updated, &changed).unwrap().counts.update, 1);

    let addition = client.dry_run(&with_new, &target, &[]);
    assert_eq!(addition.counts_for(&with_new).create, 1);
    let interrupted = apply_input(&with_new, &addition).unwrap();
    assert_eq!(interrupted.error, Some(NotionError::Transport));
    assert_eq!(interrupted.pending, 1);
    let state_path = directory.path().join("state.sqlite3");
    let read_state = || StateStore::read(&state_path, ASSIGNMENTS).unwrap().unwrap();
    assert_eq!(read_state().pending, 1);
    let absent = client.dry_run(&with_new, &target, &[]);
    assert_eq!(
        apply_input(&with_new, &absent),
        Err(ApplyError::PlanChanged)
    );
    let ambiguous = client.dry_run(&with_new, &target, &[]);
    assert_eq!(ambiguous.counts_for(&with_new).conflict, 1);
    assert_eq!(
        apply_input(&with_new, &ambiguous),
        Err(ApplyError::PlanChanged)
    );
    assert_eq!(
        apply_input(&with_new, &ambiguous),
        Err(ApplyError::Notion(NotionError::Forbidden))
    );
    assert_eq!(read_state().pending, 1);
    let retry = client.dry_run(&with_new, &target, &[]);
    let recovered = apply_input(&with_new, &retry).unwrap();
    assert_eq!(recovered.recovered, 1);
    assert_eq!(recovered.counts.unchanged, 2);
    assert_eq!(read_state().pending, 0);
    server.join().unwrap();
}

fn mixed_input(groups: &[(&str, &str)], categories: &[&str]) -> SyncInput {
    let group_ids: Vec<_> = groups.iter().map(|(id, _)| *id).collect();
    let mut webclass = attempt(
        &group_ids,
        groups
            .iter()
            .map(|(id, _)| group(id, &[("task", "WebClass task")]))
            .collect(),
        Ok(()),
    );
    let mut records = webclass.course_list().unwrap().records().to_vec();
    for (record, (_, semester)) in records.iter_mut().zip(groups) {
        record.semester = (*semester).into();
    }
    webclass.course_list = Some(AcquiredWebClassCourses::from_records(records).unwrap());
    let webclass = WebClassAcquisition::new(vec![webclass], Ok(()));
    let mut moodle = moodle(
        categories
            .iter()
            .enumerate()
            .map(|(index, _)| {
                candidate(&format!("moodle-{index}"), &format!("Moodle task {index}"))
            })
            .collect(),
    );
    for (candidate, category) in moodle.assignments.batch().as_slice().iter().zip(categories) {
        moodle.courses.insert(
            candidate.external_key.clone(),
            SourceCourse {
                name: "A display label".into(),
                external_key: ExternalRecordKey {
                    origin: OriginKind::Moodle,
                    scope: FEED.into(),
                    id: (*category).into(),
                },
            },
        );
    }
    SyncInput::from_acquisitions(Some(&moodle), Some(&webclass)).unwrap()
}

#[test]
fn eighteen_paginated_rules_route_to_twelve_courses_by_exact_external_keys() {
    let group_ids: Vec<_> = (0..12).map(|index| format!("group-{index}")).collect();
    let groups: Vec<_> = group_ids.iter().map(|id| (id.as_str(), "2")).collect();
    let categories = [" 集合 A ", "集合 A", "集合 B", "集合 C", "集合 D", "集合 E"];
    let input = mixed_input(&groups, &categories);
    let group_rules = group_ids.iter().enumerate().map(|(index, group)| {
        rule(
            index,
            "hoppii-option",
            DEFAULT_COURSE_SOURCE_SCOPE,
            group,
            &fake_id(0x100 + index),
        )
    });
    let moodle_rules = categories.iter().enumerate().map(|(index, category)| {
        rule(
            12 + index,
            "moodle-option",
            FEED,
            category,
            &fake_id(0x100 + index / 2),
        )
    });
    let mut rows: Vec<_> = group_rules.chain(moodle_rules).collect();
    let rest = rows.split_off(10);
    let mut script = schemas();
    script.extend([
        rules_query(rows, Some("course-rule-cursor")).check(|url, body| {
            assert!(url.contains("filter_properties=external"));
            assert!(body.get("start_cursor").is_none());
        }),
        rules_query(rest, None)
            .check(|_, body| assert_eq!(body["start_cursor"], "course-rule-cursor")),
        courses_query((0..12).map(|index| course(index, TERM)).collect()),
        query(vec![], None),
    ]);
    let (client, server) = mock(script);
    let legacy = CourseMapping {
        course_key: input.assignments()[&key("moodle-0")]
            .course_key
            .clone()
            .unwrap(),
        page_id: "irrelevant legacy value".into(),
    };
    let (inspection, report) = client.inspect_dry_run(&input, &rules_target(), &[], &[legacy]);
    assert_eq!(inspection.error, None);
    assert!(
        inspection
            .to_string()
            .contains("course_rules=complete rows=18 confirmed=18")
    );
    assert_eq!(
        report.counts(),
        DryRunCounts {
            create: 18,
            ..Default::default()
        }
    );
    let destinations: HashSet<_> = report
        .requests()
        .iter()
        .map(|request| {
            let properties = &request.body()["properties"];
            assert_eq!(
                properties["sync"]["rich_text"][0]["text"]["content"],
                request.key().as_str()
            );
            properties["R%3Ael"]["relation"][0]["id"].as_str().unwrap()
        })
        .collect();
    assert_eq!(destinations.len(), 12);
    for index in 0..categories.len() {
        let uid = key(&format!("moodle-{index}"));
        let request = report
            .requests()
            .iter()
            .find(|request| request.key() == &uid)
            .unwrap();
        assert_eq!(
            request.body()["properties"]["R%3Ael"]["relation"][0]["id"],
            fake_id(0x100 + index / 2)
        );
    }
    server.join().unwrap();
}

#[test]
fn strict_rules_defer_affected_tasks_and_preserve_user_managed_fields() {
    let input = mixed_input(
        &[("wrong-term", "1"), ("unknown-term", "")],
        &[
            "Update",
            "Mismatch",
            "Unset",
            "Duplicate",
            "Excluded",
            "Bad relation",
            "Bad term",
            "Exact scope",
            "A label ",
            "Other target",
            "Multi relation",
            "Manual",
        ],
    );
    let target_page = fake_id(0x100);
    let other_term_page = fake_id(0x101);
    let rows = |index, external| rule(index, "moodle-option", FEED, external, &target_page);
    let mut unset = rows(2, "Unset");
    unset["properties"]["設定状態"]["select"]["id"] = json!("unset");
    let mut duplicate_in_another_term = rows(3, "Duplicate");
    duplicate_in_another_term["properties"]["学期"] = rich("term", "2025:2");
    let mut excluded = rows(4, "Excluded");
    excluded["properties"]["設定状態"]["select"]["id"] = json!("excluded");
    let mut invalid_term = rows(6, "Bad term");
    invalid_term["properties"]["学期"] = rich("term", "");
    let mut empty_relation = rows(5, "Bad relation");
    empty_relation["properties"]["対応する授業"]["relation"] = json!([]);
    let mut multi_relation = rows(13, "Multi relation");
    multi_relation["properties"]["対応する授業"]["has_more"] = json!(true);
    let mut mismatch = task(PAGE_B, Some(&key("moodle-1")), "Old mismatch");
    mismatch["properties"]["授業"]["relation"] = json!([{"id":OLD_COURSE}]);
    let mut done = task(PAGE_A, Some(&key("moodle-0")), "Old title");
    done["properties"]["授業"]["relation"] = json!([{"id":target_page}]);

    let mut script = schemas();
    script.extend([
        rules_query(
            vec![
                rows(0, "Update"),
                rows(1, "Mismatch"),
                unset,
                rows(12, "Duplicate"),
                duplicate_in_another_term,
                excluded,
                empty_relation,
                invalid_term,
                rule(
                    7,
                    "moodle-option",
                    "other.course-related",
                    "Exact scope",
                    &target_page,
                ),
                rows(8, "A label"),
                rule(9, "moodle-option", FEED, "Other target", &other_term_page),
                multi_relation,
                rows(14, "Manual"),
                rule(
                    10,
                    "hoppii-option",
                    DEFAULT_COURSE_SOURCE_SCOPE,
                    "wrong-term",
                    &target_page,
                ),
                rule(
                    11,
                    "hoppii-option",
                    DEFAULT_COURSE_SOURCE_SCOPE,
                    "unknown-term",
                    &target_page,
                ),
            ],
            None,
        ),
        courses_query(vec![course(0, TERM), course(1, "2025:2")]),
        query(
            vec![done, mismatch, task(MANUAL, None, "Moodle task 11")],
            None,
        ),
    ]);
    let (client, server) = mock(script);
    let (inspection, report) = client.inspect_dry_run(&input, &rules_target(), &[], &[]);
    assert_eq!(inspection.error, None);
    let summary = inspection.course_rules.unwrap();
    assert!(summary.has_issues());
    assert_eq!(summary.rows, 15);
    assert_eq!(summary.duplicate_keys, 1);
    assert_eq!(summary.invalid_relations, 2);
    assert_eq!(summary.unverified_terms, 2);
    assert_eq!(
        report.counts(),
        DryRunCounts {
            update: 1,
            deferred: 12,
            course_relation_mismatch: 1,
            course_rules: CoursePlanCounts {
                unconfigured: 3,
                duplicates: 1,
                invalid_relation: 2,
                unverified_term: 4,
                excluded: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    );
    assert_eq!(report.requests().len(), 1);
    let update = &report.requests()[0];
    assert_eq!(update.key(), &key("moodle-0"));
    assert_eq!(
        update.body(),
        &json!({"properties":{"title":{"title":[text("Moodle task 0")]}}})
    );
    assert!(
        report
            .plan()
            .iter()
            .any(|item| item.action == PlanAction::Deferred(DeferredReason::ManualCandidate))
    );
    server.join().unwrap();
}

#[test]
fn malformed_or_incomplete_rule_reads_yield_no_assignment_plan_writes() {
    let (input, _) = input_with_course(vec![candidate("a", "Task")]);
    let good = rule(0, "moodle-option", FEED, "course-a", COURSE);
    let mut wrong_relation = rules_schema();
    wrong_relation["properties"]["対応する授業"]["relation"]["data_source_id"] = json!(A);
    let mut capped = list(vec![], None);
    capped["request_status"] = json!({"type":"incomplete"});
    for (tail, expected) in [
        (
            vec![Exchange::json(
                "GET",
                format!("/v1/data_sources/{RULES}"),
                wrong_relation,
            )],
            NotionError::SchemaMismatch,
        ),
        (
            vec![
                schemas().pop().unwrap(),
                rules_query(vec![good.clone()], Some("next")),
                Exchange::error("POST", format!("/v1/data_sources/{RULES}/query"), 403, ""),
            ],
            NotionError::Forbidden,
        ),
        (
            vec![
                schemas().pop().unwrap(),
                rules_query(vec![good.clone()], Some("next")),
                Exchange::json("POST", format!("/v1/data_sources/{RULES}/query"), capped),
            ],
            NotionError::IncompleteSnapshot,
        ),
        (
            vec![
                schemas().pop().unwrap(),
                rules_query(vec![good.clone()], Some("next")),
                rules_query(vec![good.clone()], None),
            ],
            NotionError::IncompleteSnapshot,
        ),
    ] {
        let mut script = schemas();
        script.pop();
        script.extend(tail);
        let (client, server) = mock(script);
        let (inspection, report) = client.inspect_dry_run(&input, &rules_target(), &[], &[]);
        assert_eq!(inspection.error, Some(expected));
        assert!(inspection.to_string().contains("course_rules=unavailable"));
        assert_eq!(report.error(), Some(expected));
        assert_eq!(report.counts().deferred, 1);
        assert!(report.requests().is_empty());
        server.join().unwrap();
    }
}
