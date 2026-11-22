//! One-shot application operations connecting source clients to validated persistence.
//!
//! Fetching, parsing, mapping and validation finish before the store starts its
//! import transaction. Candidate absence has no deletion or presence semantics.

mod acquisition;

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

pub use acquisition::{
    AcquiredAssignments, AcquiredWebClassCourses, MoodleAcquisition, MoodleAcquisitionError,
    SourceCourse, WebClassAcquisition, WebClassAcquisitionAttempt, WebClassCourseListError,
    acquire_moodle, acquire_moodle_with_diagnostic, acquire_webclass_group, acquire_webclass_once,
    acquire_webclass_with_reauth, login_and_acquire_webclass_courses,
};
use acquisition::{
    WebClassEvent, acquire_webclass_courses, run_with_webclass_reauth, visit_moodle,
    visit_webclass_groups,
};

use crate::moodle_http::{MoodleClient, MoodleFeed};
use crate::persistence::{AssignmentImportSummary, MoodleCourseResolution, SqliteStore};
use crate::webclass_auth::{HoseiAuthenticator, HoseiCredentials};
use crate::webclass_http::WebClassClient;

/// Fixed stage classifications only; source payloads and underlying errors are discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleSyncError {
    Fetch,
    Parse,
    Validation,
    Persistence,
}

impl fmt::Display for MoodleSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fetch => "Moodle sync fetch failed",
            Self::Parse => "Moodle sync parse failed",
            Self::Validation => "Moodle sync candidate validation failed",
            Self::Persistence => "Moodle sync persistence failed",
        })
    }
}

// No source chain can expose a URL, response body, identity, SQL or database path.
impl Error for MoodleSyncError {}

/// Synchronize one Moodle feed once on the calling thread.
///
/// The reusable client and feed are borrowed. Only the feed's fixed, non-secret
/// scope reaches the mapper. After all validation, usable categories referenced
/// by candidates discover courses by Moodle + feed scope + category. Each new
/// course/binding commits separately and survives a later import failure. Existing
/// names and schedules are retained. Pre-discovery failures leave the DB untouched.
/// Import failures roll back the store's
/// batch transaction and never return a partial summary. An empty validated batch
/// returns zero counts without changing existing assignments. All parsed events,
/// including unclassified/start events, supply identities before mapping. Positive
/// observations and scope freshness commit with import; absence never means missing.
pub fn sync_moodle_once(
    client: &MoodleClient,
    feed: &MoodleFeed,
    store: &mut SqliteStore,
) -> Result<AssignmentImportSummary, MoodleSyncError> {
    let mut resolutions = HashMap::new();
    let assignments = visit_moodle(client, feed, |assignment_key, _, course| {
        let Some(course) = course else { return Ok(()) };
        let key = course.external_key;
        let course_id = match store
            .find_moodle_course_id(&key.scope, &key.id)
            .map_err(|_| MoodleSyncError::Persistence)?
        {
            Some(id) => id,
            None => {
                store
                    .create_course_with_source(course.name, vec![], key.origin, &key.scope, &key.id)
                    .map_err(|_| MoodleSyncError::Persistence)?
                    .id
            }
        };
        resolutions.insert(
            assignment_key,
            MoodleCourseResolution::new(course_id, key.id),
        );
        Ok(())
    })
    .map_err(|error| error.kind)?;
    store
        .import_assignment_batch_with_observation_and_courses(
            assignments.batch(),
            assignments.observation(),
            &resolutions,
            assignments.observed_at(),
        )
        .map_err(|_| MoodleSyncError::Persistence)
}

/// Fixed stage classifications only; no group ID, payload, cookie or underlying error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebClassSyncError {
    AuthExpired,
    Fetch,
    Parse,
    CourseValidation,
    CoursePersistence,
    Map,
    Validation,
    Persistence,
}

impl fmt::Display for WebClassSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AuthExpired => "WebClass sync authentication expired",
            Self::Fetch => "WebClass sync fetch failed",
            Self::Parse => "WebClass sync parse failed",
            Self::CourseValidation => "WebClass sync course list validation failed",
            Self::CoursePersistence => "WebClass sync course persistence failed",
            Self::Map => "WebClass sync mapping failed",
            Self::Validation => "WebClass sync candidate validation failed",
            Self::Persistence => "WebClass sync persistence failed",
        })
    }
}

// Discard causes rather than exposing source data through an error chain.
impl Error for WebClassSyncError {}

