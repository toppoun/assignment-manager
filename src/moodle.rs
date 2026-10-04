//! Offline Moodle calendar import. A VEVENT is kept as a calendar event, not a logical assignment.

use std::error::Error;
use std::fmt;

use chrono::{DateTime, NaiveDateTime, Utc};
use ical::IcalParser;
use ical::property::Property;

use crate::domain::{Deadline, SubmissionTarget};
use crate::import::{AssignmentCandidate, ExternalRecordKey, OriginKind};
use crate::moodle_http::{MoodleExportKind, MoodleFeed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoodleEvent {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    /// Decoded TEXT values, in source order across all CATEGORIES properties.
    /// Empty values and duplicates are retained; no business normalization occurs.
    /// `Ok(vec![])` means absent metadata. `Err` means unusable category syntax,
    /// not an invalid assignment. No partial list is exposed after a decode failure.
    ///
    /// Unlike `ical` 0.11's LineReader (which trims physical lines) and property
    /// parser (which removes leading colons), this metadata comes from raw ICS.
    /// CRLF/LF delimiters and exactly one fold SPACE/HTAB are removed; all other
    /// value whitespace is preserved. The other event fields retain the existing
    /// crate's behavior. Input is UTF-8 `&str`; invalid UTF-8 is rejected upstream.
    pub categories: Result<Vec<String>, MoodleCategoryError>,
    pub dtstart: DateTime<Utc>,
    pub dtend: Option<DateTime<Utc>>,
    pub last_modified: Option<DateTime<Utc>>,
}

/// Fixed metadata classification; never retains raw ICS or category values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleCategoryError {
    InvalidText,
}

impl fmt::Display for MoodleCategoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid Moodle CATEGORIES TEXT syntax")
    }
}

impl Error for MoodleCategoryError {}

type ParsedCategories = Result<Vec<String>, MoodleCategoryError>;

/// An opaque source-level Course key candidate, without discovery or persistence.
/// Duplicate values count separately. Trimming is used only to reject blanks.
pub fn single_course_category<'a>(event: &'a MoodleEvent, feed: &MoodleFeed) -> Option<&'a str> {
    if feed.export_kind() != MoodleExportKind::CourseRelated {
        return None;
    }
    let [category] = event.categories.as_ref().ok()?.as_slice() else {
        return None;
    };
    (!category.trim().is_empty()).then_some(category.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoodleEventKind {
    SubmissionDeadline,
    AvailabilityStart,
    AvailabilityEnd,
    Unknown,
}

/// Only fixed reasons, known property names and event indices; no source values or causes.
#[derive(Debug, PartialEq, Eq)]
pub enum MoodleParseError {
    InvalidIcs(&'static str),
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
    },
    UnsupportedDateTime {
        event_index: usize,
        property: &'static str,
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
            } => write!(f, "VEVENT {event_index}: invalid {property} datetime"),
            Self::UnsupportedDateTime {
                event_index,
                property,
            } => write!(f, "VEVENT {event_index}: unsupported {property} datetime"),
        }
    }
}

impl Error for MoodleParseError {}

/// Parse one calendar. Structural/required-event errors fail the whole import;
/// unusable CATEGORIES metadata is retained separately on an otherwise valid event.
pub fn parse_calendar(ics: &str) -> Result<Vec<MoodleEvent>, MoodleParseError> {
    let categories = parse_category_metadata(ics)?;
    let calendars = IcalParser::new(ics.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MoodleParseError::InvalidIcs("invalid component or property"))?;
    let [calendar] = calendars.as_slice() else {
        return Err(MoodleParseError::InvalidIcs(
            "expected exactly one VCALENDAR",
        ));
    };

    if calendar.events.len() != categories.len() {
        return Err(MoodleParseError::InvalidIcs(
            "inconsistent VEVENT boundaries",
        ));
    }
    calendar
        .events
        .iter()
        .zip(categories)
        .enumerate()
        .map(|(index, (event, categories))| parse_event(&event.properties, index + 1, categories))
        .collect()
}

