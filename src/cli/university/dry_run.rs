use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, IsTerminal, Write},
    path::Path,
};

use assignment_manager::{
    import::{ExternalRecordKey, OriginKind},
    notion::{
        ApplyError, ApplyReport, CourseSetupVerification, DeferredReason, DryRunCounts,
        DryRunReport, NotionClient, NotionTarget, PlanAction, ProductionAssignmentWriter,
        ProtectedManualAssignment, SyncInput, UniversitySource, VerificationCourseRule,
        live_assignment_read_target, state::StateStore,
    },
    observation::SyncScope,
    sync::acquire_moodle_with_diagnostic,
    webclass_browser::{self, ProfileMode},
};
use serde::Serialize;
use serde_json::json;

use super::{prepare_moodle_request, secured_moodle_policy};
use crate::cli::{
    CliError, Config, CredentialStatus, config, credentials, display_inspection,
    display_production_writer_errors, notion_diagnosis,
};

#[derive(Clone, Copy)]
pub(in crate::cli) enum Mode<'a> {
    VerificationDryRun,
    ProductionDryRun,
    VerificationApply { expected: (usize, usize, usize) },
    ProductionFirstSync { writer: &'a str, create: usize },
}

enum SyncWriter<'a> {
    Verification(&'a NotionClient),
    Production(ProductionAssignmentWriter<'a>, ProtectedManualAssignment),
}

impl SyncWriter<'_> {
    fn apply(
        &self,
        reader: &NotionClient,
        target: &NotionTarget,
        input: &SyncInput,
        plan: &DryRunReport,
        directory: &Path,
    ) -> Result<ApplyReport, ApplyError> {
        match self {
            Self::Verification(writer) => {
                reader.apply_verified_assignments(writer, target, input, plan, directory)
            }
            Self::Production(writer, protected) => reader
                .apply_production_first_sync(writer, target, input, plan, directory, protected),
        }
    }
}