/// Synchronize contents for one exact, opaque group ID on the calling thread.
///
/// The authenticated client is borrowed; authentication expiry is returned without
/// logging in or retrying. No course discovery request is made. The group mapper
/// checks every record against the requested ID and calls
/// [`AssignmentCandidateBatch::try_new`] before returning a validated batch.
/// Only then does the store start its import transaction and resolve existing
/// course bindings. Missing courses are not created. Pre-import failures leave
/// the database untouched; import failures roll back the entire batch. Existing
/// assignments absent from the batch are preserved, including filtered records.
/// All parsed records (including hidden/unsupported) are observed before mapping,
/// with exact requested-group identity validation. Import, positive seen metadata
/// and this contents scope's freshness commit together. Empty batches return zero
/// counts while committing freshness; they never imply disappearance.
pub fn sync_webclass_group_once(
    client: &WebClassClient,
    group_id: &str,
    store: &mut SqliteStore,
) -> Result<AssignmentImportSummary, WebClassSyncError> {
    let assignments = acquire_webclass_group(client, group_id)?;
    import_webclass_assignments(store, &assignments)
}

fn import_webclass_assignments(
    store: &mut SqliteStore,
    assignments: &AcquiredAssignments,
) -> Result<AssignmentImportSummary, WebClassSyncError> {
    store
        .import_assignment_batch_with_observation(
            assignments.batch(),
            assignments.observation(),
            assignments.observed_at(),
        )
        .map_err(|_| WebClassSyncError::Persistence)
}

/// Counts from a fully successful course discovery and contents sync.
/// No source identities, names or entity data are retained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebClassSyncSummary {
    /// Number of records in the validated course list, including existing bindings.
    pub courses_discovered: usize,
    pub courses_created: usize,
    /// Successful group operations, including those with empty candidate batches.
    pub groups_synced: usize,
    pub assignments_created: usize,
    pub assignments_updated: usize,
    pub assignments_unchanged: usize,
}

/// Discover WebClass courses and synchronize their contents once in response order.
///
/// Course fetching, parsing and whole-list validation finish before any database
/// writes. Group IDs are exact, opaque source identities, never internal CourseIds.
/// A missing binding creates a course using `group_name` and an empty, unknown
/// schedule, atomically with its source binding. Existing course names and
/// schedules are preserved; source metadata and children are not interpreted.
/// Each group uses [`sync_webclass_group_once`] and its existing batch transaction.
/// No SQLite transaction spans an HTTP request or the entire operation.
/// Before discovery, a short transaction records validated course-list freshness.
/// This timestamp means only successful list fetch/parse/identity validation and
/// metadata commit, not completion of course binding creation or all contents.
/// Later discovery/group failures preserve it. Each contents scope commits only
/// on its own successful import/observation; no global WebClass timestamp exists.
///
/// Stops at the first failure. Previously committed courses and group imports
/// remain; a course discovered for the failing contents request also remains.
/// Later courses/groups are untouched. Failed course creation or assignment
/// import rolls back only its own persistence operation. Errors return no partial
/// summary. Authentication expiry at either endpoint is returned without login or
/// retry. Repeating the operation reuses bindings and assignments. Missing courses
/// or candidates in a later response never cause deletion or presence updates.
pub fn sync_webclass_all_once(
    client: &WebClassClient,
    store: &mut SqliteStore,
) -> Result<WebClassSyncSummary, WebClassSyncError> {
    let course_list = acquire_webclass_courses(client)?;
    store
        .record_webclass_course_list_observation(&course_list.observation, course_list.observed_at)
        .map_err(|_| WebClassSyncError::CoursePersistence)?;
    let mut summary = WebClassSyncSummary {
        courses_discovered: course_list.courses.len(),
        ..Default::default()
    };
    visit_webclass_groups(client, &course_list.courses, |event| {
        match event {
            WebClassEvent::Course(course) => {
                let key = &course.external_key;
                if store
                    .find_webclass_course_id(&key.id)
                    .map_err(|_| WebClassSyncError::CoursePersistence)?
                    .is_none()
                {
                    store
                        .create_course_with_source(
                            &course.name,
                            vec![],
                            key.origin,
                            &key.scope,
                            &key.id,
                        )
                        .map_err(|_| WebClassSyncError::CoursePersistence)?;
                    summary.courses_created += 1;
                }
            }
            WebClassEvent::Assignments(assignments) => {
                let imported = import_webclass_assignments(store, &assignments)?;
                summary.groups_synced += 1;
                summary.assignments_created += imported.created;
                summary.assignments_updated += imported.updated;
                summary.assignments_unchanged += imported.unchanged;
            }
        }
        Ok(())
    })?;
    Ok(summary)
}

