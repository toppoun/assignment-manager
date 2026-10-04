//! Offline Moodle calendar import. A VEVENT is kept as a calendar event, not a logical assignment.

use std::error::Error;
use std::fmt;

use chrono::{DateTime, NaiveDateTime, Utc};
use ical::property::Property;
use ical::{IcalParser, LineReader};

use crate::domain::{Deadline, SubmissionTarget};
use crate::import::{AssignmentCandidate, ExternalRecordKey, OriginKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoodleEvent {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    /// Category text is retained for inspection; it is not linked to a CourseId.
    pub categories: Vec<String>,
    pub dtstart: DateTime<Utc>,
    pub dtend: Option<DateTime<Utc>>,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleEventKind {
    SubmissionDeadline,
    AvailabilityStart,
    AvailabilityEnd,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MoodleParseError {
    InvalidIcs(String),
    MissingUid {
        event_index: usize,
    },
    MissingSummary {
        event_index: usize,
    },
    MissingDtstart {
        event_index: usize,
    },
    InvalidDateTime {
        event_index: usize,
        property: &'static str,
        value: String,
    },
    UnsupportedDateTime {
        event_index: usize,
        property: &'static str,
        value: String,
    },
}

impl fmt::Display for MoodleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIcs(reason) => write!(f, "invalid ICS: {reason}"),
            Self::MissingUid { event_index } => write!(f, "VEVENT {event_index}: missing UID"),
            Self::MissingSummary { event_index } => {
                write!(f, "VEVENT {event_index}: missing SUMMARY")
            }
            Self::MissingDtstart { event_index } => {
                write!(f, "VEVENT {event_index}: missing DTSTART")
            }
            Self::InvalidDateTime {
                event_index,
                property,
                value,
            } => write!(
                f,
                "VEVENT {event_index}: invalid {property} datetime: {value}"
            ),
            Self::UnsupportedDateTime {
                event_index,
                property,
                value,
            } => write!(
                f,
                "VEVENT {event_index}: unsupported {property} datetime: {value}"
            ),
        }
    }
}

impl Error for MoodleParseError {}

/// Parse one calendar. Any malformed VEVENT fails the whole import.
pub fn parse_calendar(ics: &str) -> Result<Vec<MoodleEvent>, MoodleParseError> {
    validate_component_boundaries(ics)?;
    let calendars = IcalParser::new(ics.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| MoodleParseError::InvalidIcs(error.to_string()))?;
    let [calendar] = calendars.as_slice() else {
        return Err(MoodleParseError::InvalidIcs(
            "expected exactly one VCALENDAR".into(),
        ));
    };

    calendar
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| parse_event(&event.properties, index + 1))
        .collect()
}

// The crate parses properties and unfolding, but does not check matching END names.
fn validate_component_boundaries(ics: &str) -> Result<(), MoodleParseError> {
    let mut stack: Vec<String> = Vec::new();
    let mut calendar_seen = false;
    for line in LineReader::new(ics.as_bytes()) {
        let line = line.as_str();
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("BEGIN") {
                if stack.is_empty() {
                    if calendar_seen || !value.eq_ignore_ascii_case("VCALENDAR") {
                        return Err(MoodleParseError::InvalidIcs(
                            "expected one VCALENDAR root".into(),
                        ));
                    }
                    calendar_seen = true;
                }
                stack.push(value.to_owned());
                continue;
            }
            if name.eq_ignore_ascii_case("END") {
                if !stack
                    .pop()
                    .is_some_and(|open| open.eq_ignore_ascii_case(value))
                {
                    return Err(MoodleParseError::InvalidIcs(
                        "mismatched component END".into(),
                    ));
                }
                continue;
            }
        }
        if stack.is_empty() && !line.is_empty() {
            return Err(MoodleParseError::InvalidIcs(
                "content outside VCALENDAR".into(),
            ));
        }
    }
    if !calendar_seen || !stack.is_empty() {
        return Err(MoodleParseError::InvalidIcs(
            "unclosed or missing VCALENDAR".into(),
        ));
    }
    Ok(())
}

