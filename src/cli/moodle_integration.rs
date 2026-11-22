use std::path::Path;

use assignment_manager::{
    notion::{
        DryRunCounts, InvalidSourceData, MOODLE_INTEGRATION_SCOPE, MoodlePreviewMappings,
        SyncInput, VerificationTarget, state::StateStore,
    },
    sync::MoodleAcquisition,
};

use super::{
    CliError, Config,
    config::{self, ConfigError},
    credentials, moodle_review, university,
};

pub(super) fn execute(
    notion_path: &Path,
    moodle_path: &Path,
    require_windows_vpn: bool,
) -> Result<u8, CliError> {
    let notion_config = Config::read(notion_path)?;
    if notion_config.course_mappings.is_empty() {
        println!("result=blocked reason=incomplete_course_mappings errors=1");
        return Ok(3);
    }
    let target = VerificationTarget::moodle_integration(notion_config.notion)
        .map_err(CliError::MoodlePlanNotion)?;
    let mappings = MoodlePreviewMappings::new(&notion_config.course_mappings)
        .map_err(CliError::MoodlePlanNotion)?;

    // The feed uses its existing credential namespace, independent of the Notion target.
    let moodle_config = Config::read(moodle_path)?;
    let source = moodle_config.moodle.as_ref().ok_or(ConfigError::Invalid)?;
    if source.host != "cms.cis.k.hosei.ac.jp"
        || source.path != "/calendar/export_execute.php"
        || source.scope != MOODLE_INTEGRATION_SCOPE
    {
        return Err(ConfigError::Invalid.into());
    }
    let directory = moodle_path.parent().unwrap_or(Path::new("."));
    let Some(policy) = university::secured_moodle_policy(source, directory, require_windows_vpn)?
    else {
        println!("result=failed reason=moodle_setup errors=1");
        return Ok(1);
    };
    let id = target.assignment_id();
    let path = config::root()?
        .join("targets")
        .join(id)
        .join("state.sqlite3");
    let state = StateStore::read(&path, id)?;
    let client = credentials::notion_client(id).map_err(CliError::Credential)?;
    let (mut clients, feed) = university::prepare_moodle_request(&moodle_config, source, policy)?;
    println!(
        "sqlite={} confirmation=required",
        if state.is_some() {
            "read_only"
        } else {
            "absent"
        },
    );
    match moodle_review::confirm_fetch(
        require_windows_vpn,
        "Moodleを取得し、検証用Notion DBとの読み取り専用の差分計画を作成します。",
    ) {
        Ok(true) => (),
        Ok(false) => {
            println!("result=cancelled reason=moodle_confirmation_declined");
            return Ok(2);
        }
        Err(()) => {
            println!("result=blocked reason=moodle_confirmation_unavailable errors=1");
            return Ok(3);
        }
    }
    clients.begin_run();
    let acquired = university::acquire_moodle_for_check(&clients, &feed);
    drop((clients, feed));

    let result = plan_acquisition(acquired.as_ref(), &mappings, |input| {
        let preview =
            client.preview_moodle_integration(&target, input, &mappings, state.unwrap_or_default());
        if preview.is_ok() {
            println!(
                "notion=ready reader=verified schemas=verified course_pages_verified={}",
                notion_config.course_mappings.len()
            );
        }
        preview
            .map(|report| report.counts_for(input))
            .map_err(CliError::MoodlePlanNotion)
    });
    let (operations, retries) = client.read_counts();
    println!("notion_read operations={operations} retries={retries}");
    result
}

