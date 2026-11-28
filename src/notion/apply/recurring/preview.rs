use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::{ChangeLimits, PlanHash, PreparedApply};
use crate::{
    domain::Deadline,
    notion::{ConflictReason, DeferredReason, PlanAction, SyncInput, UniversitySource},
};

/// The opt-in report projects values from the same prepared plan as the summary.
#[derive(Default, Serialize)]
pub(super) struct PlanPreview {
    pub(super) hash: Option<PlanHash>,
    sources: BTreeMap<&'static str, SourcePlan>,
    recoverable_creates: Option<usize>,
    pub(super) unresolved_creates: Option<Vec<PendingCreate>>,
}

#[derive(Serialize)]
struct SourcePlan {
    limits: ChangeLimits,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
pub(super) struct PendingCreate {
    pub(super) sync_key: String,
    pub(super) source: Option<&'static str>,
}

#[derive(Serialize)]
struct Entry {
    sync_key: String,
    #[serde(flatten)]
    action: Action,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Create {
        title: String,
        deadline: Option<Deadline>,
        course_page_id: Option<String>,
        properties: Vec<String>,
    },
    Update {
        page_id: String,
        title: Change<String>,
        deadline: Change<Option<Deadline>>,
        properties: Vec<String>,
    },
    Unchanged {
        page_id: String,
    },
    Deferred {
        reason: DeferredReason,
    },
    Conflict {
        reason: ConflictReason,
    },
    Excluded,
}

#[derive(Serialize)]
struct Change<T> {
    before: T,
    after: T,
}

pub(super) fn source_label(source: UniversitySource) -> &'static str {
    match source {
        UniversitySource::Moodle => "moodle",
        UniversitySource::WebClass => "webclass",
    }
}

impl PlanPreview {
    pub(super) fn add_source(&mut self, source: UniversitySource, limits: ChangeLimits) {
        self.sources.insert(
            source_label(source),
            SourcePlan {
                limits,
                entries: Vec::new(),
            },
        );
    }

    pub(super) fn capture(
        &mut self,
        fresh: &PreparedApply,
        inputs: &[(UniversitySource, SyncInput)],
    ) {
        self.recoverable_creates = Some(fresh.recovered.len());
        self.unresolved_creates = Some(Vec::new());
        let by_page: HashMap<_, _> = fresh
            .pages
            .iter()
            .map(|page| (page.row.page_id.as_str(), &page.row))
            .collect();
        let properties_by_key: HashMap<_, _> = fresh
            .deltas
            .requests()
            .iter()
            .map(|request| {
                let properties = request.body()["properties"]
                    .as_object()
                    .expect("prepared properties");
                (
                    request.key(),
                    properties.keys().cloned().collect::<Vec<_>>(),
                )
            })
            .collect();
        for (source, input) in inputs {
            let plan = self
                .sources
                .get_mut(source_label(*source))
                .expect("configured source");
            for entry in fresh
                .deltas
                .plan()
                .iter()
                .filter(|entry| input.assignments().contains_key(&entry.key))
            {
                let action = match &entry.action {
                    PlanAction::Create {
                        assignment,
                        display_title,
                        course_page_id,
                    } => Action::Create {
                        title: display_title.clone(),
                        deadline: assignment.deadline.clone(),
                        course_page_id: course_page_id.clone(),
                        properties: properties_by_key[&entry.key].clone(),
                    },
                    PlanAction::Update { page_id, patch } => {
                        let before = by_page[page_id.as_str()];
                        Action::Update {
                            page_id: page_id.clone(),
                            title: Change {
                                before: before.title.clone(),
                                after: patch.title.as_ref().unwrap_or(&before.title).clone(),
                            },
                            deadline: Change {
                                before: before.deadline.clone(),
                                after: patch.deadline.as_ref().unwrap_or(&before.deadline).clone(),
                            },
                            properties: properties_by_key[&entry.key].clone(),
                        }
                    }
                    PlanAction::Unchanged { page_id } => Action::Unchanged {
                        page_id: page_id.clone(),
                    },
                    PlanAction::Deferred(reason) => Action::Deferred { reason: *reason },
                    PlanAction::Conflict(reason) => Action::Conflict { reason: *reason },
                    PlanAction::Excluded => Action::Excluded,
                };
                plan.entries.push(Entry {
                    sync_key: entry.key.as_str().to_owned(),
                    action,
                });
            }
        }
    }
}
