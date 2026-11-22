use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    notion::{DryRunReport, SyncInput, fixture, state::StateSummary},
    secured_sync::SecuredSyncClients,
    sync::{
        MoodleAcquisition, WebClassCredentialProvider, acquire_moodle, acquire_webclass_with_reauth,
    },
    webclass_auth::{
        HoseiCredentials,
        synthetic::{
            MOODLE_SCOPE, MOODLE_URL, Scenario, SyntheticUniversity, UNIVERSITY_ID,
            UNIVERSITY_PASSWORD,
        },
    },
};

use super::{AcquisitionStatus, CliError, InspectionReport, Receipt};

pub(super) fn apply(directory: &std::path::Path) -> Result<u8, CliError> {
    let acquired = acquire(Scenario::BothSuccess)?;
    let reports =
        fixture::exercise_apply(&acquired.input, directory).map_err(|error| match error {
            fixture::ApplyError::State(error) => CliError::State(error),
            fixture::ApplyError::PlanChanged => CliError::FixtureFailed,
            fixture::ApplyError::Notion(error) => {
                println!("notion_error={}", super::notion_diagnosis(error));
                CliError::FixtureFailed
            }
        })?;
    let mut ok = matches!(acquired.moodle, AcquisitionStatus::Success)
        && matches!(acquired.webclass, AcquisitionStatus::Success);
    for (pass, report) in ["initial", "repeat"].into_iter().zip(reports) {
        let c = report.counts;
        println!(
            "pass={pass} Created={} Updated={} Unchanged={} Conflict={} Deferred={} recovered={} pending={} manual_candidates={}",
            c.create,
            c.update,
            c.unchanged,
            c.conflict,
            c.deferred,
            report.recovered,
            report.pending,
            report.manual_candidates
        );
        if let Some(error) = report.error {
            println!("notion_error={}", super::notion_diagnosis(error));
            ok = false;
        }
        ok &= c.conflict == 0 && c.deferred == 0 && report.pending == 0;
    }
    println!("result={}", if ok { "success" } else { "partial" });
    Ok(if ok { 0 } else { 3 })
}

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

pub(super) fn dry_run(
    state: Option<&StateSummary>,
) -> Result<(InspectionReport, Receipt), CliError> {
    run(state, Scenario::BothSuccess)
}

fn run(
    state: Option<&StateSummary>,
    scenario: Scenario,
) -> Result<(InspectionReport, Receipt), CliError> {
    let acquired = acquire(scenario)?;
    let (notion, _server) =
        assignment_manager::notion::fixture::start().map_err(|_| CliError::FixtureFailed)?;
    let (inspection, mut report) = notion.inspect_dry_run(
        &acquired.input,
        &assignment_manager::notion::fixture::target(),
        &[],
        &[],
    );
    if let Some(state) = state {
        report.defer_unreconciled(state);
    }
    Ok((inspection, acquired.receipt(&report)))
}

pub(super) struct Acquired {
    pub input: SyncInput,
    moodle: AcquisitionStatus,
    webclass: AcquisitionStatus,
}

impl Acquired {
    pub(super) fn receipt(&self, report: &DryRunReport) -> Receipt {
        Receipt::new(self.moodle, self.webclass, report.counts(), report.error())
    }
}

pub(super) fn acquire(scenario: Scenario) -> Result<Acquired, CliError> {
    use AcquisitionStatus::{Failed, Partial, Success};
    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let moodle = fixture_moodle(&clients).ok();
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

pub(super) fn moodle_only(scenario: Scenario) -> Result<SyncInput, CliError> {
    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let moodle = fixture_moodle(&clients)?;
    SyncInput::from_acquisitions(Some(&moodle), None).map_err(|_| CliError::FixtureFailed)
}

fn fixture_moodle(clients: &SecuredSyncClients) -> Result<MoodleAcquisition, CliError> {
    let feed = MoodleFeed::new(MOODLE_SCOPE, MoodleExportKind::CourseRelated, MOODLE_URL)
        .map_err(|_| CliError::FixtureFailed)?;
    let client = clients.moodle().map_err(|_| CliError::FixtureFailed)?;
    acquire_moodle(&client, &feed).map_err(|_| CliError::FixtureFailed)
}

#[cfg(test)]
#[test]
fn one_source_failure_still_plans_for_the_other_source() {
    let (_, report) = run(None, Scenario::MoodleFailure).unwrap();
    assert_eq!(report.moodle.code(), "failed");
    assert_eq!(report.webclass.code(), "success");
    assert!(report.counts.create > 0);
    assert_eq!(report.outcome.code(), "partial");
}
