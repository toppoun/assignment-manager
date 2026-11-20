use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
};

use crate::{domain::Deadline, import::ExternalRecordKey};
use serde::Deserialize;

use super::wire::uuid;
use super::{SyncAssignment, SyncInput, SyncKey, UniversitySource};

/// The read projection needed to compare source-owned properties. A row with
/// no sync key is manual; unrecognized nonempty keys must be handled by the
/// snapshot reader before marking the snapshot complete.
#[derive(Clone, PartialEq, Eq)]
pub struct NotionAssignment {
    pub page_id: String,
    pub sync_key: Option<SyncKey>,
    pub source: Option<UniversitySource>,
    pub title: String,
    pub deadline: Option<Deadline>,
    /// All page IDs in the course relation on a managed page.
    pub course_page_ids: Vec<String>,
}

pub enum NotionSnapshot<'a> {
    /// All managed rows in the query scope and all comparison fields were read.
    Complete(&'a [NotionAssignment]),
    /// Read error, incomplete pagination, or an unverified projection.
    Incomplete,
}

/// An explicit binding for an exact course key. Names are never matched.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CourseMapping {
    pub course_key: ExternalRecordKey,
    pub page_id: String,
}

impl CourseMapping {
    pub fn has_valid_page_id(&self) -> bool {
        uuid(&self.page_id).is_some()
    }
}

/// Only properties this phase can change. Omitted fields keep their values.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct AssignmentPatch {
    pub source: Option<UniversitySource>,
    pub title: Option<String>,
    /// None omits the property. Some(None) explicitly clears the deadline.
    pub deadline: Option<Option<Deadline>>,
    /// When present, fill a course relation that was empty in the complete snapshot.
    pub course_page_id: Option<String>,
}

impl fmt::Debug for AssignmentPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssignmentPatch")
            .field("source_changed", &self.source.is_some())
            .field("title_changed", &self.title.is_some())
            .field("deadline_changed", &self.deadline.is_some())
            .field("course_changed", &self.course_page_id.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    DuplicateSyncKey,
    InvalidOrRepeatedPage,
    AmbiguousCourseMapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredReason {
    IncompleteSnapshot,
    PendingReconciliation,
    /// Only an earlier WebClass attempt produced this candidate.
    EarlierAttempt,
    /// The run has no current candidate. Includes failed, skipped, filtered and absent records.
    NoValidatedCandidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Create {
        assignment: SyncAssignment,
        course_page_id: Option<String>,
    },
    Update {
        page_id: String,
        patch: AssignmentPatch,
    },
    Unchanged {
        page_id: String,
    },
    Conflict(ConflictReason),
    Deferred(DeferredReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAssignment {
    pub key: SyncKey,
    pub action: PlanAction,
    /// A populated relation differs from the single mapped course page.
    pub course_relation_mismatch: bool,
}

/// Produce one action per managed synchronization key in lexicographic key
/// order. Conflicts suppress writes for the affected keys. An incomplete
/// snapshot suppresses all writes. Manual rows never produce an operation.
pub fn plan_sync(
    input: &SyncInput,
    snapshot: NotionSnapshot<'_>,
    course_mappings: &[CourseMapping],
) -> Vec<PlannedAssignment> {
    let mut keys: BTreeSet<_> = input
        .assignments
        .keys()
        .chain(&input.earlier_attempt)
        .collect();
    let NotionSnapshot::Complete(rows) = snapshot else {
        return keys
            .into_iter()
            .map(|key| PlannedAssignment {
                key: key.clone(),
                action: PlanAction::Deferred(DeferredReason::IncompleteSnapshot),
                course_relation_mismatch: false,
            })
            .collect();
    };

    let mut pages_by_key = HashMap::<_, Vec<_>>::new();
    let mut page_ids = HashSet::new();
    let mut repeated_page_ids = HashSet::new();
    for row in rows {
        if !page_ids.insert(row.page_id.as_str()) {
            repeated_page_ids.insert(row.page_id.as_str());
        }
        if let Some(key) = &row.sync_key {
            keys.insert(key);
            pages_by_key.entry(key).or_default().push(row);
        }
    }

    let mut pages_by_course = HashMap::<_, BTreeSet<_>>::new();
    for mapping in course_mappings {
        pages_by_course
            .entry(&mapping.course_key)
            .or_default()
            .insert(uuid(&mapping.page_id));
    }

    keys.into_iter()
        .map(|key| {
            let (action, course_relation_mismatch) = plan_assignment(
                input.assignments.get(key),
                input.earlier_attempt.contains(key),
                pages_by_key.get(key).map_or(&[], Vec::as_slice),
                &repeated_page_ids,
                &pages_by_course,
            );
            PlannedAssignment {
                key: key.clone(),
                action,
                course_relation_mismatch,
            }
        })
        .collect()
}

fn plan_assignment(
    assignment: Option<&SyncAssignment>,
    earlier_attempt: bool,
    matches: &[&NotionAssignment],
    repeated_page_ids: &HashSet<&str>,
    pages_by_course: &HashMap<&ExternalRecordKey, BTreeSet<Option<String>>>,
) -> (PlanAction, bool) {
    let page = match matches {
        [] => None,
        [page]
            if !page.page_id.trim().is_empty()
                && !repeated_page_ids.contains(page.page_id.as_str()) =>
        {
            Some(*page)
        }
        [_] => {
            return (
                PlanAction::Conflict(ConflictReason::InvalidOrRepeatedPage),
                false,
            );
        }
        _ => {
            return (
                PlanAction::Conflict(ConflictReason::DuplicateSyncKey),
                false,
            );
        }
    };
    let Some(assignment) = assignment else {
        let reason = if earlier_attempt {
            DeferredReason::EarlierAttempt
        } else {
            DeferredReason::NoValidatedCandidate
        };
        return (PlanAction::Deferred(reason), false);
    };

    let course_page_id = match assignment
        .course_key
        .as_ref()
        .and_then(|key| pages_by_course.get(key))
    {
        None => None,
        Some(pages) => {
            let Some(Some(id)) = pages.first().filter(|_| pages.len() == 1) else {
                return (
                    PlanAction::Conflict(ConflictReason::AmbiguousCourseMapping),
                    false,
                );
            };
            Some(id)
        }
    };
    let Some(page) = page else {
        return (
            PlanAction::Create {
                assignment: assignment.clone(),
                course_page_id: course_page_id.cloned(),
            },
            false,
        );
    };

    let (course_page_id, course_relation_mismatch) =
        match (course_page_id, page.course_page_ids.as_slice()) {
            (id, []) => (id.cloned(), false),
            (Some(id), [current]) if uuid(current).as_ref() == Some(id) => (None, false),
            (id, _) => (None, id.is_some()),
        };
    let patch = AssignmentPatch {
        source: (page.source != Some(assignment.source)).then_some(assignment.source),
        title: (page.title != assignment.title).then(|| assignment.title.clone()),
        deadline: (page.deadline != assignment.deadline).then(|| assignment.deadline.clone()),
        course_page_id,
    };
    let action = if patch == AssignmentPatch::default() {
        PlanAction::Unchanged {
            page_id: page.page_id.clone(),
        }
    } else {
        PlanAction::Update {
            page_id: page.page_id.clone(),
            patch,
        }
    };
    (action, course_relation_mismatch)
}
