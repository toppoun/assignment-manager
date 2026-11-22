//! Notion synchronization planning, read-only dry runs, and guarded apply.

mod apply;
mod client;
#[cfg(feature = "synthetic-https-fixture")]
mod course_trial;
#[cfg(feature = "synthetic-https-fixture")]
pub mod fixture;
#[cfg(test)]
mod http_tests;
mod inspection;
mod plan;
mod reader;
pub mod state;
#[cfg(test)]
mod tests;
mod trial;
mod wire;

pub use apply::{ApplyError, ApplyPreview, ApplyReport};
pub use apply::{MOODLE_INTEGRATION_SCOPE, MoodlePreviewCoverage, MoodlePreviewMappings};
#[cfg(feature = "synthetic-https-fixture")]
pub use apply::{MoodleCreatePreview, MoodleUpdatePreview};
pub use client::{NOTION_VERSION, NotionClient, NotionError};
#[cfg(feature = "synthetic-https-fixture")]
pub use course_trial::{CourseCreationTrial, CourseSnapshot, CourseTrialError};
pub use inspection::{InspectionReport, PropertyFinding, PropertyStatus, SnapshotSummary};
pub use plan::{
    AssignmentPatch, ConflictReason, CourseMapping, DeferredReason, NotionAssignment,
    NotionSnapshot, PlanAction, PlannedAssignment, plan_sync,
};
pub use reader::{DryRunCounts, DryRunReport};
#[cfg(feature = "synthetic-https-fixture")]
pub use trial::MoodleCreateTarget;
pub use trial::{TrialAssignment, TrialRole, TrialTarget, VerificationTarget};
pub use wire::{NotionPropertyIds, NotionTarget, PreparedWrite};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map::Entry},
    error::Error,
    fmt::{self, Write as _},
    str::FromStr,
};

use aws_lc_rs::digest::{Context, SHA256};

use crate::{
    domain::Deadline,
    import::{AssignmentCandidate, DEFAULT_COURSE_SOURCE_SCOPE, ExternalRecordKey, OriginKind},
    observation::SyncScope,
    sync::{MoodleAcquisition, WebClassAcquisition},
};

const KEY_PREFIX: &str = "v1:";

/// Canonical key for the current single-profile identity contract.
///
/// V1 hashes a domain tag, a one-byte source discriminator, then the exact
/// UTF-8 scope and ID, each prefixed with its byte length as a big-endian u64.
/// The text format is the version prefix followed by 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyncKey(String);

impl SyncKey {
    fn derive(key: &ExternalRecordKey) -> Result<Self, InvalidSourceData> {
        let source_tag = match key.origin {
            OriginKind::Moodle => 1,
            OriginKind::WebClass => 2,
            OriginKind::Manual => return Err(InvalidSourceData::UnexpectedOrigin),
        };
        if key.scope.trim().is_empty() || key.id.trim().is_empty() {
            return Err(InvalidSourceData::InvalidIdentity);
        }

        let mut hash = Context::new(&SHA256);
        hash.update(b"assignment-manager/notion-sync\0\x01");
        hash.update(&[source_tag]);
        for part in [&key.scope, &key.id] {
            hash.update(&(part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        let mut text = String::with_capacity(KEY_PREFIX.len() + 64);
        text.push_str(KEY_PREFIX);
        for byte in hash.finish().as_ref() {
            write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(feature = "synthetic-https-fixture")]
    fn fixture_moodle(uid: &str) -> Result<Self, InvalidSourceData> {
        Self::derive(&ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: crate::webclass_auth::synthetic::MOODLE_SCOPE.into(),
            id: uid.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSyncKey;

impl fmt::Display for InvalidSyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid or unsupported synchronization key")
    }
}

impl Error for InvalidSyncKey {}

impl FromStr for SyncKey {
    type Err = InvalidSyncKey;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value.strip_prefix(KEY_PREFIX).ok_or(InvalidSyncKey)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(InvalidSyncKey);
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniversitySource {
    Moodle,
    WebClass,
}

/// Only source-owned data. The current acquisition contract leaves the URL unknown.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncAssignment {
    pub source: UniversitySource,
    pub title: String,
    pub deadline: Option<Deadline>,
    pub course_key: Option<ExternalRecordKey>,
    pub submission_url: Option<String>,
}

impl fmt::Debug for SyncAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncAssignment")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Rejection categories contain no input values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidSourceData {
    UnexpectedOrigin,
    InvalidIdentity,
    InvalidScope,
    EmptyTitle,
    UnexpectedSubmissionUrl,
    DuplicateSyncKey,
}

impl fmt::Display for InvalidSourceData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnexpectedOrigin => "unexpected assignment origin",
            Self::InvalidIdentity => "invalid external identity",
            Self::InvalidScope => "invalid acquisition scope",
            Self::EmptyTitle => "empty assignment title",
            Self::UnexpectedSubmissionUrl => "submission URLs are not yet supported",
            Self::DuplicateSyncKey => {
                "duplicate synchronization key in a single acquisition attempt"
            }
        })
    }
}