pub(in crate::cli) fn execute(local: &Config, path: &Path, mode: Mode<'_>) -> Result<u8, CliError> {
    let production = matches!(
        mode,
        Mode::ProductionDryRun | Mode::ProductionFirstSync { .. }
    );
    let interactive = matches!(mode, Mode::ProductionFirstSync { .. });
    if interactive && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        println!("result=stopped mode=production-first-sync issue=terminal_required");
        return Ok(3);
    }
    let assignments_id = match live_assignment_read_target(&local.notion, production) {
        Ok(assignments_id) => assignments_id,
        Err(error) => {
            let scope = if production {
                "production"
            } else {
                "verification"
            };
            println!("{scope}_target diagnostic={}", notion_diagnosis(error));
            return Ok(2);
        }
    };
    if !local.webclass {
        println!("webclass=disabled");
        return Err(CliError::Credential(CredentialStatus::Disabled));
    }
    let moodle = local.moodle.as_ref().ok_or_else(|| {
        println!("moodle=disabled");
        CliError::Credential(CredentialStatus::Disabled)
    })?;
    let Some(policy) = secured_moodle_policy(
        moodle,
        path.parent().unwrap_or_else(|| Path::new(".")),
        false,
    )?
    else {
        return Ok(1);
    };
    let client = credentials::notion_client(&assignments_id).map_err(CliError::Credential)?;
    let writer = match mode {
        Mode::VerificationApply { .. } => {
            let verification = CourseSetupVerification::new(&local.notion)
                .map_err(|_| crate::cli::ConfigError::Invalid)?;
            Some(credentials::course_setup_writer(&verification).map_err(CliError::Credential)?)
        }
        Mode::ProductionFirstSync { .. } => {
            Some(credentials::production_writer(&assignments_id).map_err(CliError::Credential)?)
        }
        _ => None,
    };
    let apply = match (mode, writer.as_ref()) {
        (Mode::VerificationApply { expected }, Some(writer)) => {
            if let Err(error) = writer.check_verification_assignment_writer(&local.notion) {
                println!("writer=failed diagnostic={}", notion_diagnosis(error));
                return Ok(2);
            }
            Some((SyncWriter::Verification(writer), expected))
        }
        (
            Mode::ProductionFirstSync {
                writer: bot,
                create,
            },
            Some(writer),
        ) => {
            let writer = match writer.check_production_assignment_writer(&local.notion, bot) {
                Ok(writer) => writer,
                Err(errors) => {
                    display_production_writer_errors(&errors);
                    return Ok(2);
                }
            };
            let protected = match client.protected_production_assignment(&local.notion) {
                Ok(protected) => protected,
                Err(error) => {
                    println!(
                        "protected_manual_assignment=failed diagnostic={}",
                        notion_diagnosis(error)
                    );
                    return Ok(2);
                }
            };
            println!(
                "production_writer=verified scope=assignment-creates protected_manual_assignments=1"
            );
            Some((SyncWriter::Production(writer, protected), (create, 0, 0)))
        }
        _ => None,
    };
    let (mut clients, feed) = prepare_moodle_request(local, moodle, policy)?;
    let directory = config::root()?.join("targets").join(&assignments_id);
    let state_path = directory.join("state.sqlite3");
    let requested = match client.live_webclass_scopes(&local.notion, production) {
        Ok(scopes) => scopes,
        Err(error) => {
            println!("course_rules diagnostic={}", notion_diagnosis(error));
            return Ok(1);
        }
    };
    if production {
        println!(
            "target=production mode={}",
            if interactive {
                "first-sync-preview"
            } else {
                "read-only"
            }
        );
    }

    clients.begin_run();
    let moodle = match clients.moodle() {
        Ok(client) => match acquire_moodle_with_diagnostic(&client, &feed) {
            Ok(acquired) => {
                println!(
                    "moodle=success candidates={}",
                    acquired.assignments().batch().as_slice().len()
                );
                Some(acquired)
            }
            Err(error) => {
                println!(
                    "moodle=failed diagnostic={} http_status={}",
                    error.diagnostic.code(),
                    error.diagnostic.http_status().unwrap_or(0)
                );
                None
            }
        },
        Err(_) => {
            println!("moodle=failed diagnostic=client_initialization");
            None
        }
    };
    let webclass = if requested.is_empty() {
        println!("webclass=not_requested confirmed_groups=0");
        None
    } else if let Err(reason) = webclass_browser::check_local_dependencies() {
        println!("webclass=failed diagnostic={reason}");
        None
    } else {
        println!("Chromeの大学公式画面で認証し、HOPPIIのWebClassリンクを開いてください。");
        let check = webclass_browser::run_for_confirmed_courses(ProfileMode::Temporary, &requested);
        println!(
            "webclass_fetch confirmed_groups={} courses_get={} contents_get={}",
            requested.len(),
            check.courses_get,
            check.contents_get
        );
        match check.result {
            Ok(acquired) => Some(acquired),
            Err(reason) => {
                println!("webclass=failed diagnostic={reason}");
                None
            }
        }
    };
    let input = match SyncInput::from_acquisitions(moodle.as_ref(), webclass.as_ref()) {
        Ok(input) => input,
        Err(error) => {
            println!("source_input=invalid diagnostic={error}");
            return Ok(1);
        }
    };
    let state = StateStore::read(&state_path, &assignments_id)?.unwrap_or_default();
    let (inspection, plan, rules) = match client.inspect_live_assignment_dry_run(
        &input,
        &local.notion,
        &local.sample_assignment_page_ids,
        state,
        production,
    ) {
        Ok(result) => result,
        Err(error) => {
            println!("notion=failed diagnostic={}", notion_diagnosis(error));
            return Ok(1);
        }
    };
    display_inspection(&inspection);
    println!(
        "recoverable_creates={} unresolved_creates={}",
        inspection.recoverable_creates, inspection.unresolved_creates,
    );
    let acquired_groups: HashSet<_> = webclass
        .as_ref()
        .into_iter()
        .flat_map(|report| report.attempts())
        .flat_map(|attempt| attempt.groups())
        .filter_map(|group| match group.observation().scope() {
            SyncScope::WebClassContents(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let latest_scopes: BTreeMap<_, _> = rules
        .iter()
        .filter_map(|rule| {
            if rule.key.origin != OriginKind::WebClass {
                return None;
            }
            rule.destination
                .as_ref()
                .ok()
                .map(|course| (rule.key.id.clone(), course.term.clone()))
        })
        .collect();
    let missing_groups = latest_scopes
        .keys()
        .filter(|id| !acquired_groups.contains(id.as_str()))
        .count();
    println!(
        "webclass confirmed_groups={} fetched_groups={} missing_groups={} rules_changed={}",
        latest_scopes.len(),
        acquired_groups.len(),
        missing_groups,
        requested != latest_scopes
    );

    let by_key: HashMap<_, _> = rules.iter().map(|rule| (&rule.key, rule)).collect();
    let mut per_course: BTreeMap<_, _> = rules
        .iter()
        .map(|rule| {
            (
                course_index(rule.key.origin, Some(&rule.key)),
                CourseCounts::default(),
            )
        })
        .collect();
    let mut per_source = BTreeMap::<_, CourseCounts>::new();
    let mut duplicate_candidates: HashMap<_, u8> = HashMap::new();
    for entry in plan.plan() {
        let Some(assignment) = input.assignments().get(&entry.key) else {
            continue;
        };
        let origin = match assignment.source {
            UniversitySource::WebClass => OriginKind::WebClass,
            UniversitySource::Moodle => OriginKind::Moodle,
        };
        let candidate_rule = assignment
            .course_key
            .as_ref()
            .and_then(|key| by_key.get(key));
        let idx = course_index(origin, assignment.course_key.as_ref());
        per_course.entry(idx).or_default().add(&entry.action);
        per_source
            .entry(source_name(origin))
            .or_default()
            .add(&entry.action);
        if matches!(
            entry.action,
            PlanAction::Create { .. } | PlanAction::Update { .. } | PlanAction::Unchanged { .. }
        ) && let Some(course) = candidate_rule.and_then(|rule| rule.destination.as_ref().ok())
            && let Some(deadline) = assignment.deadline.as_ref()
        {
            let identity = (
                &course.page_id,
                &assignment.title,
                json!(deadline).to_string(),
            );
            *duplicate_candidates.entry(identity).or_default() |=
                if origin == OriginKind::Moodle { 1 } else { 2 };
        }
    }
    for source in ["HOPPII", "Moodle"] {
        println!(
            "source {}",
            json!({"source": source, "counts": per_source.remove(source).unwrap_or_default()})
        );
    }
    let rules_by_index: BTreeMap<_, _> = rules
        .iter()
        .map(|rule| (course_index(rule.key.origin, Some(&rule.key)), rule))
        .collect();
    for (idx, counts) in per_course {
        let rule = rules_by_index.get(&idx).copied();
        let destination = rule.and_then(|rule| rule.destination.as_ref().ok());
        let status = rule_status(rule);
        let read = match idx.0 {
            "Moodle" if moodle.is_some() => "feed_read",
            "HOPPII" if idx.2.is_some_and(|id| acquired_groups.contains(id)) => "group_read",
            "HOPPII" if status == "excluded" => "excluded_before_fetch",
            _ => "not_read",
        };
        println!(
            "course {}",
            json!({
                "source": idx.0, "name": rule.map(|rule| rule.source_name.as_str()).or(idx.2),
                "rule": status, "destination": destination.map(|course| course.title.as_str()),
                "read": read, "counts": counts,
            })
        );
    }
    let counts = plan.counts_for(&input);
    let cross_duplicates = duplicate_candidates
        .values()
        .filter(|mask| **mask == 3)
        .count();
    println!("potential_cross_source_duplicates={cross_duplicates}");
    println!(
        "Create={} Update={} Unchanged={} Conflict={} Deferred={} Excluded={}",
        counts.create,
        counts.update,
        counts.unchanged,
        counts.conflict,
        counts.deferred,
        counts.course_rules.excluded
    );
    println!("{}", counts.course_rules);
    let incomplete = moodle.is_none()
        || (!requested.is_empty() && webclass.is_none())
        || missing_groups != 0
        || requested != latest_scopes;
    let partial = incomplete
        || inspection.unresolved_creates != 0
        || counts.conflict != 0
        || counts.deferred != 0
        || cross_duplicates != 0
        || inspection
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.duplicate_keys != 0)
        || inspection
            .course_rules
            .is_some_and(|rules| rules.has_issues());
    let failed = moodle.is_none() && webclass.is_none();
    println!(
        "result={} mode=dry-run writes=0",
        if failed {
            "failed"
        } else if partial {
            "partial"
        } else {
            "success"
        }
    );
    if let Some((writer, expected)) = apply {
        if failed
            || partial
            || !matches_expected(counts, expected, input.assignments().len())
            || (interactive
                && !inspection
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.managed == 0 && snapshot.manual == 1))
        {
            println!("result=stopped mode=apply issue=unexpected_preview");
            return Ok(3);
        }
        if interactive && !approve_production_first_sync(expected.0) {
            println!("result=stopped mode=production-first-sync issue=approval_missing writes=0");
            return Ok(3);
        }
        let apply = writer.apply(&client, &local.notion, &input, &plan, &directory);
        if !apply_completed(apply, "apply", expected, input.assignments().len())? {
            return Ok(3);
        }
        return verify_and_repeat(&client, &writer, local, &input, &directory, &state_path);
    }
    Ok(if failed {
        1
    } else if partial {
        3
    } else {
        0
    })
}

