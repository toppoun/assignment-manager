//! Positive source identities observed before candidate mapping or filtering.
//!
//! A validated observation is not a complete source snapshot. Neither missing
//! identities nor an empty successful response imply disappearance or deletion.
//! Construct these values only after a successful whole-response parse; failures
//! must never be converted to empty observations.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::import::{AssignmentCandidateBatch, ExternalRecordKey, OriginKind};

/// A non-secret processing scope in the current single-profile database.
/// String identities are opaque; validation rejects blanks without normalizing.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SyncScope {
    /// The stable `MoodleFeed::scope()` label, never a feed URL or token.
    MoodleFeed(String),
    /// Observation of the course list, not completion of all contents imports.
    WebClassCourses,
    WebClassContents(String),
}

impl SyncScope {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        match self {
            Self::MoodleFeed(value) | Self::WebClassContents(value) if value.trim().is_empty() => {
                Err(ObservationValidationError::EmptyScope)
            }
            _ => Ok(()),
        }
    }

    // Explicit storage codes, independent of enum order or Debug rendering.
    pub(crate) fn storage_key(&self) -> Result<(&'static str, &str), ObservationValidationError> {
        self.validate()?;
        Ok(match self {
            Self::MoodleFeed(scope) => ("moodle_feed", scope),
            Self::WebClassCourses => ("webclass_courses", "default"),
            Self::WebClassContents(scope) => ("webclass_contents", scope),
        })
    }

    fn assignment_namespace(&self) -> Result<(OriginKind, &str), ObservationValidationError> {
        self.validate()?;
        match self {
            Self::MoodleFeed(scope) => Ok((OriginKind::Moodle, scope)),
            Self::WebClassContents(scope) => Ok((OriginKind::WebClass, scope)),
            Self::WebClassCourses => Err(ObservationValidationError::NotAssignmentScope),
        }
    }
}

impl fmt::Debug for SyncScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MoodleFeed(_) => "MoodleFeed",
            Self::WebClassCourses => "WebClassCourses",
            Self::WebClassContents(_) => "WebClassContents",
        })
    }
}

/// Fixed classifications only, without source values or underlying errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationValidationError {
    EmptyScope,
    NotAssignmentScope,
    EmptyIdentity,
    DuplicateIdentity,
    OriginMismatch,
    ScopeMismatch,
    CandidateNotObserved,
}

impl fmt::Display for ObservationValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyScope => "observation scope is empty or whitespace-only",
            Self::NotAssignmentScope => "observation scope does not contain assignments",
            Self::EmptyIdentity => "observed identity is empty or whitespace-only",
            Self::DuplicateIdentity => "duplicate observed identity",
            Self::OriginMismatch => "observed origin does not match scope kind",
            Self::ScopeMismatch => "observed scope does not match requested scope",
            Self::CandidateNotObserved => "assignment candidate was not observed",
        })
    }
}

impl Error for ObservationValidationError {}

/// All validated identities from a parsed feed or requested contents group.
/// Includes hidden, unsupported and otherwise filtered records. It does not
/// claim completeness. No mutable access or deserialization bypasses validation.
#[derive(Clone, PartialEq, Eq)]
pub struct AssignmentObservation {
    scope: SyncScope,
    keys: Vec<ExternalRecordKey>,
}

impl AssignmentObservation {
    pub fn try_new(
        scope: SyncScope,
        keys: Vec<ExternalRecordKey>,
    ) -> Result<Self, ObservationValidationError> {
        let (origin, namespace) = scope.assignment_namespace()?;
        let mut unique = HashSet::with_capacity(keys.len());
        for key in &keys {
            if key.scope.trim().is_empty() {
                return Err(ObservationValidationError::EmptyScope);
            }
            if key.id.trim().is_empty() {
                return Err(ObservationValidationError::EmptyIdentity);
            }
            if key.origin != origin {
                return Err(ObservationValidationError::OriginMismatch);
            }
            if key.scope != namespace {
                return Err(ObservationValidationError::ScopeMismatch);
            }
            if !unique.insert(key) {
                return Err(ObservationValidationError::DuplicateIdentity);
            }
        }
        Ok(Self { scope, keys })
    }

    pub fn scope(&self) -> &SyncScope {
        &self.scope
    }

    pub fn keys(&self) -> &[ExternalRecordKey] {
        &self.keys
    }

    /// Candidates may be a proper subset, but may never introduce unobserved keys.
    pub fn validate_candidates(
        &self,
        batch: &AssignmentCandidateBatch,
    ) -> Result<(), ObservationValidationError> {
        let keys: HashSet<_> = self.keys.iter().collect();
        if batch
            .as_slice()
            .iter()
            .any(|candidate| !keys.contains(&candidate.external_key))
        {
            return Err(ObservationValidationError::CandidateNotObserved);
        }
        Ok(())
    }
}

