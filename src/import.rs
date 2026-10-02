use serde::{Deserialize, Serialize};

use crate::domain::{Deadline, SubmissionTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginKind {
    Moodle,
    WebClass,
    Manual,
}

/// An external identifier qualified by its origin and namespace, not an application ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRecordKey {
    pub origin: OriginKind,
    pub scope: String,
    pub id: String,
}

/// Data awaiting a future merge; no internal AssignmentId has been assigned yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentCandidate {
    pub external_key: ExternalRecordKey,
    pub title: String,
    pub deadline: Option<Deadline>,
    pub submission: SubmissionTarget,
    /// The actual page where the user can submit or answer, if known.
    pub submission_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use super::*;
    use crate::domain::{AssignmentId, CourseId};

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
                scope: "university-account".into(),
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
}
