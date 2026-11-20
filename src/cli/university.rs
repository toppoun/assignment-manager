use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    secured_sync::SecuredSyncClients,
    sync::{MoodleSyncError, acquire_moodle},
};

use super::{CliError, Config, CredentialStatus, credentials};

pub(super) fn moodle(config: &Config) -> Result<u8, CliError> {
    let prepare = || {
        let source = config
            .moodle
            .as_ref()
            .ok_or(CliError::Credential(CredentialStatus::Disabled))?;
        let policy = source.policy()?;
        let url = credentials::moodle_url(&config.target_id()?).map_err(CliError::Credential)?;
        policy
            .validate_moodle_url(&url)
            .map_err(|_| CliError::Credential(CredentialStatus::Invalid))?;
        let feed = MoodleFeed::new(&source.scope, MoodleExportKind::CourseRelated, &url)
            .map_err(|_| CliError::Credential(CredentialStatus::Invalid))?;
        Ok((SecuredSyncClients::new(policy), feed))
    };
    let (mut clients, feed) = prepare().inspect_err(|_: &CliError| {
        print_source("moodle", 0, Some("setup"));
    })?;
    clients.begin_run();
    let success = check_moodle(&clients, &feed);
    Ok(finish(success))
}

fn check_moodle(clients: &SecuredSyncClients, feed: &MoodleFeed) -> bool {
    let result = clients
        .moodle()
        .map_err(|_| MoodleSyncError::Fetch)
        .and_then(|client| acquire_moodle(&client, feed));
    match result {
        Ok(acquired) => print_source(
            "moodle",
            acquired.assignments().batch().as_slice().len(),
            None,
        ),
        Err(error) => print_source(
            "moodle",
            0,
            Some(match error {
                MoodleSyncError::Fetch => "fetch",
                MoodleSyncError::Parse => "parse",
                MoodleSyncError::Validation => "validation",
                MoodleSyncError::Persistence => "persistence",
            }),
        ),
    }
}

fn print_source(source: &str, acquired: usize, error: Option<&str>) -> bool {
    if let Some(error) = error {
        println!("{source} acquired={acquired} failures=1 error={error}");
    } else {
        println!("{source} acquired={acquired} failures=0");
    }
    error.is_none()
}

fn finish(success: bool) -> u8 {
    println!("result={}", if success { "success" } else { "failed" });
    u8::from(!success)
}

#[cfg(feature = "synthetic-https-fixture")]
pub(super) fn fixture(scenario: assignment_manager::webclass_auth::synthetic::Scenario) -> u8 {
    use assignment_manager::{
        sync::{WebClassSyncError, acquire_webclass_once},
        webclass_auth::{
            HoseiCredentials,
            synthetic::{MOODLE_URL, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD},
        },
    };

    let university = SyntheticUniversity::start(scenario);
    let mut clients = SecuredSyncClients::new(university.policy());
    clients.begin_run();
    let feed = MoodleFeed::new(
        "default.course-related",
        MoodleExportKind::CourseRelated,
        MOODLE_URL,
    )
    .expect("fixed synthetic feed");
    let moodle_ok = check_moodle(&clients, &feed);
    let webclass_ok = match clients
        .authenticator()
        .login(HoseiCredentials::new(UNIVERSITY_ID, UNIVERSITY_PASSWORD))
    {
        Ok(client) => {
            let attempt = acquire_webclass_once(&client);
            let acquired = attempt
                .groups()
                .iter()
                .map(|group| group.batch().as_slice().len())
                .sum();
            let error = attempt.result().err().map(|error| match error {
                WebClassSyncError::AuthExpired => "auth_expired",
                WebClassSyncError::Fetch => "fetch",
                WebClassSyncError::Parse => "parse",
                WebClassSyncError::Map => "mapping",
                WebClassSyncError::CourseValidation | WebClassSyncError::Validation => "validation",
                WebClassSyncError::CoursePersistence | WebClassSyncError::Persistence => {
                    "persistence"
                }
            });
            print_source("webclass", acquired, error)
        }
        Err(_) => print_source("webclass", 0, Some("authentication")),
    };
    finish(moodle_ok && webclass_ok)
}

#[cfg(all(test, feature = "synthetic-https-fixture"))]
#[test]
fn university_check_fails_when_one_source_fails_or_the_session_expires() {
    use assignment_manager::webclass_auth::synthetic::Scenario;

    for scenario in [Scenario::MoodleFailure, Scenario::AuthExpiredOnce] {
        assert_eq!(fixture(scenario), 1);
    }
}