/// Obtain credentials only when requested and lend them to one login attempt.
///
/// Providers acquire their owned secret values inside this method and release them
/// before returning. Invoke the callback exactly once on successful acquisition,
/// or return `None` without invoking it when unavailable, cancelled or failed.
/// The callback accepts any credential lifetime; its result cannot borrow those
/// credentials. No Debug, Clone, serialization or credential storage is required.
///
/// Credentials cannot escape through the callback's result:
/// ```compile_fail
/// use assignment_manager::sync::WebClassCredentialProvider;
/// use assignment_manager::webclass_auth::HoseiCredentials;
/// fn retain_credentials(
///     provider: &mut impl WebClassCredentialProvider,
/// ) -> HoseiCredentials<'_> {
///     provider.with_credentials(|credentials| credentials).unwrap()
/// }
/// ```
pub trait WebClassCredentialProvider {
    fn with_credentials<R>(
        &mut self,
        use_credentials: impl FnOnce(HoseiCredentials<'_>) -> R,
    ) -> Option<R>;
}

/// Safe classifications only. `Sync` retains only the fixed sync category, not a cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebClassReauthSyncError {
    Sync(WebClassSyncError),
    CredentialsUnavailable,
    AuthenticationFailed,
}

impl fmt::Display for WebClassReauthSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(category) => category.fmt(f),
            Self::CredentialsUnavailable => f.write_str("WebClass sync credentials unavailable"),
            Self::AuthenticationFailed => f.write_str("WebClass sync reauthentication failed"),
        }
    }
}

// Even the safe sync category is not exposed as an underlying error chain.
impl Error for WebClassReauthSyncError {}

/// Synchronize with the current client, recovering from authentication expiry once.
///
/// Only a first-attempt [`WebClassSyncError::AuthExpired`] acquires credentials
/// and calls [`HoseiAuthenticator::login`]. Credential or login failure preserves
/// the old client. Successful login replaces the caller's client before retrying
/// [`sync_webclass_all_once`], and it stays replaced even if that retry fails.
/// A second expiry or any other retry error is returned without another login.
/// Credentials are borrowed only inside the login callback, never retained in a
/// client, database, result or error. No transaction spans login or an HTTP fetch.
///
/// First-attempt commits remain and are safely reimported by the full retry.
/// Success returns only the final successful attempt's summary, never combined
/// partial counts from the expired attempt. Existing full-sync failure and
/// disappearance semantics remain unchanged.
pub fn sync_webclass_all_with_reauth(
    client: &mut WebClassClient,
    authenticator: &HoseiAuthenticator,
    credential_provider: &mut impl WebClassCredentialProvider,
    store: &mut SqliteStore,
) -> Result<WebClassSyncSummary, WebClassReauthSyncError> {
    run_with_webclass_reauth(client, authenticator, credential_provider, |client| {
        sync_webclass_all_once(client, store)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_errors_have_fixed_display_debug_and_no_source() {
        for (error, debug, display) in [
            (MoodleSyncError::Fetch, "Fetch", "Moodle sync fetch failed"),
            (MoodleSyncError::Parse, "Parse", "Moodle sync parse failed"),
            (
                MoodleSyncError::Validation,
                "Validation",
                "Moodle sync candidate validation failed",
            ),
            (
                MoodleSyncError::Persistence,
                "Persistence",
                "Moodle sync persistence failed",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn webclass_sync_errors_have_fixed_display_debug_and_no_source() {
        for (error, debug, display) in [
            (
                WebClassSyncError::AuthExpired,
                "AuthExpired",
                "WebClass sync authentication expired",
            ),
            (
                WebClassSyncError::Fetch,
                "Fetch",
                "WebClass sync fetch failed",
            ),
            (
                WebClassSyncError::Parse,
                "Parse",
                "WebClass sync parse failed",
            ),
            (
                WebClassSyncError::CourseValidation,
                "CourseValidation",
                "WebClass sync course list validation failed",
            ),
            (
                WebClassSyncError::CoursePersistence,
                "CoursePersistence",
                "WebClass sync course persistence failed",
            ),
            (
                WebClassSyncError::Map,
                "Map",
                "WebClass sync mapping failed",
            ),
            (
                WebClassSyncError::Validation,
                "Validation",
                "WebClass sync candidate validation failed",
            ),
            (
                WebClassSyncError::Persistence,
                "Persistence",
                "WebClass sync persistence failed",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn reauth_sync_errors_have_fixed_display_debug_and_no_source() {
        for category in [
            WebClassSyncError::AuthExpired,
            WebClassSyncError::Fetch,
            WebClassSyncError::Parse,
            WebClassSyncError::CourseValidation,
            WebClassSyncError::CoursePersistence,
            WebClassSyncError::Map,
            WebClassSyncError::Validation,
            WebClassSyncError::Persistence,
        ] {
            let error = WebClassReauthSyncError::Sync(category);
            assert_eq!(format!("{error:?}"), format!("Sync({category:?})"));
            assert_eq!(error.to_string(), category.to_string());
            assert!(error.source().is_none());
        }
        for (error, debug, display) in [
            (
                WebClassReauthSyncError::CredentialsUnavailable,
                "CredentialsUnavailable",
                "WebClass sync credentials unavailable",
            ),
            (
                WebClassReauthSyncError::AuthenticationFailed,
                "AuthenticationFailed",
                "WebClass sync reauthentication failed",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert!(error.source().is_none());
        }
    }
}
