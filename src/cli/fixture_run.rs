use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{SyncInput, state::StateSummary},
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
    let (notion, _server) =
        assignment_manager::notion::fixture::start().map_err(|_| CliError::FixtureFailed)?;
    let mut report = notion.dry_run(&input, &assignment_manager::notion::fixture::target(), &[]);
    if let Some(state) = state {
        report.defer_creates(&state.blocked_creates);
    }
    Ok(Receipt::new(
        moodle_status,
        webclass_status,
        report.counts(),
        report.error(),
    ))
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