fn approve_production_first_sync(create: usize) -> bool {
    let phrase = format!("APPLY PRODUCTION {create}");
    println!("approval=required production_creates={create} updates=0 manual_preserved=1");
    println!("To apply this exact plan, type {phrase} and press Enter.");
    let mut answer = String::new();
    io::stdout().flush().is_ok()
        && io::stdin().read_line(&mut answer).is_ok()
        && answer.trim_end_matches(['\r', '\n']) == phrase
}

fn matches_expected(
    counts: DryRunCounts,
    expected: (usize, usize, usize),
    candidates: usize,
) -> bool {
    expected == (counts.create, counts.update, counts.unchanged)
        && counts.create + counts.update + counts.unchanged == candidates
        && counts.conflict == 0
        && counts.deferred == 0
        && counts.course_rules.excluded == 0
}

fn apply_completed(
    result: Result<ApplyReport, ApplyError>,
    phase: &str,
    expected: (usize, usize, usize),
    candidates: usize,
) -> Result<bool, CliError> {
    match result {
        Ok(report) => {
            let success = report.error.is_none()
                && report.pending == 0
                && matches_expected(report.counts, expected, candidates);
            println!(
                "phase={phase} result={} create={} update={} unchanged={} conflict={} deferred={} recovered={} pending={}",
                if success { "success" } else { "partial" },
                report.counts.create,
                report.counts.update,
                report.counts.unchanged,
                report.counts.conflict,
                report.counts.deferred,
                report.recovered,
                report.pending,
            );
            if let Some(error) = report.error {
                println!("phase={phase} diagnostic={}", notion_diagnosis(error));
            }
            Ok(success)
        }
        Err(ApplyError::State(error)) => Err(error.into()),
        Err(ApplyError::Notion(error)) => {
            println!(
                "phase={phase} result=stopped diagnostic={}",
                notion_diagnosis(error)
            );
            Ok(false)
        }
        Err(_) => {
            println!("phase={phase} result=stopped issue=plan_changed");
            Ok(false)
        }
    }
}

