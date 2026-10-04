//! One-shot application operations connecting source clients to validated persistence.
//!
//! Fetching, parsing, mapping and validation finish before the store starts its
//! import transaction. Candidate absence has no deletion or presence semantics.

use std::error::Error;
use std::fmt;

use crate::import::AssignmentCandidateBatch;
use crate::moodle::{map_events, parse_calendar};
use crate::moodle_http::{MoodleClient, MoodleFeed};
use crate::persistence::{AssignmentImportSummary, SqliteStore};

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
}
