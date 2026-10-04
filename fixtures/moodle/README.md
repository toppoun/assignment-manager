# Moodle ICS fixtures

The original fixtures are anonymized and reduced from a real Moodle calendar export.
The Phase 7E1A fixture below is fully synthetic. All are for offline parser/mapper tests.

## calendar_basic.ics
- 1 VEVENT
- clear submission deadline
- empty DESCRIPTION
- UTC timestamp (`Z`)

## calendar_multiple.ics
- availability start and availability end are separate VEVENTs
- the two events intentionally use different UIDs
- includes one ordinary submission deadline
- useful for verifying that VEVENT UID is an event ID, not automatically a logical assignment ID

## calendar_empty.ics
- valid VCALENDAR with no VEVENT

## calendar_edge_cases.ics
- empty DESCRIPTION
- escaped `\\n`
- folded content line (continuation line begins with a space)
- external-form wording where Moodle is explicitly not the submission target
- unknown/non-assignment event

## calendar_course_related.ics
- fully synthetic course-related Moodle export shape for Phase 7E1A
- single categories `2026-DB`, `Java2-2026`, `AI2026_Ak`
- no real event UID, title, description, server, token or export file is copied
- availability end, submission deadline and availability start remain separate events

## Phase 7E1A category/feed contract

`MoodleEvent.categories` is `Result<Vec<String>, MoodleCategoryError>`: a decoded
value list across properties, or a fixed metadata error without source payloads.
Order, duplicates, empty values, case, Unicode and whitespace remain opaque.
The parser splits unescaped commas before TEXT decoding, unfolds CRLF/LF plus
exactly one SPACE/HTAB, and preserves whitespace that `ical` 0.11 trims.
Non-category fields still use the existing crate and its whitespace behavior.
Malformed content-line/component structure and required event fields still fail
the calendar parse; category TEXT errors alone keep the event importable.

`single_course_category` permits only a declared `CourseRelated` feed with one
successfully decoded nonblank value. `trim()` checks blanks only. Two identical
values are still ambiguous. It does not discover, create or bind a Course.

`MoodleFeed::new(scope, export_kind, secret_url)` requires an explicit kind and a
safe scope ending in `.course-related`, `.all-events` or `.unknown`, respectively.
The profile label before the suffix is caller-selected and nonempty. Scopes are
retained exactly; legacy unqualified scopes are rejected rather than rewritten.
Changing kind or the logical environment/profile/export contract requires a new
scope. The suffix enforces scope/kind consistency across process restarts; URLs
stay private and are not used to infer kind. Future persistence must retain this
contract. Schema v3 and Moodle Course binding/resolution remain a later phase.

## Intended Phase 2 behavior

Parse all VEVENTs into a source-specific `MoodleEvent` first.
Then classify events, for example:
- `...の提出期限` -> submission deadline
- `...の受験可能期間の終了` -> deadline-like event
- `...の受験可能期間の開始` -> availability start
- otherwise -> unknown

Only deadline-like events should become `AssignmentCandidate` in the first implementation.
Do not infer `SubmissionTarget::Moodle` only because an event came from Moodle.