fn verify_and_repeat(
    client: &NotionClient,
    writer: &SyncWriter<'_>,
    local: &Config,
    input: &SyncInput,
    directory: &Path,
    state_path: &Path,
) -> Result<u8, CliError> {
    let verification = match match writer {
        SyncWriter::Verification(_) => client.verify_synced_assignments(&local.notion, input),
        SyncWriter::Production(_, protected) => {
            client.verify_production_assignments(&local.notion, input, protected)
        }
    } {
        Ok(report) => report,
        Err(error) => {
            println!(
                "phase=readback result=failed diagnostic={} detail={error}",
                notion_diagnosis(error),
            );
            return Ok(3);
        }
    };
    println!(
        "phase=readback rows={} unique_keys={} deadlines_present={} deadlines_at_minute_precision={} urls_present={} notion_statuses={} mismatches={}",
        verification.matched,
        verification.matched,
        verification.deadlines_present,
        verification.deadlines_at_minute_precision,
        verification.urls_present,
        json!(verification.statuses),
        json!(verification.field_mismatches),
    );
    if !verification.field_mismatches.is_empty() {
        return Ok(3);
    }
    if matches!(writer, SyncWriter::Production(..)) {
        println!("protected_manual_assignment=unchanged");
    }
    let expected = input.assignments().len();
    let mut plan = client.dry_run(input, &local.notion, &[]);
    if let Some(state) = StateStore::read(state_path, &local.target_id()?)? {
        plan.defer_unreconciled(&state);
    }
    let counts = plan.counts_for(input);
    println!(
        "phase=repeat_dry_run create={} update={} unchanged={} conflict={} deferred={}",
        counts.create, counts.update, counts.unchanged, counts.conflict, counts.deferred,
    );
    if plan.error().is_some() || !matches_expected(counts, (0, 0, expected), expected) {
        return Ok(3);
    }
    let apply = writer.apply(client, &local.notion, input, &plan, directory);
    if !apply_completed(apply, "repeat_apply", (0, 0, expected), expected)? {
        return Ok(3);
    }
    println!("phase=repeat_apply writes=0");
    Ok(0)
}