fn plan_acquisition(
    acquired: Option<&MoodleAcquisition>,
    mappings: &MoodlePreviewMappings<'_>,
    plan: impl FnOnce(&SyncInput) -> Result<DryRunCounts, CliError>,
) -> Result<u8, CliError> {
    let Some(acquired) = acquired else {
        println!("result=failed reason=moodle_acquisition errors=1");
        return Ok(1);
    };
    let input = match SyncInput::from_acquisitions(Some(acquired), None) {
        Ok(input) => input,
        Err(error) => {
            let reason = if error == InvalidSourceData::DuplicateSyncKey {
                "moodle_sync_key_conflict"
            } else {
                "moodle_invalid_candidates"
            };
            println!("result=failed reason={reason} errors=1");
            return Ok(1);
        }
    };
    let coverage = mappings
        .coverage(&input)
        .map_err(CliError::MoodlePlanNotion)?;
    println!(
        "moodle=live candidates={} courses={} mapped_courses={} unmapped_courses={} unused_mappings={} unmapped_candidates={} unclassified_candidates={} timed_deadlines={} date_only_deadlines={} missing_deadlines={}",
        coverage.candidates,
        coverage.courses,
        coverage.courses - coverage.unmapped_courses,
        coverage.unmapped_courses,
        coverage.unused_mappings,
        coverage.unmapped_candidates,
        coverage.unclassified_candidates,
        coverage.timed_deadlines,
        coverage.date_only_deadlines,
        coverage.missing_deadlines
    );
    if !coverage.ready() {
        let reason = if coverage.candidates == 0 {
            "empty_acquisition"
        } else if coverage.missing_deadlines != 0 {
            "missing_deadlines"
        } else {
            "incomplete_course_mappings"
        };
        println!("result=blocked reason={reason} errors=1");
        return Ok(3);
    }
    let counts = plan(&input).inspect_err(|_| println!("phase=notion_preview errors=1"))?;
    let errors = counts.conflict + counts.deferred;
    let ready = errors == 0 && counts.course_relation_mismatch == 0;
    let result = if ready { "success" } else { "blocked" };
    println!(
        "result={result} create={} update={} unchanged={} errors={errors} conflict={} deferred={} course_relation_mismatch={}",
        counts.create,
        counts.update,
        counts.unchanged,
        counts.conflict,
        counts.deferred,
        counts.course_relation_mismatch
    );
    Ok(if ready { 0 } else { 3 })
}

#[cfg(all(test, feature = "synthetic-https-fixture"))]
mod tests {
    use super::*;
    use assignment_manager::{
        import::{ExternalRecordKey, OriginKind},
        moodle_http::{MoodleExportKind, MoodleFeed},
        notion::CourseMapping,
        secured_sync::SecuredSyncClients,
        webclass_auth::synthetic::{MOODLE_URL, Scenario, SyntheticUniversity},
    };

    fn acquire(scenario: Scenario) -> Option<MoodleAcquisition> {
        let university = SyntheticUniversity::start(scenario);
        let mut clients = SecuredSyncClients::new(university.policy());
        clients.begin_run();
        let feed = MoodleFeed::new(
            MOODLE_INTEGRATION_SCOPE,
            MoodleExportKind::CourseRelated,
            MOODLE_URL,
        )
        .unwrap();
        university::acquire_moodle_for_check(&clients, &feed)
    }

    #[test]
    fn integration_stops_before_notion_on_failed_empty_or_unmapped_acquisition() {
        let mappings = MoodlePreviewMappings::new(&[]).unwrap();
        for (scenario, code) in [
            (Scenario::MoodleFailure, 1),
            (Scenario::MoodleDuplicate, 1),
            (Scenario::MoodleEmpty, 3),
            (Scenario::MoodleIntegration, 3),
        ] {
            let acquired = acquire(scenario);
            assert_eq!(
                plan_acquisition(acquired.as_ref(), &mappings, |_| panic!(
                    "Notion must not be called"
                )),
                Ok(code)
            );
        }
    }

    #[test]
    fn integration_passes_all_mapped_candidates_to_the_read_only_planner() {
        let acquired = acquire(Scenario::MoodleIntegration).unwrap();
        let entries = [
            ("統合試験 A", "00000000-0000-4000-8000-000000000001"),
            ("統合試験 B", "00000000-0000-4000-8000-000000000002"),
        ]
        .map(|(name, page)| CourseMapping {
            course_key: ExternalRecordKey {
                origin: OriginKind::Moodle,
                scope: MOODLE_INTEGRATION_SCOPE.into(),
                id: name.into(),
            },
            page_id: page.into(),
        });
        let mappings = MoodlePreviewMappings::new(&entries).unwrap();
        assert_eq!(
            plan_acquisition(Some(&acquired), &mappings, |input| {
                assert_eq!(input.assignments().len(), 4);
                Ok(DryRunCounts {
                    create: 2,
                    update: 1,
                    unchanged: 1,
                    ..Default::default()
                })
            }),
            Ok(0)
        );
    }
}