fn parse_event(
    properties: &[Property],
    event_index: usize,
) -> Result<MoodleEvent, MoodleParseError> {
    let uid = property(properties, "UID")?
        .and_then(|value| value.value.as_deref())
        .filter(|value| !value.is_empty())
        .ok_or(MoodleParseError::MissingUid { event_index })?
        .to_owned();
    let summary = property(properties, "SUMMARY")?
        .and_then(|value| value.value.as_deref())
        .filter(|value| !value.is_empty())
        .ok_or(MoodleParseError::MissingSummary { event_index })?;
    let summary = decode_text(summary)?;
    let description = property(properties, "DESCRIPTION")?
        .map(|value| decode_text(value.value.as_deref().unwrap_or("")))
        .transpose()?;
    let categories = properties
        .iter()
        .filter(|value| value.name == "CATEGORIES")
        .map(|value| decode_text(value.value.as_deref().unwrap_or("")))
        .collect::<Result<Vec<_>, _>>()?;
    let dtstart = property(properties, "DTSTART")?
        .ok_or(MoodleParseError::MissingDtstart { event_index })
        .and_then(|value| parse_datetime(value, "DTSTART", event_index))?;
    let dtend = property(properties, "DTEND")?
        .map(|value| parse_datetime(value, "DTEND", event_index))
        .transpose()?;
    let last_modified = property(properties, "LAST-MODIFIED")?
        .map(|value| parse_datetime(value, "LAST-MODIFIED", event_index))
        .transpose()?;

    Ok(MoodleEvent {
        uid,
        summary,
        description,
        categories,
        dtstart,
        dtend,
        last_modified,
    })
}

fn property<'a>(
    properties: &'a [Property],
    name: &str,
) -> Result<Option<&'a Property>, MoodleParseError> {
    let mut matches = properties.iter().filter(|value| value.name == name);
    let first = matches.next();
    if matches.next().is_some() {
        return Err(MoodleParseError::InvalidIcs(format!(
            "duplicate {name} property in VEVENT"
        )));
    }
    Ok(first)
}

// `ical` unfolds content lines but leaves TEXT property escapes untouched.
fn decode_text(value: &str) -> Result<String, MoodleParseError> {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => decoded.push('\n'),
            Some('\\') => decoded.push('\\'),
            Some(',') => decoded.push(','),
            Some(';') => decoded.push(';'),
            _ => {
                return Err(MoodleParseError::InvalidIcs(
                    "invalid iCalendar TEXT escape".into(),
                ));
            }
        }
    }
    Ok(decoded)
}

fn parse_datetime(
    property: &Property,
    name: &'static str,
    event_index: usize,
) -> Result<DateTime<Utc>, MoodleParseError> {
    let value = property.value.as_deref().unwrap_or("");
    let bytes = value.as_bytes();
    if property
        .params
        .as_ref()
        .is_some_and(|params| !params.is_empty())
        || bytes.len() != 16
        || bytes.get(8) != Some(&b'T')
        || bytes.get(15) != Some(&b'Z')
        || bytes[..8]
            .iter()
            .chain(&bytes[9..15])
            .any(|b| !b.is_ascii_digit())
    {
        return Err(MoodleParseError::UnsupportedDateTime {
            event_index,
            property: name,
            value: value.into(),
        });
    }
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map(|datetime| datetime.and_utc())
        .map_err(|_| MoodleParseError::InvalidDateTime {
            event_index,
            property: name,
            value: value.into(),
        })
}

pub fn classify_event(event: &MoodleEvent) -> MoodleEventKind {
    if normalized_title(&event.summary, "の提出期限").is_some() {
        MoodleEventKind::SubmissionDeadline
    } else if normalized_title(&event.summary, "の受験可能期間の開始").is_some() {
        MoodleEventKind::AvailabilityStart
    } else if normalized_title(&event.summary, "の受験可能期間の終了").is_some() {
        MoodleEventKind::AvailabilityEnd
    } else {
        MoodleEventKind::Unknown
    }
}

fn normalized_title(summary: &str, suffix: &str) -> Option<String> {
    let title = summary.strip_suffix(suffix)?;
    let title = title.strip_suffix(' ').unwrap_or(title);
    let title = title
        .strip_prefix('「')
        .and_then(|inner| inner.strip_suffix('」'))
        .filter(|inner| !inner.is_empty())
        .unwrap_or(title);
    (!title.is_empty()).then(|| title.to_owned())
}

/// `scope` is a stable caller-provided feed label, never a calendar URL or token.
pub fn map_event(event: &MoodleEvent, scope: &str) -> Option<AssignmentCandidate> {
    let suffix = match classify_event(event) {
        MoodleEventKind::SubmissionDeadline => "の提出期限",
        MoodleEventKind::AvailabilityEnd => "の受験可能期間の終了",
        MoodleEventKind::AvailabilityStart | MoodleEventKind::Unknown => return None,
    };
    Some(AssignmentCandidate {
        external_key: ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: scope.to_owned(),
            id: event.uid.clone(),
        },
        title: normalized_title(&event.summary, suffix)?,
        deadline: Some(Deadline::DateTime(event.dtstart)),
        submission: SubmissionTarget::Unknown,
        submission_url: None,
    })
}

