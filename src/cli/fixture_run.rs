use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{CourseMapping, DryRunReport, NotionError, SyncInput, state::StateSummary},
    secured_sync::SecuredSyncClients,
    sync::{WebClassCredentialProvider, acquire_moodle, acquire_webclass_with_reauth},
    webclass_auth::{
        HoseiCredentials,
        synthetic::{
            MOODLE_URL, Scenario, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD,
        },
    },
};

use super::{AcquisitionStatus, CliError, Receipt};

struct FixtureCredentials;

impl WebClassCredentialProvider for FixtureCredentials {
    fn with_credentials<R>(
        &mut self,
        use_credentials: impl FnOnce(HoseiCredentials<'_>) -> R,
    ) -> Option<R> {
        Some(use_credentials(credentials()))
    }
}

fn credentials() -> HoseiCredentials<'static> {
    HoseiCredentials::new(UNIVERSITY_ID, UNIVERSITY_PASSWORD)
}

pub(super) fn dry_run(state: Option<&StateSummary>) -> Result<Receipt, CliError> {
    run(state, Scenario::BothSuccess)
}

fn run(state: Option<&StateSummary>, scenario: Scenario) -> Result<Receipt, CliError> {
    let acquired = acquire(scenario)?;
    let (notion, _server) =
        assignment_manager::notion::fixture::start().map_err(|_| CliError::FixtureFailed)?;
    let mut report = notion.dry_run(
        &acquired.input,
        &assignment_manager::notion::fixture::target(),
        &[],
    );
    if let Some(state) = state {
        report.defer_creates(&state.blocked_creates);
    }
    Ok(acquired.receipt(&report))
}

/// Selected pages only, so the official GET-only path cannot produce writes.
pub(super) fn get_only_preview(
    error: Option<NotionError>,
    mappings: &[CourseMapping],
) -> Result<Receipt, CliError> {
    let acquired = acquire(Scenario::BothSuccess)?;
    let configured: std::collections::HashSet<_> =
        mappings.iter().map(|mapping| &mapping.course_key).collect();
    let unresolved = acquired
        .input
        .assignments()
        .values()
        .filter(|assignment| {
            assignment
                .course_key
                .as_ref()
                .is_some_and(|key| !configured.contains(key))
        })
        .count();
    println!("course_mapping_unconfigured={unresolved}");
    let report = DryRunReport::incomplete(
        &acquired.input,
        error.unwrap_or(NotionError::MethodDisabled),
    );
    Ok(acquired.receipt(&report))
}

struct Acquired {
    input: SyncInput,
    moodle: AcquisitionStatus,
    webclass: AcquisitionStatus,
}

impl Acquired {
    fn receipt(&self, report: &DryRunReport) -> Receipt {
        Receipt::new(self.moodle, self.webclass, report.counts(), report.error())
    }
}

fn acquire(scenario: Scenario) -> Result<Acquired, CliError> {
    use AcquisitionStatus::{Failed, Partial, Success};
    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let feed = MoodleFeed::new(
        "default.course-related",
        MoodleExportKind::CourseRelated,
        MOODLE_URL,
    )
    .map_err(|_| CliError::FixtureFailed)?;
    let moodle = clients
        .moodle()
        .ok()
        .and_then(|client| acquire_moodle(&client, &feed).ok());
    let moodle_status = if moodle.is_some() { Success } else { Failed };
    let authenticator = clients.authenticator();
    let webclass = authenticator.login(credentials()).ok().map(|mut session| {
        acquire_webclass_with_reauth(&mut session, &authenticator, &mut FixtureCredentials)
    });
    let webclass_status = match &webclass {
        Some(report) if report.result().is_ok() => Success,
        Some(report)
            if report
                .attempts()
                .last()
                .is_some_and(|attempt| !attempt.groups().is_empty()) =>
        {
            Partial
        }
        _ => Failed,
    };

    let input = SyncInput::from_acquisitions(moodle.as_ref(), webclass.as_ref())
        .map_err(|_| CliError::FixtureFailed)?;
    Ok(Acquired {
        input,
        moodle: moodle_status,
        webclass: webclass_status,
    })
}

#[cfg(test)]
#[test]
fn one_source_failure_still_plans_for_the_other_source() {
    let report = run(None, Scenario::MoodleFailure).unwrap();
    assert_eq!(report.moodle.code(), "failed");
    assert_eq!(report.webclass.code(), "success");
    assert!(report.counts.create > 0);
    assert_eq!(report.outcome.code(), "partial");
}

#[cfg(test)]
#[test]
fn get_only_preview_is_deferred_with_or_without_a_compatible_schema() {
    for (error, outcome) in [
        (None, "partial"),
        (Some(NotionError::SchemaMismatch), "failed"),
        (Some(NotionError::Forbidden), "failed"),
    ] {
        let report = get_only_preview(error, &[]).unwrap();
        assert_eq!(report.outcome.code(), outcome);
        assert_eq!(report.counts.deferred, 10);
        assert_eq!(report.counts.create, 0);
        assert_eq!(report.counts.update, 0);
    }
}