// Only unfolding, the existing matching-boundary validation, and direct VEVENT
// CATEGORIES are handled here. `ical` still parses components/properties;
// the existing datetime validation is unchanged.
// Calendar, VTODO, VTIMEZONE and nested VALARM categories must not leak into events.
fn parse_category_metadata(ics: &str) -> Result<Vec<ParsedCategories>, MoodleParseError> {
    let mut stack: Vec<String> = Vec::new();
    let mut calendar_seen = false;
    let mut events: Vec<ParsedCategories> = Vec::new();
    for line in unfold_content_lines(ics) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("BEGIN") {
                if stack.is_empty() {
                    if calendar_seen || !value.eq_ignore_ascii_case("VCALENDAR") {
                        return Err(MoodleParseError::InvalidIcs("expected one VCALENDAR root"));
                    }
                    calendar_seen = true;
                }
                if stack.len() == 1 && value.eq_ignore_ascii_case("VEVENT") {
                    events.push(Ok(Vec::new()));
                }
                stack.push(value.to_owned());
                continue;
            }
            if name.eq_ignore_ascii_case("END") {
                if !stack
                    .pop()
                    .is_some_and(|open| open.eq_ignore_ascii_case(value))
                {
                    return Err(MoodleParseError::InvalidIcs("mismatched component END"));
                }
                continue;
            }
        }
        if stack.is_empty() && !line.is_empty() {
            return Err(MoodleParseError::InvalidIcs("content outside VCALENDAR"));
        }
        if stack.len() == 2 && stack[1].eq_ignore_ascii_case("VEVENT") {
            let name = line.split([';', ':']).next().unwrap_or("");
            if name.eq_ignore_ascii_case("CATEGORIES") {
                // Locate the value delimiter outside quoted parameter values.
                // Parameters themselves are still validated by the `ical` parser.
                let mut quoted = false;
                let value = line.char_indices().find_map(|(index, character)| {
                    if character == '"' {
                        quoted = !quoted;
                    }
                    (character == ':' && !quoted).then(|| &line[index + 1..])
                });
                let decoded = value
                    .ok_or(MoodleCategoryError::InvalidText)
                    .and_then(decode_categories);
                let categories = events.last_mut().expect("direct VEVENT has metadata slot");
                match decoded {
                    Ok(values) => {
                        if let Ok(categories) = categories {
                            categories.extend(values);
                        }
                    }
                    Err(error) => *categories = Err(error),
                }
            }
        }
    }
    if !calendar_seen || !stack.is_empty() {
        return Err(MoodleParseError::InvalidIcs(
            "unclosed or missing VCALENDAR",
        ));
    }
    Ok(events)
}

// RFC 5545 section 3.1. Accept LF as well for existing offline fixtures. Only
// strip a CR that belongs to CRLF, and one ASCII SPACE/HTAB from continuations.
fn unfold_content_lines(ics: &str) -> Vec<String> {
    let mut unfolded: Vec<String> = Vec::new();
    for physical in ics.split_inclusive('\n') {
        let line = physical
            .strip_suffix("\r\n")
            .or_else(|| physical.strip_suffix('\n'))
            .unwrap_or(physical);
        if let Some(continuation) = line.strip_prefix([' ', '\t'])
            && let Some(previous) = unfolded.last_mut()
        {
            previous.push_str(continuation);
            continue;
        }
        unfolded.push(line.to_owned());
    }
    unfolded
}

// Split while escapes are still present. Decoding first would make an escaped
// comma indistinguishable from a list separator. Empty TEXT values are retained.
fn decode_categories(value: &str) -> ParsedCategories {
    let mut values = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ',' {
            values.push(decode_category_text(&value[start..index])?);
            start = index + 1;
        }
    }
    values.push(decode_category_text(&value[start..])?);
    Ok(values)
}

