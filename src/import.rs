//! Import contracts shared by all sources and the future persistence layer.
//!
//! Persistence must accept an [`AssignmentCandidateBatch`] rather than unchecked
//! candidates. A batch contains assignment candidates, not a source snapshot:
//! candidate absence does not imply source disappearance and must never trigger
//! deletion. Future presence / last_seen tracking must use identities observed in
//! source models before classification or visibility filtering.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{
    Assignment, AssignmentId, CompletionState, CourseId, Deadline, SubmissionTarget,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OriginKind {
    Moodle,
    WebClass,
    Manual,
}

/// An external identifier qualified by its origin and namespace, not an application ID.
///
/// All identity strings are opaque and retained exactly, including surrounding
/// whitespace, case and leading zeroes. Validation never normalizes them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalRecordKey {
    pub origin: OriginKind,
    /// The namespace in which the external ID is unique; never an internal CourseId.
    ///
    /// Moodle uses a stable, non-secret feed label (not a URL or token), with a
    /// VEVENT UID as `id`. WebClass uses `group_id`, with `contents_id` as `id`.
    /// The WebClass pair is treated as unique within the initial single-university,
    /// single-user-profile database; it is not a multi-profile identity contract.
    pub scope: String,
    pub id: String,
}

/// Data awaiting a future merge; no internal AssignmentId has been assigned yet.
///
/// Candidates represent persistable assignment data, not all observed external
/// records. Hidden / unsupported WebClass contents and unclassified Moodle events
/// can still exist in the source without appearing here. Do not delete an existing
/// assignment because its key is absent from a candidate batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentCandidate {
    pub external_key: ExternalRecordKey,
    /// Source-owned: replaces the existing title during an update.
    pub title: String,
    /// Source-owned: replaces the existing deadline, including with `None`.
    pub deadline: Option<Deadline>,
    /// `Unknown` supplies no update; `None` explicitly means submission is not required.
    pub submission: SubmissionTarget,
    /// The actual page where the user can submit or answer, if known.
    /// `None` means not obtained / unknown, not an instruction to clear an existing URL.
    pub submission_url: Option<String>,
}

impl AssignmentCandidate {
    /// Construct a new assignment after batch validation and external-key lookup.
    ///
    /// The application / persistence layer supplies a newly generated internal ID
    /// and any explicitly resolved course relation. Source parsers and mappers do
    /// not generate either ID. Local completion always starts as `Incomplete`.
    pub fn into_assignment(
        self,
        id: AssignmentId,
        resolved_course_id: Option<CourseId>,
    ) -> Assignment {
        Assignment {
            id,
            course_id: resolved_course_id,
            title: self.title,
            deadline: self.deadline,
            submission: self.submission,
            submission_url: self.submission_url,
            completion: CompletionState::Incomplete,
        }
    }

    /// Apply the field ownership policy to an assignment already matched by external key.
    ///
    /// Callers validate the batch and resolve the external-key match before using
    /// this helper; it performs no lookup, course matching or persistence.
    /// `title` and `deadline` are source-owned and replaced. `id` and local
    /// `completion` are preserved, regardless of source scores or answer timestamps.
    /// A resolved course replaces the relation; unresolved (`None`) preserves it.
    /// A known submission target (including explicit `SubmissionTarget::None`)
    /// replaces the target; `Unknown` preserves it. A known URL replaces the URL;
    /// `None` preserves it. Clearing a course or URL requires a separate explicit
    /// contract and cannot be inferred from missing import information.
    pub fn apply_source_update(
        &self,
        assignment: &mut Assignment,
        resolved_course_id: Option<CourseId>,
    ) {
        assignment.title.clone_from(&self.title);
        assignment.deadline.clone_from(&self.deadline);
        if let Some(course_id) = resolved_course_id {
            assignment.course_id = Some(course_id);
        }
        if self.submission != SubmissionTarget::Unknown {
            assignment.submission = self.submission;
        }
        if let Some(url) = &self.submission_url {
            assignment.submission_url = Some(url.clone());
        }
    }
}