/// Map deadline-like events only; the result is not a source-presence snapshot.
///
/// Unmapped events still exist in `events`. Candidate absence must not trigger
/// deletion. Wrap the returned candidates in
/// [`crate::import::AssignmentCandidateBatch`] before persistence to validate
/// identity and duplicates independently of the source parser.
pub fn map_events(events: &[MoodleEvent], scope: &str) -> Vec<AssignmentCandidate> {
    events
        .iter()
        .filter_map(|event| map_event(event, scope))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "cis-moodle-main";
    const BASIC: &str = include_str!("../fixtures/moodle/calendar_basic.ics");
    const MULTIPLE: &str = include_str!("../fixtures/moodle/calendar_multiple.ics");
    const EMPTY: &str = include_str!("../fixtures/moodle/calendar_empty.ics");
    const EDGE_CASES: &str = include_str!("../fixtures/moodle/calendar_edge_cases.ics");

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn basic_deadline_maps_to_one_candidate() {
        let events = parse_calendar(BASIC).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.uid, "1001@cms.example.edu");
        assert_eq!(event.summary, "「演習課題01」の提出期限");
        assert_eq!(event.description.as_deref(), Some(""));
        assert_eq!(event.categories, ["Java2-2026"]);
        assert_eq!(event.dtstart, utc("2026-09-27T14:59:00Z"));
        assert_eq!(event.dtend, Some(event.dtstart));
        assert_eq!(event.last_modified, Some(utc("2026-09-20T09:12:59Z")));
        assert_eq!(classify_event(event), MoodleEventKind::SubmissionDeadline);

        let candidates = map_events(&events, SCOPE);
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.external_key.origin, OriginKind::Moodle);
        assert_eq!(candidate.external_key.scope, SCOPE);
        assert_eq!(candidate.external_key.id, event.uid);
        assert_eq!(candidate.title, "演習課題01");
        assert_eq!(candidate.deadline, Some(Deadline::DateTime(event.dtstart)));
        assert_eq!(candidate.submission, SubmissionTarget::Unknown);
        assert_eq!(candidate.submission_url, None);
    }

    #[test]
    fn multiple_events_keep_event_identity_and_skip_availability_start() {
        let events = parse_calendar(MULTIPLE).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].uid, "2001@cms.example.edu");
        assert_eq!(events[1].uid, "2002@cms.example.edu");
        assert_eq!(events[2].uid, "2003@cms.example.edu");
        assert_eq!(
            classify_event(&events[0]),
            MoodleEventKind::AvailabilityStart
        );
        assert_eq!(classify_event(&events[1]), MoodleEventKind::AvailabilityEnd);
        assert_eq!(
            classify_event(&events[2]),
            MoodleEventKind::SubmissionDeadline
        );
        assert_eq!(map_event(&events[0], SCOPE), None);

        let candidates = map_events(&events, SCOPE);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].external_key.id, events[1].uid);
        assert_eq!(candidates[0].title, "課題（DFS/BFS）");
        assert_eq!(
            candidates[0].deadline,
            Some(Deadline::DateTime(utc("2026-10-08T01:50:00Z")))
        );
        assert_eq!(candidates[1].external_key.id, events[2].uid);
        assert_eq!(candidates[1].title, "CF-L01課題");
        assert_ne!(events[0].uid, events[1].uid);
    }

    #[test]
    fn empty_calendar_has_no_events_or_candidates() {
        let events = parse_calendar(EMPTY).unwrap();
        assert!(events.is_empty());
        assert!(map_events(&events, SCOPE).is_empty());
    }

    #[test]
    fn unmapped_event_is_still_observed_in_source_not_disappeared() {
        let events = parse_calendar(MULTIPLE).unwrap();
        let batch =
            crate::import::AssignmentCandidateBatch::try_new(map_events(&events, SCOPE)).unwrap();
        let observed = &events[0];
        assert_eq!(classify_event(observed), MoodleEventKind::AvailabilityStart);
        assert_eq!(events.len(), 3);
        assert_eq!(batch.as_slice().len(), 2);
        assert!(
            batch
                .as_slice()
                .iter()
                .all(|candidate| candidate.external_key.id != observed.uid)
        );
        // Presence tracking must use the source event UID even though it is not a candidate.
    }

    #[test]
    fn mapped_events_pass_through_shared_identity_and_duplicate_validation() {
        use crate::import::{AssignmentCandidateBatch, ImportValidationError};

        let events = parse_calendar(BASIC).unwrap();
        assert_eq!(
            AssignmentCandidateBatch::try_new(map_events(&events, "\t")),
            Err(ImportValidationError::EmptyExternalScope)
        );
        let mut blank_uid = events.clone();
        blank_uid[0].uid = "\n".into();
        assert_eq!(
            AssignmentCandidateBatch::try_new(map_events(&blank_uid, SCOPE)),
            Err(ImportValidationError::EmptyExternalId)
        );
        let duplicate = vec![events[0].clone(), events[0].clone()];
        assert_eq!(
            AssignmentCandidateBatch::try_new(map_events(&duplicate, SCOPE)),
            Err(ImportValidationError::DuplicateExternalIdentity)
        );
    }

    #[test]
    fn edge_cases_preserve_text_without_inferring_submission_target() {
        let events = parse_calendar(EDGE_CASES).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].description.as_deref(), Some(""));
        assert_eq!(events[0].categories, ["2026-DB"]);
        assert_eq!(classify_event(&events[0]), MoodleEventKind::AvailabilityEnd);
        assert_eq!(
            events[1].summary,
            "折り返し行を含む課題 の受験可能期間の終了"
        );
        assert_eq!(
            events[1].description.as_deref(),
            Some(
                "第2回 リレーショナルモデル課題\n解答時間: 15分以内\n受験可能回数: 1回\n締切:10/9 13:00\nこの行はiCalendarのfolded lineを確認するために意図的に折り返されています。"
            )
        );
        assert_eq!(
            classify_event(&events[2]),
            MoodleEventKind::SubmissionDeadline
        );
        assert!(
            events[2]
                .description
                .as_deref()
                .unwrap()
                .contains("Moodleへの提出は不要。")
        );
        assert_eq!(classify_event(&events[3]), MoodleEventKind::Unknown);
        assert_eq!(map_event(&events[3], SCOPE), None);

        let candidates = map_events(&events, SCOPE);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[2].title, "外部フォーム回答課題");
        assert_eq!(candidates[2].submission, SubmissionTarget::Unknown);
        assert_eq!(candidates[2].submission_url, None);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.external_key.id != events[3].uid)
        );
    }

    #[test]
    fn invalid_and_unsupported_datetimes_are_rejected() {
        let invalid = BASIC.replace("20260927T145900Z", "20261327T145900Z");
        assert!(matches!(
            parse_calendar(&invalid),
            Err(MoodleParseError::InvalidDateTime {
                property: "DTSTART",
                ..
            })
        ));

        let date_only = BASIC.replace("DTSTART:20260927T145900Z", "DTSTART;VALUE=DATE:20260927");
        assert!(matches!(
            parse_calendar(&date_only),
            Err(MoodleParseError::UnsupportedDateTime {
                property: "DTSTART",
                ..
            })
        ));

        let floating = BASIC.replace("DTSTART:20260927T145900Z", "DTSTART:20260927T145900");
        assert!(matches!(
            parse_calendar(&floating),
            Err(MoodleParseError::UnsupportedDateTime {
                property: "DTSTART",
                ..
            })
        ));

        let timezone = BASIC.replace(
            "DTSTART:20260927T145900Z",
            "DTSTART;TZID=Asia/Tokyo:20260927T235900",
        );
        assert!(matches!(
            parse_calendar(&timezone),
            Err(MoodleParseError::UnsupportedDateTime {
                property: "DTSTART",
                ..
            })
        ));
    }

    #[test]
    fn malformed_calendar_or_event_fails_whole_parse() {
        assert!(matches!(
            parse_calendar(""),
            Err(MoodleParseError::InvalidIcs(_))
        ));
        let broken_ics = BASIC.replace("END:VEVENT", "END:VTODO");
        assert!(matches!(
            parse_calendar(&broken_ics),
            Err(MoodleParseError::InvalidIcs(_))
        ));
        let missing_uid = MULTIPLE.replace("UID:2002@cms.example.edu\n", "");
        assert_eq!(
            parse_calendar(&missing_uid),
            Err(MoodleParseError::MissingUid { event_index: 2 })
        );
        let missing_summary = BASIC.replace("SUMMARY:「演習課題01」の提出期限\n", "");
        assert_eq!(
            parse_calendar(&missing_summary),
            Err(MoodleParseError::MissingSummary { event_index: 1 })
        );
        let missing_dtstart = BASIC.replace("DTSTART:20260927T145900Z\n", "");
        assert_eq!(
            parse_calendar(&missing_dtstart),
            Err(MoodleParseError::MissingDtstart { event_index: 1 })
        );
    }
}
