use std::collections::{HashMap, HashSet};

use crate::{
    domain::Deadline,
    import::{ExternalRecordKey, OriginKind},
    notion::{
        CourseMapping, DryRunReport, NotionClient, NotionError, PlanAction, SyncInput,
        UniversitySource, VerificationTarget,
        state::{BindingStatus, StateSummary},
        trial::MOODLE_TRIAL_COURSES,
        wire::{Schema, page_property, text, uuid},
    },
};

pub const MOODLE_INTEGRATION_SCOPE: &str = "default.course-related";

/// Aggregate checks for the bindings persisted before the dry run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MoodleBindingAudit {
    pub linked: usize,
    pub moodle_present: usize,
    pub notion_exact: usize,
    pub notion_missing: usize,
    pub notion_duplicate: usize,
    pub page_mismatch: usize,
    pub create: usize,
    pub update: usize,
    pub unchanged: usize,
    pub title_changes: usize,
    pub deadline_changes: usize,
}

/// Explicit, one-to-one real course mappings. Trial-only pages cannot be reused here.
pub struct MoodlePreviewMappings<'a> {
    entries: &'a [CourseMapping],
    by_course: HashMap<&'a ExternalRecordKey, String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MoodlePreviewCoverage {
    pub candidates: usize,
    pub courses: usize,
    pub unmapped_courses: usize,
    pub unused_mappings: usize,
    pub unmapped_candidates: usize,
    pub unclassified_candidates: usize,
    pub timed_deadlines: usize,
    pub date_only_deadlines: usize,
    pub missing_deadlines: usize,
}

impl MoodlePreviewCoverage {
    pub fn ready(&self) -> bool {
        self.candidates != 0
            && self.unmapped_candidates == 0
            && self.unclassified_candidates == 0
            && self.missing_deadlines == 0
    }
}

impl<'a> MoodlePreviewMappings<'a> {
    pub fn new(entries: &'a [CourseMapping]) -> Result<Self, NotionError> {
        let mut by_course = HashMap::new();
        let mut used = HashSet::new();
        for mapping in entries {
            let key = &mapping.course_key;
            let page = uuid(&mapping.page_id).ok_or(NotionError::Configuration)?;
            if key.origin != OriginKind::Moodle
                || key.scope != MOODLE_INTEGRATION_SCOPE
                || key.id.trim().is_empty()
                || MOODLE_TRIAL_COURSES
                    .iter()
                    .any(|(category, id)| key.id == *category || page == *id)
                || !used.insert(page.clone())
                || by_course.insert(key, page).is_some()
            {
                return Err(NotionError::Configuration);
            }
        }
        Ok(Self { entries, by_course })
    }

    pub fn coverage(&self, input: &SyncInput) -> Result<MoodlePreviewCoverage, NotionError> {
        let mut coverage = MoodlePreviewCoverage {
            candidates: input.assignments().len(),
            ..Default::default()
        };
        let mut courses = HashSet::new();
        let mut unmapped = HashSet::new();
        for entry in input.assignments().values() {
            if entry.source != UniversitySource::Moodle {
                return Err(NotionError::Configuration);
            }
            match entry.deadline {
                Some(Deadline::DateTime(_)) => coverage.timed_deadlines += 1,
                Some(Deadline::DateOnly(_)) => coverage.date_only_deadlines += 1,
                None => coverage.missing_deadlines += 1,
            }
            if let Some(course) = &entry.course_key {
                courses.insert(course);
                if !self.by_course.contains_key(course) {
                    unmapped.insert(course);
                    coverage.unmapped_candidates += 1;
                }
            } else {
                coverage.unclassified_candidates += 1;
            }
        }
        coverage.courses = courses.len();
        coverage.unmapped_courses = unmapped.len();
        coverage.unused_mappings =
            self.by_course.len() - (coverage.courses - coverage.unmapped_courses);
        Ok(coverage)
    }
}