fn decode_category_text(value: &str) -> Result<String, MoodleCategoryError> {
    // TEXT forbids literal semicolons and control characters other than HTAB.
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if !matches!(characters.next(), Some('\\' | ',' | ';' | 'n' | 'N')) {
                return Err(MoodleCategoryError::InvalidText);
            }
        } else if character == ';' || (character.is_ascii_control() && character != '\t') {
            return Err(MoodleCategoryError::InvalidText);
        }
    }
    // Reuse the existing TEXT escape decoder, with a metadata-only safe error.
    decode_text(value).map_err(|_| MoodleCategoryError::InvalidText)
}

fn parse_event(
    properties: &[Property],
    event_index: usize,
    categories: ParsedCategories,
) -> Result<MoodleEvent, MoodleParseError> {
    let categories = if properties.iter().any(|property| {
        property.name.eq_ignore_ascii_case("CATEGORIES")
            && property.params.as_ref().is_some_and(|params| {
                params.iter().any(|(name, values)| {
                    name.eq_ignore_ascii_case("VALUE")
                        && !(values.len() == 1 && values[0].eq_ignore_ascii_case("TEXT"))
                })
            })
    }) {
        Err(MoodleCategoryError::InvalidText)
    } else {
        categories
    };
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
        return Err(MoodleParseError::InvalidIcs("duplicate property in VEVENT"));
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
                    "invalid iCalendar TEXT escape",
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
        });
    }
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map(|datetime| datetime.and_utc())
        .map_err(|_| MoodleParseError::InvalidDateTime {
            event_index,
            property: name,
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
    const COURSE_RELATED: &str = include_str!("../fixtures/moodle/calendar_course_related.ics");
    const PRIVATE_MARKER: &str = "VERY_PRIVATE_MARKER_5_5B";

    fn assert_private_marker_absent(error: &dyn Error) {
        assert!(!format!("{error}").contains(PRIVATE_MARKER));
        assert!(!format!("{error:?}").contains(PRIVATE_MARKER));
        assert!(error.source().is_none());
    }

    fn private_calendar(properties: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:{PRIVATE_MARKER}\nSUMMARY:{PRIVATE_MARKER}の提出期限\n{properties}\nEND:VEVENT\nEND:VCALENDAR\n"
        )
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn category_calendar(properties: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:category-test@example.invalid\nSUMMARY:「Synthetic task」の提出期限\nDTSTART:20261004T000000Z\n{properties}\nEND:VEVENT\nEND:VCALENDAR\n"
        )
    }

    fn course_feed() -> MoodleFeed {
        MoodleFeed::new(
            "synthetic-main.course-related",
            MoodleExportKind::CourseRelated,
            "https://example.invalid/calendar/export",
        )
        .unwrap()
    }

    #[test]
    fn category_values_preserve_list_syntax_order_duplicates_and_empty_values() {
        let baseline = map_events(&parse_calendar(&category_calendar("")).unwrap(), SCOPE);
        for (properties, expected) in [
            ("", vec![]),
            ("CATEGORIES:", vec![""]),
            ("CATEGORIES:X", vec!["X"]),
            ("CATEGORIES: \t \u{3000}", vec![" \t \u{3000}"]),
            ("CATEGORIES:X\nCATEGORIES:Y", vec!["X", "Y"]),
            ("CATEGORIES:X,Y", vec!["X", "Y"]),
            (r"CATEGORIES:X\,Y", vec!["X,Y"]),
            (r"CATEGORIES:X\,Y,Z", vec!["X,Y", "Z"]),
            ("CATEGORIES:X,X", vec!["X", "X"]),
            ("CATEGORIES:X,", vec!["X", ""]),
            ("CATEGORIES:,X", vec!["", "X"]),
            ("CATEGORIES:X\nCATEGORIES:", vec!["X", ""]),
            (r"CATEGORIES:X\\Y", vec![r"X\Y"]),
            (r"CATEGORIES:X\\,Y", vec![r"X\", "Y"]),
            (r"CATEGORIES:X\\\,Y", vec![r"X\,Y"]),
            (r"CATEGORIES:X\;Y", vec!["X;Y"]),
            (r"CATEGORIES:X\nY\NZ", vec!["X\nY\nZ"]),
            ("CATEGORIES:Z,a,Z", vec!["Z", "a", "Z"]),
            ("categories;LANGUAGE=ja:X", vec!["X"]),
            ("CATEGORIES;VALUE=TEXT:X", vec!["X"]),
            ("CATEGORIES;X-INFO=\"a:b,c\":X\\,Y", vec!["X,Y"]),
            ("CATEGORIES::X", vec![":X"]),
        ] {
            let events = parse_calendar(&category_calendar(properties)).unwrap();
            assert_eq!(events[0].categories.as_ref().unwrap(), &expected);
            // Metadata ambiguity never changes an otherwise valid assignment.
            assert_eq!(map_events(&events, SCOPE), baseline);
        }
    }

    #[test]
    fn raw_categories_unfold_without_the_dependency_whitespace_loss() {
        for (properties, expected) in [
            ("CATEGORIES:  AbC \t ", "  AbC \t "),
            (
                "CATEGORIES: first \t\n  second\t \n\t third  ",
                " first \t second\t  third  ",
            ),
            ("CATEGORIES:Java2-\n 2026", "Java2-2026"),
            ("CATE\n GORIES:X\\\n\t,Y", "X,Y"),
            ("CATEGORIES:X\\n\n Y", "X\nY"),
            ("CATEGORIES:e\u{301} Ａa", "e\u{301} Ａa"),
        ] {
            let calendar = category_calendar(properties);
            for calendar in [calendar.clone(), calendar.replace('\n', "\r\n")] {
                let events = parse_calendar(&calendar).unwrap();
                assert_eq!(events[0].categories.as_ref().unwrap(), &[expected]);
                assert_eq!(
                    single_course_category(&events[0], &course_feed()),
                    Some(expected)
                );
            }
        }
    }

    #[test]
    fn categories_stay_with_their_direct_event_and_ignore_other_components() {
        let calendar = category_calendar(
            "CATEGORIES:X\nBEGIN:VALARM\nCATEGORIES:alarm-category\nEND:VALARM",
        )
        .replace("VERSION:2.0", "VERSION:2.0\nCATEGORIES:calendar-category\nBEGIN:VTODO\nCATEGORIES:todo-category\nEND:VTODO")
        .replace("END:VCALENDAR", "BEGIN:VEVENT\nUID:second@example.invalid\nSUMMARY:Second\nDTSTART:20261004T000000Z\nCATEGORIES:Y,Z\nEND:VEVENT\nEND:VCALENDAR");
        let events = parse_calendar(&calendar).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].categories.as_ref().unwrap(), &["X"]);
        assert_eq!(events[1].categories.as_ref().unwrap(), &["Y", "Z"]);
    }

    #[test]
    fn invalid_category_text_is_metadata_failure_without_partial_course_key() {
        for properties in [
            r"CATEGORIES:X\x",
            r"CATEGORIES:X\",
            "CATEGORIES:X;Y",
            "CATEGORIES:X\u{7}Y",
            "CATEGORIES:X\rY",
            "CATEGORIES;VALUE=DATE:20261004",
            "CATEGORIES:X\nCATEGORIES:Y\\x",
            "CATEGORIES:X\\x\nCATEGORIES:Y",
        ] {
            let events = parse_calendar(&category_calendar(properties)).unwrap();
            assert_eq!(events[0].categories, Err(MoodleCategoryError::InvalidText));
            assert_eq!(single_course_category(&events[0], &course_feed()), None);
            assert_eq!(map_events(&events, SCOPE).len(), 1);
        }
        let calendar = category_calendar(&format!("CATEGORIES:{PRIVATE_MARKER}\\x"));
        let events = parse_calendar(&calendar).unwrap();
        assert_private_marker_absent(events[0].categories.as_ref().unwrap_err());
    }

    #[test]
    fn category_error_display_debug_and_source_do_not_expose_ics_or_token_url() {
        let url = format!(
            "https://example.invalid/calendar/export?authtoken={PRIVATE_MARKER}&events=course"
        );
        let calendar = category_calendar(&format!("CATEGORIES:{url}\\x"));
        let events = parse_calendar(&calendar).unwrap();
        let error = events[0].categories.as_ref().unwrap_err();
        let rendered = format!("{error} {error:?}");
        for forbidden in [
            calendar.as_str(),
            url.as_str(),
            PRIVATE_MARKER,
            "authtoken",
            "events=course",
            "BEGIN:VCALENDAR",
            "CATEGORIES:",
        ] {
            assert!(!rendered.contains(forbidden));
        }
        assert!(error.source().is_none());
        assert_eq!(map_events(&events, SCOPE).len(), 1);
    }

    #[test]
    fn course_key_requires_course_related_export_and_exactly_one_nonblank_value() {
        let course = course_feed();
        let all = MoodleFeed::new(
            "synthetic-main.all-events",
            MoodleExportKind::AllEvents,
            "https://example.invalid/calendar/export?events=course",
        )
        .unwrap();
        let unknown = MoodleFeed::new(
            "synthetic-main.unknown",
            MoodleExportKind::Unknown,
            "https://example.invalid/calendar/export",
        )
        .unwrap();
        for (properties, expected) in [
            ("CATEGORIES:X", Some("X")),
            (r"CATEGORIES:X\,Y", Some("X,Y")),
            ("CATEGORIES: X \t", Some(" X \t")),
            ("", None),
            ("CATEGORIES:", None),
            ("CATEGORIES: \t\u{3000}", None),
            (r"CATEGORIES:\n\N", None),
            ("CATEGORIES:X,Y", None),
            ("CATEGORIES:X,X", None),
            ("CATEGORIES:X\nCATEGORIES:X", None),
            ("CATEGORIES:X,", None),
            ("CATEGORIES:,X", None),
            (r"CATEGORIES:X\x", None),
        ] {
            let events = parse_calendar(&category_calendar(properties)).unwrap();
            assert_eq!(single_course_category(&events[0], &course), expected);
            assert_eq!(single_course_category(&events[0], &all), None);
            assert_eq!(single_course_category(&events[0], &unknown), None);
        }
    }

    #[test]
    fn synthetic_course_related_export_shape_supplies_single_course_keys() {
        let events = parse_calendar(COURSE_RELATED).unwrap();
        assert_eq!(events.len(), 3);
        let feed = course_feed();
        assert_eq!(
            events
                .iter()
                .map(|event| single_course_category(event, &feed))
                .collect::<Vec<_>>(),
            [Some("2026-DB"), Some("Java2-2026"), Some("AI2026_Ak")]
        );
        assert_eq!(map_events(&events, feed.scope()).len(), 2);
    }

    #[test]
    fn basic_deadline_maps_to_one_candidate() {
        let events = parse_calendar(BASIC).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.uid, "1001@cms.example.edu");
        assert_eq!(event.summary, "「演習課題01」の提出期限");
        assert_eq!(event.description.as_deref(), Some(""));
        assert_eq!(event.categories.as_ref().unwrap(), &["Java2-2026"]);
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
        assert_eq!(events[0].categories.as_ref().unwrap(), &["2026-DB"]);
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
    fn datetime_errors_keep_only_event_index_and_known_property_name() {
        for property in ["DTSTART", "DTEND", "LAST-MODIFIED"] {
            let properties = if property == "DTSTART" {
                format!("DTSTART:{PRIVATE_MARKER}")
            } else {
                format!("DTSTART:20261004T000000Z\n{property}:{PRIVATE_MARKER}")
            };
            let error = parse_calendar(&private_calendar(&properties)).unwrap_err();
            assert_eq!(
                error,
                MoodleParseError::UnsupportedDateTime {
                    event_index: 1,
                    property,
                }
            );
            assert_private_marker_absent(&error);

            let invalid_date = "20261304T000000Z";
            let properties = properties.replace(PRIVATE_MARKER, invalid_date);
            let error = parse_calendar(&private_calendar(&properties)).unwrap_err();
            assert_eq!(
                error,
                MoodleParseError::InvalidDateTime {
                    event_index: 1,
                    property,
                }
            );
            assert_private_marker_absent(&error);
            assert!(!format!("{error} {error:?}").contains(invalid_date));
        }
    }

    #[test]
    fn structure_text_and_missing_field_errors_do_not_retain_payloads() {
        for ics in [
            // Reaches the external property parser after component boundary validation.
            private_calendar(&format!("{PRIVATE_MARKER}\nDTSTART:20261004T000000Z")),
            private_calendar("DTSTART:20261004T000000Z")
                .replace("END:VEVENT", &format!("END:{PRIVATE_MARKER}")),
            private_calendar("DTSTART:20261004T000000Z")
                .replace("BEGIN:VEVENT", &format!("BEGIN:{PRIVATE_MARKER}"))
                .replace("END:VEVENT", &format!("END:{PRIVATE_MARKER}")),
            private_calendar("DTSTART:20261004T000000Z").replace(
                &format!("SUMMARY:{PRIVATE_MARKER}"),
                &format!("SUMMARY:{PRIVATE_MARKER}\\x"),
            ),
            private_calendar(&format!("DTSTART:20261004T000000Z\nUID:{PRIVATE_MARKER}")),
        ] {
            let error = parse_calendar(&ics).unwrap_err();
            assert!(matches!(error, MoodleParseError::InvalidIcs(_)));
            assert_private_marker_absent(&error);
        }

        for (line, expected) in [
            (
                format!("UID:{PRIVATE_MARKER}\n"),
                MoodleParseError::MissingUid { event_index: 1 },
            ),
            (
                format!("SUMMARY:{PRIVATE_MARKER}の提出期限\n"),
                MoodleParseError::MissingSummary { event_index: 1 },
            ),
            (
                "DTSTART:20261004T000000Z\n".into(),
                MoodleParseError::MissingDtstart { event_index: 1 },
            ),
        ] {
            let ics = private_calendar("DTSTART:20261004T000000Z").replace(&line, "");
            let error = parse_calendar(&ics).unwrap_err();
            assert_eq!(error, expected);
            assert_private_marker_absent(&error);
        }
    }

    #[test]
    fn mapped_events_use_secret_free_shared_validation_errors() {
        use crate::import::{AssignmentCandidateBatch, ImportValidationError};

        // The Moodle mapper has no error type; validation remains at the batch boundary.
        let events = parse_calendar(&private_calendar("DTSTART:20261004T000000Z")).unwrap();
        let duplicate = vec![events[0].clone(), events[0].clone()];
        let error =
            AssignmentCandidateBatch::try_new(map_events(&duplicate, PRIVATE_MARKER)).unwrap_err();
        assert_eq!(error, ImportValidationError::DuplicateExternalIdentity);
        assert_private_marker_absent(&error);

        let error = AssignmentCandidateBatch::try_new(map_events(&events, " ")).unwrap_err();
        assert_eq!(error, ImportValidationError::EmptyExternalScope);
        assert_private_marker_absent(&error);

        let mut blank_uid = events;
        blank_uid[0].uid = " ".into();
        let error =
            AssignmentCandidateBatch::try_new(map_events(&blank_uid, PRIVATE_MARKER)).unwrap_err();
        assert_eq!(error, ImportValidationError::EmptyExternalId);
        assert_private_marker_absent(&error);
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
