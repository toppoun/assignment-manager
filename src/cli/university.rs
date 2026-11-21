use assignment_manager::{
    moodle_http::{MoodleExportKind, MoodleFeed},
    secured_sync::SecuredSyncClients,
    sync::{MoodleSyncError, acquire_moodle_with_diagnostic},
};

use super::{CliError, Config, CredentialStatus, credentials};

pub(super) fn moodle(
    config: &Config,
    config_dir: &std::path::Path,
    require_windows_vpn: bool,
) -> Result<u8, CliError> {
    let source = config
        .moodle
        .as_ref()
        .ok_or(CliError::Credential(CredentialStatus::Disabled))
        .inspect_err(|_| {
            print_source("moodle", 0, Some("setup"));
        })?;
    let policy = source.policy()?;
    #[cfg(windows)]
    let policy = {
        let Some(files) = source.windows_crl.as_ref() else {
            return Ok(crl_failure("tls_revocation_unconfirmed"));
        };
        match policy.with_moodle_windows_crl(
            &config_dir.join(&files.leaf_certificate),
            &config_dir.join(&files.issuer_certificate),
            &config_dir.join(&files.crl),
        ) {
            Ok(policy) if require_windows_vpn => match policy.with_moodle_windows_vpn() {
                Ok(policy) => policy,
                Err(_) => return Ok(crl_failure("vpn_route_unconfirmed")),
            },
            Ok(policy) => policy,
            Err(error) => return Ok(crl_failure(error.diagnostic())),
        }
    };
    #[cfg(not(windows))]
    {
        let _ = config_dir;
        if require_windows_vpn {
            return Ok(crl_failure("vpn_route_unconfirmed"));
        }
        if source.windows_crl.is_some() {
            return Ok(crl_failure("tls_revocation_unconfirmed"));
        }
    }
    let prepare = || {
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

fn crl_failure(diagnostic: &'static str) -> u8 {
    print_source("moodle", 0, Some("fetch"));
    println!("moodle diagnostic={diagnostic}");
    finish(false)
}

fn check_moodle(clients: &SecuredSyncClients, feed: &MoodleFeed) -> bool {
    let Ok(client) = clients.moodle() else {
        print_source("moodle", 0, Some("fetch"));
        println!("moodle diagnostic=client_initialization");
        return false;
    };
    match acquire_moodle_with_diagnostic(&client, feed) {
        Ok(acquired) => print_source(
            "moodle",
            acquired.assignments().batch().as_slice().len(),
            None,
        ),
        Err(error) => {
            let stage = match error.kind {
                MoodleSyncError::Fetch => "fetch",
                MoodleSyncError::Parse => "parse",
                MoodleSyncError::Validation => "validation",
                MoodleSyncError::Persistence => "persistence",
            };
            print_source("moodle", 0, Some(stage));
            let code = error.diagnostic.code();
            if let Some(status) = error.diagnostic.http_status() {
                println!("moodle diagnostic={code} http_status={status}");
            } else {
                println!("moodle diagnostic={code}");
            }
            false
        }
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