impl fmt::Debug for AssignmentObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssignmentObservation")
            .field("scope_kind", &self.scope)
            .field("record_count", &self.keys.len())
            .finish()
    }
}

/// Validated course-list identities, separate from assignment identities.
/// Used only for course-list freshness, not course binding observation or absence.
#[derive(Clone, PartialEq, Eq)]
pub struct WebClassCourseListObservation {
    group_ids: Vec<String>,
}

impl WebClassCourseListObservation {
    pub fn try_new(group_ids: Vec<String>) -> Result<Self, ObservationValidationError> {
        let mut unique = HashSet::with_capacity(group_ids.len());
        for id in &group_ids {
            if id.trim().is_empty() {
                return Err(ObservationValidationError::EmptyIdentity);
            }
            if !unique.insert(id) {
                return Err(ObservationValidationError::DuplicateIdentity);
            }
        }
        Ok(Self { group_ids })
    }

    pub fn group_ids(&self) -> &[String] {
        &self.group_ids
    }
}

impl fmt::Debug for WebClassCourseListObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebClassCourseListObservation")
            .field("record_count", &self.group_ids.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SubmissionTarget;
    use crate::import::AssignmentCandidate;

    fn key(origin: OriginKind, scope: &str, id: &str) -> ExternalRecordKey {
        ExternalRecordKey {
            origin,
            scope: scope.into(),
            id: id.into(),
        }
    }

    #[test]
    fn scope_codes_are_explicit_and_opaque_values_are_preserved() {
        for (scope, expected) in [
            (
                SyncScope::MoodleFeed(" 000.A ".into()),
                ("moodle_feed", " 000.A "),
            ),
            (SyncScope::WebClassCourses, ("webclass_courses", "default")),
            (
                SyncScope::WebClassContents(" AbC/001 ".into()),
                ("webclass_contents", " AbC/001 "),
            ),
        ] {
            assert_eq!(scope.storage_key().unwrap(), expected);
        }
        for blank in ["", " \t\n", "\u{3000}"] {
            for scope in [
                SyncScope::MoodleFeed(blank.into()),
                SyncScope::WebClassContents(blank.into()),
            ] {
                assert_eq!(
                    scope.validate(),
                    Err(ObservationValidationError::EmptyScope)
                );
                assert_eq!(
                    scope.storage_key(),
                    Err(ObservationValidationError::EmptyScope)
                );
            }
        }
    }

    #[test]
    fn assignment_observation_checks_every_identity_even_without_candidates() {
        let scope = SyncScope::WebClassContents(" group-001 ".into());
        let valid = key(OriginKind::WebClass, " group-001 ", " 001 ");
        let observation =
            AssignmentObservation::try_new(scope.clone(), vec![valid.clone()]).unwrap();
        assert_eq!(observation.scope(), &scope);
        assert_eq!(observation.keys(), std::slice::from_ref(&valid));
        for (keys, error) in [
            (
                vec![key(OriginKind::WebClass, " group-001 ", "")],
                ObservationValidationError::EmptyIdentity,
            ),
            (
                vec![key(OriginKind::WebClass, " group-001 ", " \t")],
                ObservationValidationError::EmptyIdentity,
            ),
            (
                vec![key(OriginKind::WebClass, " ", "id")],
                ObservationValidationError::EmptyScope,
            ),
            (
                vec![key(OriginKind::Moodle, " group-001 ", "id")],
                ObservationValidationError::OriginMismatch,
            ),
            (
                vec![key(OriginKind::Manual, " group-001 ", "id")],
                ObservationValidationError::OriginMismatch,
            ),
            (
                vec![key(OriginKind::WebClass, "group-001", "id")],
                ObservationValidationError::ScopeMismatch,
            ),
            (
                vec![valid.clone(), valid],
                ObservationValidationError::DuplicateIdentity,
            ),
        ] {
            assert_eq!(
                AssignmentObservation::try_new(scope.clone(), keys),
                Err(error)
            );
        }
        assert_eq!(
            AssignmentObservation::try_new(SyncScope::WebClassCourses, vec![]),
            Err(ObservationValidationError::NotAssignmentScope)
        );
        let moodle = SyncScope::MoodleFeed("feed".into());
        assert!(
            AssignmentObservation::try_new(
                moodle.clone(),
                vec![key(OriginKind::Moodle, "feed", "uid")]
            )
            .is_ok()
        );
        for id in ["", " \t\n"] {
            assert_eq!(
                AssignmentObservation::try_new(
                    moodle.clone(),
                    vec![key(OriginKind::Moodle, "feed", id)]
                ),
                Err(ObservationValidationError::EmptyIdentity)
            );
        }
        let repeated = key(OriginKind::Moodle, "feed", "uid");
        assert_eq!(
            AssignmentObservation::try_new(moodle, vec![repeated.clone(), repeated]),
            Err(ObservationValidationError::DuplicateIdentity)
        );
    }

    #[test]
    fn candidates_must_be_a_subset_and_successful_empty_observations_are_valid() {
        let seen = key(OriginKind::Moodle, "feed", "uid");
        let observation = AssignmentObservation::try_new(
            SyncScope::MoodleFeed("feed".into()),
            vec![seen.clone()],
        )
        .unwrap();
        let candidate = |external_key| AssignmentCandidate {
            external_key,
            title: "synthetic".into(),
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: None,
        };
        assert!(
            observation
                .validate_candidates(&AssignmentCandidateBatch::try_new(vec![]).unwrap())
                .is_ok()
        );
        assert!(
            observation
                .validate_candidates(
                    &AssignmentCandidateBatch::try_new(vec![candidate(seen)]).unwrap()
                )
                .is_ok()
        );
        for unobserved in [
            key(OriginKind::Moodle, "feed", "other"),
            key(OriginKind::Moodle, "other", "uid"),
            key(OriginKind::WebClass, "feed", "uid"),
        ] {
            assert_eq!(
                observation.validate_candidates(
                    &AssignmentCandidateBatch::try_new(vec![candidate(unobserved)]).unwrap()
                ),
                Err(ObservationValidationError::CandidateNotObserved)
            );
        }
        assert!(
            AssignmentObservation::try_new(SyncScope::WebClassContents("group".into()), vec![])
                .unwrap()
                .keys()
                .is_empty()
        );
    }

    #[test]
    fn course_list_validation_is_separate_and_does_not_normalize_ids() {
        let ids = vec![" 001 ".into(), "001".into(), "AbC".into(), "abc".into()];
        assert_eq!(
            WebClassCourseListObservation::try_new(ids.clone())
                .unwrap()
                .group_ids(),
            ids
        );
        assert!(
            WebClassCourseListObservation::try_new(vec![])
                .unwrap()
                .group_ids()
                .is_empty()
        );
        for id in ["", "\u{3000}"] {
            assert_eq!(
                WebClassCourseListObservation::try_new(vec![id.into()]),
                Err(ObservationValidationError::EmptyIdentity)
            );
        }
        assert_eq!(
            WebClassCourseListObservation::try_new(vec!["id".into(), "id".into()]),
            Err(ObservationValidationError::DuplicateIdentity)
        );
    }

    #[test]
    fn debug_and_errors_retain_no_source_values_or_causes() {
        const PRIVATE: &str = "PRIVATE_OBSERVATION_UID_GROUP_URL";
        let scope = SyncScope::MoodleFeed(PRIVATE.into());
        let observation = AssignmentObservation::try_new(
            scope.clone(),
            vec![key(OriginKind::Moodle, PRIVATE, PRIVATE)],
        )
        .unwrap();
        let courses = WebClassCourseListObservation::try_new(vec![PRIVATE.into()]).unwrap();
        assert_eq!(format!("{scope:?}"), "MoodleFeed");
        for rendered in [
            format!("{scope:#?}"),
            format!("{observation:?} {observation:#?}"),
            format!("{courses:?} {courses:#?}"),
        ] {
            assert!(!rendered.contains(PRIVATE));
        }
        for (error, debug, display) in [
            (
                ObservationValidationError::EmptyScope,
                "EmptyScope",
                "observation scope is empty or whitespace-only",
            ),
            (
                ObservationValidationError::NotAssignmentScope,
                "NotAssignmentScope",
                "observation scope does not contain assignments",
            ),
            (
                ObservationValidationError::EmptyIdentity,
                "EmptyIdentity",
                "observed identity is empty or whitespace-only",
            ),
            (
                ObservationValidationError::DuplicateIdentity,
                "DuplicateIdentity",
                "duplicate observed identity",
            ),
            (
                ObservationValidationError::OriginMismatch,
                "OriginMismatch",
                "observed origin does not match scope kind",
            ),
            (
                ObservationValidationError::ScopeMismatch,
                "ScopeMismatch",
                "observed scope does not match requested scope",
            ),
            (
                ObservationValidationError::CandidateNotObserved,
                "CandidateNotObserved",
                "assignment candidate was not observed",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert!(error.source().is_none());
        }
    }
}
