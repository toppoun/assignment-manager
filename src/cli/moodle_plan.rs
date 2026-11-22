use std::path::Path;

use assignment_manager::{
    notion::{DryRunCounts, SyncInput, VerificationTarget, fixture, state::StateStore},
    webclass_auth::synthetic::Scenario,
};

use super::{CliError, Config, Paths, config, credentials, fixture_run};

pub(super) fn mock() -> Result<u8, CliError> {
    run(moodle_input(), |input| {
        let (client, _server) = fixture::start().map_err(|_| CliError::FixtureFailed)?;
        client
            .preview_verification(
                &fixture::verification_target(),
                input,
                &fixture::moodle_mappings(),
                Default::default(),
            )
            .map_err(CliError::MoodlePlanNotion)
    })
}

pub(super) fn notion(
    path: &Path,
    assignments: &str,
    courses: &str,
    reader: &str,
) -> Result<u8, CliError> {
    let config = Config::read(path)?;
    let target = VerificationTarget::new(config.notion, assignments, courses, reader)
        .map_err(CliError::MoodlePlanNotion)?;
    run(moodle_input(), |input| {
        let id = target.assignment_id();
        let paths = Paths {
            target: id.into(),
            directory: config::root()?.join("targets").join(id),
        };
        let state = StateStore::read(&paths.state(), id)?.unwrap_or_default();
        let client = credentials::notion_client(id).map_err(CliError::Credential)?;
        client
            .preview_verification(&target, input, &config.course_mappings, state)
            .map_err(CliError::MoodlePlanNotion)
    })
}

fn moodle_input() -> Result<SyncInput, CliError> {
    fixture_run::moodle_only(Scenario::MoodleReview).map_err(|_| CliError::MoodlePlanAcquisition)
}

fn run(
    input: Result<SyncInput, CliError>,
    preview: impl FnOnce(&SyncInput) -> Result<DryRunCounts, CliError>,
) -> Result<u8, CliError> {
    let input = input?;
    if input.assignments().is_empty() {
        return Err(CliError::MoodlePlanEmpty);
    }
    let counts = preview(&input)?;
    let errors = counts.conflict + counts.deferred;
    let result = if errors == 0 { "success" } else { "partial" };
    println!(
        "result={result} moodle=fixture candidates={} create={} update={} unchanged={} errors={errors} conflict={} deferred={} course_relation_mismatch={}",
        input.assignments().len(),
        counts.create,
        counts.update,
        counts.unchanged,
        counts.conflict,
        counts.deferred,
        counts.course_relation_mismatch,
    );
    Ok(if errors == 0 { 0 } else { 3 })
}

#[cfg(test)]
#[test]
fn unavailable_moodle_stops_before_notion_or_state() {
    let missing = fixture_run::moodle_only(Scenario::MoodleFailure)
        .map_err(|_| CliError::MoodlePlanAcquisition);
    assert_eq!(
        run(missing, |_| panic!("Notion must not be queried")),
        Err(CliError::MoodlePlanAcquisition)
    );
    assert_eq!(
        run(Ok(SyncInput::default()), |_| panic!(
            "Notion must not be queried"
        )),
        Err(CliError::MoodlePlanEmpty)
    );
}
