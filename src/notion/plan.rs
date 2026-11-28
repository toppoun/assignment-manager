use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
};

use crate::{domain::Deadline, import::ExternalRecordKey};
use serde::{Deserialize, Serialize};

use super::course_rules::CourseRules;
use super::wire::{notion_deadline_matches, uuid, valid_title};
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
    /// Complete relation on a managed page. Manual rows may include course IDs
    /// as context for a potential duplicate.
    pub course_page_ids: Vec<String>,
}

pub enum NotionSnapshot<'a> {
    /// All managed rows in the query scope and all comparison fields were read.
    Complete(&'a [NotionAssignment]),
    /// Read error, incomplete pagination, or an unverified projection.
    Incomplete,
}

/// An explicit binding for an exact course key. Names are never matched.
#[derive(Deserialize, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    DuplicateSyncKey,
    InvalidOrRepeatedPage,
    AmbiguousCourseMapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredReason {
    IncompleteSnapshot,
    PendingReconciliation,
    ManualCandidate,
    CrossSourceCandidate,
    /// Only an earlier WebClass attempt produced this candidate.
    EarlierAttempt,
    /// The run has no current candidate. Includes failed, skipped, filtered and absent records.
    NoValidatedCandidate,
    CourseUnconfigured,
    CourseRuleDuplicate,
    CourseRuleInvalid,
    CourseRuleInvalidRelation,
    CourseTermUnverified,
    CourseRelationMismatch,
    CourseExcluded,
    InvalidDisplayTitle,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PlanAction {
    Create {
        assignment: SyncAssignment,
        display_title: String,
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
    Excluded,
}

impl fmt::Debug for PlanAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create {
                assignment,
                course_page_id,
                ..
            } => f
                .debug_struct("Create")
                .field("assignment", assignment)
                .field("course_page_id", course_page_id)
                .finish(),
            Self::Update { page_id, patch } => f
                .debug_struct("Update")
                .field("page_id", page_id)
                .field("patch", patch)
                .finish(),
            Self::Unchanged { page_id } => f
                .debug_struct("Unchanged")
                .field("page_id", page_id)
                .finish(),
            Self::Conflict(reason) => f.debug_tuple("Conflict").field(reason).finish(),
            Self::Deferred(reason) => f.debug_tuple("Deferred").field(reason).finish(),
            Self::Excluded => f.write_str("Excluded"),
        }
    }
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
/// This entry point uses the legacy page-ID mappings for the trial commands.
pub fn plan_sync(
    input: &SyncInput,
    snapshot: NotionSnapshot<'_>,
    course_mappings: &[CourseMapping],
) -> Vec<PlannedAssignment> {
    let mut pages_by_course = HashMap::<_, BTreeSet<_>>::new();
    for mapping in course_mappings {
        pages_by_course
            .entry(&mapping.course_key)
            .or_default()
            .insert(uuid(&mapping.page_id));
    }
    plan_assignments(input, snapshot, false, |assignment| {
        match assignment
            .course_key
            .as_ref()
            .and_then(|key| pages_by_course.get(key))
        {
            None => CourseResolution::Unmapped,
            Some(pages) => match pages.first().filter(|_| pages.len() == 1) {
                Some(Some(id)) => CourseResolution::LegacyMapped(id),
                _ => CourseResolution::AmbiguousLegacyMapping,
            },
        }
    })
}

pub(super) fn plan_sync_with_rules(
    input: &SyncInput,
    snapshot: NotionSnapshot<'_>,
    rules: &CourseRules,
) -> Vec<PlannedAssignment> {
    plan_assignments(input, snapshot, true, |assignment| {
        match rules.resolve(assignment) {
            Ok(course) => CourseResolution::Confirmed(course),
            Err(reason) => CourseResolution::Deferred(reason),
        }
    })
}

enum CourseResolution<'a> {
    Unmapped,
    LegacyMapped(&'a str),
    Confirmed(&'a super::course_rules::ConfirmedCourse),
    AmbiguousLegacyMapping,
    Deferred(DeferredReason),
}

pub(super) fn display_title(course_title: &str, original_title: &str) -> String {
    let prefix = course_prefix(course_title);
    if original_title.starts_with(&prefix) {
        original_title.to_owned()
    } else {
        prefix + original_title
    }
}

pub(super) fn course_prefix(course_title: &str) -> String {
    format!("【{course_title}】")
}

fn plan_assignments<'a>(
    input: &SyncInput,
    snapshot: NotionSnapshot<'_>,
    use_course_rules: bool,
    resolve_course: impl Fn(&SyncAssignment) -> CourseResolution<'a>,
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

    keys.into_iter()
        .map(|key| {
            let (action, course_relation_mismatch) = plan_assignment(
                input.assignments.get(key),
                input.earlier_attempt.contains(key),
                pages_by_key.get(key).map_or(&[], Vec::as_slice),
                &repeated_page_ids,
                use_course_rules,
                &resolve_course,
            );
            PlannedAssignment {
                key: key.clone(),
                action,
                course_relation_mismatch,
            }
        })
        .collect()
}

fn plan_assignment<'a>(
    assignment: Option<&SyncAssignment>,
    earlier_attempt: bool,
    matches: &[&NotionAssignment],
    repeated_page_ids: &HashSet<&str>,
    use_course_rules: bool,
    resolve_course: impl Fn(&SyncAssignment) -> CourseResolution<'a>,
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

    let (course_page_id, title) = match resolve_course(assignment) {
        CourseResolution::Unmapped => (None, assignment.title.clone()),
        CourseResolution::LegacyMapped(id) => (Some(id), assignment.title.clone()),
        CourseResolution::Confirmed(course) => {
            let title = display_title(&course.title, &assignment.title);
            if !valid_title(&title) {
                return (
                    PlanAction::Deferred(DeferredReason::InvalidDisplayTitle),
                    false,
                );
            }
            (Some(course.page_id.as_str()), title)
        }
        CourseResolution::AmbiguousLegacyMapping => {
            return (
                PlanAction::Conflict(ConflictReason::AmbiguousCourseMapping),
                false,
            );
        }
        CourseResolution::Deferred(DeferredReason::CourseExcluded) => {
            return (PlanAction::Excluded, false);
        }
        CourseResolution::Deferred(reason) => return (PlanAction::Deferred(reason), false),
    };
    let Some(page) = page else {
        return (
            PlanAction::Create {
                assignment: assignment.clone(),
                display_title: title,
                course_page_id: course_page_id.map(str::to_owned),
            },
            false,
        );
    };

    let (course_page_id, course_relation_mismatch) =
        match (course_page_id, page.course_page_ids.as_slice()) {
            (id, []) => (id.map(str::to_owned), false),
            (Some(id), [current]) if uuid(current).as_deref() == Some(id) => (None, false),
            (id, _) => (None, id.is_some()),
        };
    if course_relation_mismatch && use_course_rules {
        return (
            PlanAction::Deferred(DeferredReason::CourseRelationMismatch),
            true,
        );
    }
    let deadline_matches = if use_course_rules {
        notion_deadline_matches(&page.deadline, &assignment.deadline)
    } else {
        page.deadline == assignment.deadline
    };
    let patch = AssignmentPatch {
        source: (page.source != Some(assignment.source)).then_some(assignment.source),
        title: (page.title != title).then_some(title),
        deadline: (!deadline_matches).then(|| assignment.deadline.clone()),
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