/// Safe classifications only: no raw identity, URL, response body or source error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportValidationError {
    EmptyExternalScope,
    EmptyExternalId,
    DuplicateExternalIdentity,
    WebClassGroupMismatch,
}

impl fmt::Display for ImportValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyExternalScope => "external scope is empty or whitespace-only",
            Self::EmptyExternalId => "external ID is empty or whitespace-only",
            Self::DuplicateExternalIdentity => "duplicate external identity in import batch",
            Self::WebClassGroupMismatch => "WebClass response group does not match requested group",
        })
    }
}

impl Error for ImportValidationError {}

/// Validated assignment candidates for the future persistence boundary, from any source.
///
/// Construction rejects empty / whitespace-only scope or ID and duplicate complete
/// keys. Values and order are retained exactly. There is no mutable access or
/// deserialization that could bypass validation. Source mappers may still return
/// unchecked candidates; all import paths (including manual imports) must construct
/// this batch before persistence. Combining batches requires validating again.
///
/// This is not a source snapshot. Even an empty batch cannot establish that source
/// records disappeared, and must not cause deletion or archiving.
///
/// ```
/// use assignment_manager::domain::SubmissionTarget;
/// use assignment_manager::import::{
///     AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey, OriginKind,
/// };
///
/// let batch = AssignmentCandidateBatch::try_new(vec![AssignmentCandidate {
///     external_key: ExternalRecordKey {
///         origin: OriginKind::Manual,
///         scope: "manual-import".into(),
///         id: " opaque-id ".into(),
///     },
///     title: "Report".into(),
///     deadline: None,
///     submission: SubmissionTarget::Unknown,
///     submission_url: None,
/// }])?;
/// assert_eq!(batch.as_slice()[0].external_key.id, " opaque-id ");
/// # Ok::<(), assignment_manager::import::ImportValidationError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentCandidateBatch {
    candidates: Vec<AssignmentCandidate>,
}

impl AssignmentCandidateBatch {
    pub fn try_new(candidates: Vec<AssignmentCandidate>) -> Result<Self, ImportValidationError> {
        let mut keys = HashSet::with_capacity(candidates.len());
        for candidate in &candidates {
            let key = &candidate.external_key;
            if key.scope.trim().is_empty() {
                return Err(ImportValidationError::EmptyExternalScope);
            }
            if key.id.trim().is_empty() {
                return Err(ImportValidationError::EmptyExternalId);
            }
            if !keys.insert(key) {
                return Err(ImportValidationError::DuplicateExternalIdentity);
            }
        }
        Ok(Self { candidates })
    }

    pub fn as_slice(&self) -> &[AssignmentCandidate] {
        &self.candidates
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use super::*;

    fn candidate(scope: &str, id: &str) -> AssignmentCandidate {
        AssignmentCandidate {
            external_key: ExternalRecordKey {
                origin: OriginKind::WebClass,
                scope: scope.into(),
                id: id.into(),
            },
            title: "Report".into(),
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        }
    }

    fn existing_assignment() -> Assignment {
        Assignment {
            id: AssignmentId::new("assignment-1"),
            course_id: Some(CourseId::new("course-1")),
            title: "Old title".into(),
            deadline: Some(Deadline::DateOnly(
                chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            )),
            submission: SubmissionTarget::WebClass,
            submission_url: Some("https://submit.example.invalid/old".into()),
            completion: CompletionState::Completed,
        }
    }

    #[test]
    fn internal_ids_and_external_keys_have_distinct_types() {
        assert_ne!(TypeId::of::<AssignmentId>(), TypeId::of::<CourseId>());
        assert_ne!(
            TypeId::of::<AssignmentId>(),
            TypeId::of::<ExternalRecordKey>()
        );
        assert_ne!(TypeId::of::<CourseId>(), TypeId::of::<ExternalRecordKey>());
    }

    #[test]
    fn external_key_is_qualified_by_origin_and_scope() {
        let key = ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: "feed-a".into(),
            id: "shared-id".into(),
        };
        let different_scope = ExternalRecordKey {
            scope: "feed-b".into(),
            ..key.clone()
        };
        let different_origin = ExternalRecordKey {
            origin: OriginKind::WebClass,
            ..key.clone()
        };

        assert_ne!(key, different_scope);
        assert_ne!(key, different_origin);
    }

