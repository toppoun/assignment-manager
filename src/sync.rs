//! One-shot application operations connecting source clients to validated persistence.
//!
//! Fetching, parsing, mapping and validation finish before the store starts its
//! import transaction. Candidate absence has no deletion or presence semantics.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::import::{AssignmentCandidateBatch, OriginKind};
use crate::moodle::{map_events, parse_calendar};
use crate::moodle_http::{MoodleClient, MoodleFeed};
use crate::persistence::{AssignmentImportSummary, SqliteStore};
use crate::webclass::{
    WebClassCourse, WebClassMapError, map_contents_for_group, parse_contents, parse_courses,
};
use crate::webclass_http::{WebClassClient, WebClassHttpError};

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
/// scope reaches the mapper; feed labels do not resolve courses. Pre-import
/// failures leave the database untouched. Import failures roll back the store's
/// batch transaction and never return a partial summary. An empty validated batch
/// returns zero counts without changing existing assignments.
pub fn sync_moodle_once(
    client: &MoodleClient,
    feed: &MoodleFeed,
    store: &mut SqliteStore,
) -> Result<AssignmentImportSummary, MoodleSyncError> {
    let raw = client
        .fetch_calendar(feed)
        .map_err(|_| MoodleSyncError::Fetch)?;
    let events = parse_calendar(&raw).map_err(|_| MoodleSyncError::Parse)?;
    let candidates = map_events(&events, feed.scope());
    let batch =
        AssignmentCandidateBatch::try_new(candidates).map_err(|_| MoodleSyncError::Validation)?;
    store
        .import_assignment_batch(&batch)
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
/// Empty batches return zero counts.
pub fn sync_webclass_group_once(
    client: &WebClassClient,
    group_id: &str,
    store: &mut SqliteStore,
) -> Result<AssignmentImportSummary, WebClassSyncError> {
    let raw = client
        .fetch_contents_json(group_id)
        .map_err(|error| match error {
            WebClassHttpError::AuthExpired => WebClassSyncError::AuthExpired,
            _ => WebClassSyncError::Fetch,
        })?;
    let contents = parse_contents(&raw).map_err(|_| WebClassSyncError::Parse)?;
    let batch = map_contents_for_group(&contents, group_id).map_err(|error| match error {
        WebClassMapError::InvalidEndDate => WebClassSyncError::Map,
        WebClassMapError::Validation(_) => WebClassSyncError::Validation,
    })?;
    store
        .import_assignment_batch(&batch)
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
    let raw = client.fetch_courses_json().map_err(|error| match error {
        WebClassHttpError::AuthExpired => WebClassSyncError::AuthExpired,
        _ => WebClassSyncError::Fetch,
    })?;
    let courses = parse_courses(&raw).map_err(|_| WebClassSyncError::Parse)?;
    validate_webclass_courses(&courses)?;
    let mut summary = WebClassSyncSummary {
        courses_discovered: courses.len(),
        ..Default::default()
    };
    for course in courses {
        if store
            .find_webclass_course_id(&course.group_id)
            .map_err(|_| WebClassSyncError::CoursePersistence)?
            .is_none()
        {
            store
                .create_course_with_source(
                    course.group_name,
                    vec![],
                    OriginKind::WebClass,
                    &course.group_id,
                )
                .map_err(|_| WebClassSyncError::CoursePersistence)?;
            summary.courses_created += 1;
        }
        let imported = sync_webclass_group_once(client, &course.group_id, store)?;
        summary.groups_synced += 1;
        summary.assignments_created += imported.created;
        summary.assignments_updated += imported.updated;
        summary.assignments_unchanged += imported.unchanged;
    }
    Ok(summary)
}

fn validate_webclass_courses(courses: &[WebClassCourse]) -> Result<(), WebClassSyncError> {
    let mut groups = HashSet::with_capacity(courses.len());
    for course in courses {
        if course.group_id.trim().is_empty() || !groups.insert(course.group_id.as_str()) {
            return Err(WebClassSyncError::CourseValidation);
        }
    }
    Ok(())
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
}