fn source_name(origin: OriginKind) -> &'static str {
    match origin {
        OriginKind::Moodle => "Moodle",
        OriginKind::WebClass => "HOPPII",
        OriginKind::Manual => "Manual",
    }
}

fn course_index(
    origin: OriginKind,
    key: Option<&ExternalRecordKey>,
) -> (&'static str, Option<&str>, Option<&str>) {
    (
        source_name(origin),
        key.map(|key| key.scope.as_str()),
        key.map(|key| key.id.as_str()),
    )
}

fn rule_status(rule: Option<&VerificationCourseRule>) -> &'static str {
    match rule.map(|rule| &rule.destination) {
        Some(Ok(_)) => "confirmed",
        Some(Err(DeferredReason::CourseExcluded)) => "excluded",
        Some(Err(DeferredReason::CourseUnconfigured)) => "unset",
        Some(Err(DeferredReason::CourseRuleDuplicate)) => "duplicate",
        Some(Err(DeferredReason::CourseTermUnverified)) => "unverified_term",
        Some(Err(DeferredReason::CourseRuleInvalidRelation)) => "invalid_relation",
        Some(Err(_)) => "invalid",
        None => "unresolved",
    }
}

#[derive(Default, Serialize)]
struct CourseCounts {
    candidates: usize,
    create: usize,
    update: usize,
    unchanged: usize,
    excluded: usize,
    held: usize,
    conflict: usize,
    reasons: BTreeMap<String, usize>,
}

impl CourseCounts {
    fn add(&mut self, action: &PlanAction) {
        self.candidates += 1;
        match action {
            PlanAction::Create { .. } => self.create += 1,
            PlanAction::Update { .. } => self.update += 1,
            PlanAction::Unchanged { .. } => self.unchanged += 1,
            PlanAction::Excluded => self.excluded += 1,
            PlanAction::Deferred(reason) => {
                self.held += 1;
                *self.reasons.entry(format!("{reason:?}")).or_default() += 1;
            }
            PlanAction::Conflict(reason) => {
                self.conflict += 1;
                *self.reasons.entry(format!("{reason:?}")).or_default() += 1;
            }
        }
    }
}