impl NotionClient {
    /// Read a complete, scoped course snapshot before trusting any mapped page ID.
    pub fn preview_moodle_integration(
        &self,
        target: &VerificationTarget,
        input: &SyncInput,
        mappings: &MoodlePreviewMappings<'_>,
        state: StateSummary,
    ) -> Result<DryRunReport, NotionError> {
        self.preview_moodle_integration_with_audit(target, input, mappings, state)
            .map(|(report, _)| report)
    }

    pub fn preview_moodle_integration_with_audit(
        &self,
        target: &VerificationTarget,
        input: &SyncInput,
        mappings: &MoodlePreviewMappings<'_>,
        state: StateSummary,
    ) -> Result<(DryRunReport, MoodleBindingAudit), NotionError> {
        if !mappings.coverage(input)?.ready() {
            return Err(NotionError::Configuration);
        }
        target.verify_reader(self)?;
        let schema = target.retrieve_schema(self)?;
        let linked: Vec<_> = state
            .bindings
            .iter()
            .filter(|(_, binding)| binding.status == BindingStatus::Linked)
            .map(|(key, binding)| (key.clone(), binding.page_id.clone()))
            .collect();
        let prepared = self.prepare_moodle_plan(schema, input, mappings, state, false)?;
        let mut audit = MoodleBindingAudit {
            linked: linked.len(),
            ..Default::default()
        };
        let mut rows_by_key = HashMap::<_, Vec<_>>::new();
        for observation in &prepared.pages {
            if let Some(key) = &observation.row.sync_key {
                rows_by_key.entry(key).or_default().push(&observation.row);
            }
        }
        let actions: HashMap<_, _> = prepared
            .deltas
            .plan()
            .iter()
            .map(|entry| (&entry.key, &entry.action))
            .collect();
        for (key, page) in &linked {
            let candidate = input.assignments().get(key);
            audit.moodle_present += usize::from(candidate.is_some());
            match actions.get(key) {
                Some(PlanAction::Create { .. }) => audit.create += 1,
                Some(PlanAction::Update { .. }) => audit.update += 1,
                Some(PlanAction::Unchanged { .. }) => audit.unchanged += 1,
                _ => (),
            }
            match rows_by_key.get(key).map_or(&[][..], Vec::as_slice) {
                [] => audit.notion_missing += 1,
                [row] if page.as_ref() == Some(&row.page_id) => {
                    audit.notion_exact += 1;
                    if let Some(candidate) = candidate {
                        audit.title_changes += usize::from(candidate.title != row.title);
                        audit.deadline_changes += usize::from(candidate.deadline != row.deadline);
                    }
                }
                [_] => audit.page_mismatch += 1,
                _ => audit.notion_duplicate += 1,
            }
        }
        Ok((prepared.deltas, audit))
    }

    pub(super) fn prepare_moodle_plan(
        &self,
        schema: Schema,
        input: &SyncInput,
        mappings: &MoodlePreviewMappings<'_>,
        state: StateSummary,
        verify_titles: bool,
    ) -> Result<super::PreparedApply, NotionError> {
        let expected_titles: HashMap<_, _> = mappings
            .by_course
            .iter()
            .map(|(key, page)| (page.as_str(), key.id.as_str()))
            .collect();
        let course_pages = self.query_course_pages(&schema, |id, page| {
            if verify_titles
                && let Some(expected) = expected_titles.get(id)
                && text(page_property(page, "title", "title")?)? != *expected
            {
                return Err(NotionError::CourseMappingOutsideTarget);
            }
            Ok(())
        })?;
        let verify_course = |id: &str| {
            if course_pages.contains_key(id) {
                Ok(())
            } else {
                Err(NotionError::CourseMappingOutsideTarget)
            }
        };
        for id in mappings.by_course.values() {
            verify_course(id)?;
        }
        self.prepare_plan(schema, state, |schema, rows| {
            self.plan_from_rows_with_course_check(
                input,
                schema,
                rows,
                mappings.entries,
                verify_course,
            )
        })
    }
}
