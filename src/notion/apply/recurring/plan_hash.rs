use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    str::FromStr,
};

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use serde_json::Value;

use super::{PreparedApply, RecurringReport, digest, source_label};
use crate::{
    domain::Deadline,
    notion::{
        ConflictReason, DeferredReason, PlanAction, SyncInput, SyncKey, UniversitySource,
        state::{BindingStatus, StateSummary},
    },
};

const PREFIX: &str = "v1-";

/// The fingerprint printed by a recurring sync preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanHash([u8; 64]);

impl FromStr for PlanHash {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value.strip_prefix(PREFIX).ok_or("invalid_plan_hash")?;
        let digits: [u8; 64] = hex.as_bytes().try_into().map_err(|_| "invalid_plan_hash")?;
        if digits
            .iter()
            .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(digits))
        } else {
            Err("invalid_plan_hash")
        }
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        f.write_str(str::from_utf8(&self.0).expect("validated lowercase hex"))
    }
}

impl Serialize for PlanHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Serialize)]
pub(super) struct PendingBinding {
    status: &'static str,
    page_id: Option<String>,
}

pub(super) fn pending_bindings(state: &StateSummary) -> BTreeMap<SyncKey, PendingBinding> {
    state
        .bindings
        .iter()
        .filter_map(|(key, binding)| {
            let status = match binding.status {
                BindingStatus::CreateIntent => "intent",
                BindingStatus::Unknown => "unknown",
                BindingStatus::Reconcile => "reconcile",
                BindingStatus::Linked => return None,
            };
            Some((
                key.clone(),
                PendingBinding {
                    status,
                    page_id: binding.page_id.clone(),
                },
            ))
        })
        .collect()
}

#[derive(Serialize)]
struct Entry<'a> {
    source: &'static str,
    key: &'a str,
    action: Action<'a>,
    course_relation_mismatch: bool,
    request: Option<Request<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action<'a> {
    Create,
    Update {
        page_id: &'a str,
        before: Baseline<'a>,
    },
    Unchanged {
        page_id: &'a str,
    },
    Conflict {
        reason: ConflictReason,
    },
    Deferred {
        reason: DeferredReason,
    },
    Excluded,
}

#[derive(Serialize)]
struct Baseline<'a> {
    source: Option<&'static str>,
    title: &'a str,
    deadline: &'a Option<Deadline>,
    course_page_ids: Vec<&'a str>,
    edited_at: &'a Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct Request<'a> {
    method: &'a str,
    path: &'a str,
    body: Value,
}

/// Version 1 is a domain-separated JSON encoding of typed plan data. Source
/// and page identities, sorted entries and recovery records, and recursively
/// ordered request object keys make the encoding independent of listing order.
pub(super) fn compute(
    target_and_schema: (&str, &str),
    fresh: &PreparedApply,
    report: &RecurringReport,
    inputs: &[(UniversitySource, SyncInput)],
    candidates: &[(UniversitySource, &SyncInput)],
    pending: BTreeMap<SyncKey, PendingBinding>,
    origins: &HashMap<SyncKey, UniversitySource>,
) -> Result<PlanHash, &'static str> {
    let sources = [
        (UniversitySource::Moodle, &report.moodle),
        (UniversitySource::WebClass, &report.webclass),
    ]
    .map(|(source, status)| {
        (
            source_label(source),
            status.acquisition,
            status.error,
            candidates.iter().any(|(candidate, _)| *candidate == source),
        )
    });
    let pages: HashMap<_, _> = fresh
        .pages
        .iter()
        .map(|page| (page.row.page_id.as_str(), page))
        .collect();
    let requests: HashMap<_, _> = fresh
        .deltas
        .requests()
        .iter()
        .map(|request| (request.key(), request))
        .collect();
    let mut entries = Vec::new();
    for entry in fresh.deltas.plan() {
        let source = inputs
            .iter()
            .find(|(_, input)| input.assignments().contains_key(&entry.key))
            .map(|(source, _)| source_label(*source))
            .ok_or("plan_hash_unavailable")?;
        let action = match &entry.action {
            PlanAction::Create { .. } => Action::Create,
            PlanAction::Update { page_id, .. } => {
                let page = pages[page_id.as_str()];
                let mut courses: Vec<_> = page
                    .row
                    .course_page_ids
                    .iter()
                    .map(String::as_str)
                    .collect();
                courses.sort_unstable();
                Action::Update {
                    page_id,
                    before: Baseline {
                        source: page.row.source.map(source_label),
                        title: &page.row.title,
                        deadline: &page.row.deadline,
                        course_page_ids: courses,
                        edited_at: &page.edited_at,
                    },
                }
            }
            PlanAction::Unchanged { page_id } => Action::Unchanged { page_id },
            PlanAction::Conflict(reason) => Action::Conflict { reason: *reason },
            PlanAction::Deferred(reason) => Action::Deferred { reason: *reason },
            PlanAction::Excluded => Action::Excluded,
        };
        entries.push(Entry {
            source,
            key: entry.key.as_str(),
            action,
            course_relation_mismatch: entry.course_relation_mismatch,
            request: requests.get(&entry.key).map(|request| Request {
                method: request.method().as_str(),
                path: request.path(),
                body: canonical_json(request.body()),
            }),
        });
    }
    entries.sort_unstable_by_key(|entry| (entry.source, entry.key));
    let pending: Vec<_> = pending
        .iter()
        .map(|(key, binding)| {
            (
                key.as_str(),
                binding,
                origins.get(key).copied().map(source_label),
            )
        })
        .collect();
    let recovery = |rows: &[(SyncKey, String)]| {
        let mut ordered = rows.to_vec();
        ordered.sort_unstable();
        ordered
            .into_iter()
            .map(|(key, page)| (key.as_str().to_owned(), page))
            .collect::<Vec<_>>()
    };
    let encoded = (
        "assignment-manager/recurring-plan/v1",
        target_and_schema,
        sources,
        entries,
        pending,
        recovery(&fresh.recovered),
        recovery(&fresh.manual_candidates),
    );
    digest(&encoded)?.parse()
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let sorted: BTreeMap<_, _> = fields
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        value => value.clone(),
    }
}