    #[test]
    fn candidate_exists_before_an_internal_id_is_assigned() {
        let candidate = AssignmentCandidate {
            external_key: ExternalRecordKey {
                origin: OriginKind::WebClass,
                scope: "group-1".into(),
                id: "record-1".into(),
            },
            title: "Report".into(),
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        };

        assert_eq!(candidate.external_key.id, "record-1");
        assert_eq!(candidate.title, "Report");
    }

    #[test]
    fn batch_rejects_empty_and_whitespace_only_scope() {
        for scope in ["", "   ", "\t", "\n", " \t\n", "\u{3000}"] {
            assert_eq!(
                AssignmentCandidateBatch::try_new(vec![candidate(scope, "content-1")]),
                Err(ImportValidationError::EmptyExternalScope)
            );
        }
    }

    #[test]
    fn batch_rejects_empty_and_whitespace_only_id() {
        for id in ["", "   ", "\t", "\n", " \t\n", "\u{3000}"] {
            assert_eq!(
                AssignmentCandidateBatch::try_new(vec![candidate("group-1", id)]),
                Err(ImportValidationError::EmptyExternalId)
            );
        }
    }

    #[test]
    fn batch_preserves_opaque_identity_and_candidate_order() {
        let candidates = vec![
            candidate(" AbC/0001\t ", " 00042@Example.COM "),
            candidate(" AbC/0001\t ", "00042@example.com"),
            candidate("AbC/0001", " 00042@Example.COM "),
        ];
        let batch = AssignmentCandidateBatch::try_new(candidates.clone()).unwrap();
        assert_eq!(batch.as_slice(), candidates);
    }

    #[test]
    fn duplicate_key_rejects_the_whole_batch_regardless_of_order_or_payload() {
        let first = candidate("group-1", "content-10");
        let mut second = first.clone();
        second.title = "Changed title".into();
        for candidates in [vec![first.clone(), second.clone()], vec![second, first]] {
            assert_eq!(
                AssignmentCandidateBatch::try_new(candidates),
                Err(ImportValidationError::DuplicateExternalIdentity)
            );
        }
    }

    #[test]
    fn same_id_in_different_scopes_is_not_duplicate() {
        let batch = AssignmentCandidateBatch::try_new(vec![
            candidate("group-1", "content-10"),
            candidate("group-2", "content-10"),
        ])
        .unwrap();
        assert_eq!(batch.as_slice().len(), 2);
    }

    #[test]
    fn same_scope_and_id_from_different_origins_is_not_duplicate() {
        let first = candidate("scope-1", "id-1");
        let mut second = first.clone();
        second.external_key.origin = OriginKind::Moodle;
        let batch = AssignmentCandidateBatch::try_new(vec![first, second]).unwrap();
        assert_eq!(batch.as_slice().len(), 2);
    }

    #[test]
    fn manual_import_also_requires_identity_validation() {
        let mut manual = candidate("manual-import", "\t");
        manual.external_key.origin = OriginKind::Manual;
        assert_eq!(
            AssignmentCandidateBatch::try_new(vec![manual]),
            Err(ImportValidationError::EmptyExternalId)
        );
    }

    #[test]
    fn empty_batch_is_valid_but_is_not_a_source_snapshot() {
        let batch = AssignmentCandidateBatch::try_new(vec![]).unwrap();
        assert!(batch.as_slice().is_empty());
        // A batch exposes candidates only, with no disappearance / deletion decision.
    }