impl Error for InvalidSourceData {}

/// Reconciled assignments from one run. The source reports remain available to
/// the caller for overall failure status, independently of individual groups.
#[derive(Debug, Default)]
pub struct SyncInput {
    assignments: BTreeMap<SyncKey, SyncAssignment>,
    earlier_attempt: BTreeSet<SyncKey>,
}

impl SyncInput {
    /// Supply Moodle's successful result, if available, and the WebClass run report.
    ///
    /// Only successful scopes in the latest actual WebClass fetch attempt yield
    /// writes. A retried scope supplies its new candidate set; keys found only in
    /// an earlier attempt are deferred, including keys missing in a successful
    /// retry. If reauthentication fails before fetching again, earlier successful
    /// groups in the sole fetch attempt are still usable. Source absence is never
    /// a deletion instruction.
    pub fn from_acquisitions(
        moodle: Option<&MoodleAcquisition>,
        webclass: Option<&WebClassAcquisition>,
    ) -> Result<Self, InvalidSourceData> {
        let mut input = Self::default();
        if let Some(moodle) = moodle {
            let assignments = moodle.assignments();
            let SyncScope::MoodleFeed(scope) = assignments.observation().scope() else {
                return Err(InvalidSourceData::InvalidScope);
            };
            for candidate in assignments.batch().as_slice() {
                let course = moodle
                    .course_for(&candidate.external_key)
                    .map(|course| &course.external_key);
                if course.is_some_and(|key| {
                    key.origin != OriginKind::Moodle
                        || key.scope != *scope
                        || key.id.trim().is_empty()
                }) {
                    return Err(InvalidSourceData::InvalidScope);
                }
                input.insert(candidate, UniversitySource::Moodle, course)?;
            }
        }
        if let Some(webclass) = webclass {
            input.add_webclass(webclass)?;
        }
        Ok(input)
    }

    pub fn assignments(&self) -> &BTreeMap<SyncKey, SyncAssignment> {
        &self.assignments
    }

    #[cfg(feature = "synthetic-https-fixture")]
    pub fn select_fixture_moodle(mut self, uid: &str) -> Result<Self, NotionError> {
        let key = SyncKey::fixture_moodle(uid).map_err(|_| NotionError::Configuration)?;
        let assignment = self
            .assignments
            .remove(&key)
            .ok_or(NotionError::Configuration)?;
        Ok(Self {
            assignments: BTreeMap::from([(key, assignment)]),
            earlier_attempt: BTreeSet::new(),
        })
    }

    fn add_webclass(&mut self, webclass: &WebClassAcquisition) -> Result<(), InvalidSourceData> {
        let (latest, earlier) = webclass
            .attempts()
            .split_last()
            .ok_or(InvalidSourceData::InvalidScope)?;
        for attempt in earlier {
            for group in attempt.groups() {
                for candidate in group.batch().as_slice() {
                    self.earlier_attempt
                        .insert(SyncKey::derive(&candidate.external_key)?);
                }
            }
        }

        let mut courses = HashMap::new();
        if let Some(list) = latest.course_list() {
            for course in list.courses() {
                let key = &course.external_key;
                if key.origin != OriginKind::WebClass
                    || key.scope != DEFAULT_COURSE_SOURCE_SCOPE
                    || key.id.trim().is_empty()
                    || courses.insert(key.id.as_str(), key).is_some()
                {
                    return Err(InvalidSourceData::InvalidScope);
                }
            }
        }
        let mut completed = HashSet::new();
        for group in latest.groups() {
            let SyncScope::WebClassContents(id) = group.observation().scope() else {
                return Err(InvalidSourceData::InvalidScope);
            };
            if !completed.insert(id) {
                return Err(InvalidSourceData::InvalidScope);
            }
            let course = courses
                .get(id.as_str())
                .ok_or(InvalidSourceData::InvalidScope)?;
            for candidate in group.batch().as_slice() {
                self.insert(candidate, UniversitySource::WebClass, Some(course))?;
            }
        }
        Ok(())
    }

    fn insert(
        &mut self,
        candidate: &AssignmentCandidate,
        source: UniversitySource,
        course: Option<&ExternalRecordKey>,
    ) -> Result<(), InvalidSourceData> {
        if candidate.title.trim().is_empty() {
            return Err(InvalidSourceData::EmptyTitle);
        }
        // A validated assignment batch does not certify URLs. Current mappers
        // always return None; reject unexpected URLs until a source policy exists.
        if candidate.submission_url.is_some() {
            return Err(InvalidSourceData::UnexpectedSubmissionUrl);
        }
        let key = SyncKey::derive(&candidate.external_key)?;
        self.earlier_attempt.remove(&key);
        let Entry::Vacant(entry) = self.assignments.entry(key) else {
            return Err(InvalidSourceData::DuplicateSyncKey);
        };
        entry.insert(SyncAssignment {
            source,
            title: candidate.title.clone(),
            deadline: candidate.deadline.clone(),
            course_key: course.cloned(),
            submission_url: None,
        });
        Ok(())
    }
}