    #[test]
    fn validation_errors_have_fixed_debug_display_and_no_source() {
        for (error, debug, display) in [
            (
                ImportValidationError::EmptyExternalScope,
                "EmptyExternalScope",
                "external scope is empty or whitespace-only",
            ),
            (
                ImportValidationError::EmptyExternalId,
                "EmptyExternalId",
                "external ID is empty or whitespace-only",
            ),
            (
                ImportValidationError::DuplicateExternalIdentity,
                "DuplicateExternalIdentity",
                "duplicate external identity in import batch",
            ),
            (
                ImportValidationError::WebClassGroupMismatch,
                "WebClassGroupMismatch",
                "WebClass response group does not match requested group",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn new_assignment_uses_supplied_ids_candidate_fields_and_incomplete_state() {
        let mut candidate = candidate("group-1", "content-1");
        candidate.deadline = existing_assignment().deadline;
        candidate.submission = SubmissionTarget::External;
        candidate.submission_url = Some("https://submit.example.invalid/new".into());
        let id = AssignmentId::new("application-generated-id");
        let course_id = CourseId::new("resolved-course");
        let assignment = candidate
            .clone()
            .into_assignment(id.clone(), Some(course_id.clone()));

        assert_eq!(assignment.id, id);
        assert_eq!(assignment.course_id, Some(course_id));
        assert_eq!(assignment.title, candidate.title);
        assert_eq!(assignment.deadline, candidate.deadline);
        assert_eq!(assignment.submission, candidate.submission);
        assert_eq!(assignment.submission_url, candidate.submission_url);
        assert_eq!(assignment.completion, CompletionState::Incomplete);
    }

    #[test]
    fn source_update_replaces_title_and_deadline_but_preserves_id_and_completion() {
        for completion in [CompletionState::Incomplete, CompletionState::Completed] {
            let mut assignment = existing_assignment();
            assignment.completion = completion;
            let id = assignment.id.clone();
            let mut update = candidate("group-1", "content-1");
            update.deadline = Some(Deadline::DateOnly(
                chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
            ));
            update.apply_source_update(&mut assignment, None);

            assert_eq!(assignment.title, update.title);
            assert_eq!(assignment.deadline, update.deadline);
            assert_eq!(assignment.id, id);
            assert_eq!(assignment.completion, completion);

            update.deadline = None;
            update.apply_source_update(&mut assignment, None);
            assert_eq!(assignment.deadline, None);
        }
    }

    #[test]
    fn unresolved_course_preserves_existing_relation_and_resolved_course_updates_it() {
        let mut assignment = existing_assignment();
        let original_course = assignment.course_id.clone();
        let update = candidate("group-1", "content-1");
        update.apply_source_update(&mut assignment, None);
        assert_eq!(assignment.course_id, original_course);

        let resolved = CourseId::new("resolved-course");
        update.apply_source_update(&mut assignment, Some(resolved.clone()));
        assert_eq!(assignment.course_id, Some(resolved));
    }

    #[test]
    fn unknown_submission_preserves_every_known_target() {
        for target in [
            SubmissionTarget::None,
            SubmissionTarget::Moodle,
            SubmissionTarget::WebClass,
            SubmissionTarget::External,
        ] {
            let mut assignment = existing_assignment();
            assignment.submission = target;
            candidate("group-1", "content-1").apply_source_update(&mut assignment, None);
            assert_eq!(assignment.submission, target);
        }
    }

    #[test]
    fn known_submission_including_explicit_none_updates_the_target() {
        for target in [
            SubmissionTarget::None,
            SubmissionTarget::Moodle,
            SubmissionTarget::WebClass,
            SubmissionTarget::External,
        ] {
            let mut assignment = existing_assignment();
            let mut update = candidate("group-1", "content-1");
            update.submission = target;
            update.apply_source_update(&mut assignment, None);
            assert_eq!(assignment.submission, target);
        }
    }

    #[test]
    fn unknown_submission_url_preserves_existing_url_and_known_url_updates_it() {
        let mut assignment = existing_assignment();
        let original_url = assignment.submission_url.clone();
        let mut update = candidate("group-1", "content-1");
        update.apply_source_update(&mut assignment, None);
        assert_eq!(assignment.submission_url, original_url);

        update.submission_url = Some("https://submit.example.invalid/new".into());
        update.apply_source_update(&mut assignment, None);
        assert_eq!(assignment.submission_url, update.submission_url);
    }
}
