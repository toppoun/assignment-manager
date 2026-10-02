# Moodle ICS fixtures

These fixtures are anonymized and reduced from a real Moodle calendar export.
They are intended for offline parser/mapper tests.

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

## Intended Phase 2 behavior

Parse all VEVENTs into a source-specific `MoodleEvent` first.
Then classify events, for example:
- `...の提出期限` -> submission deadline
- `...の受験可能期間の終了` -> deadline-like event
- `...の受験可能期間の開始` -> availability start
- otherwise -> unknown

Only deadline-like events should become `AssignmentCandidate` in the first implementation.
Do not infer `SubmissionTarget::Moodle` only because an event came from Moodle.
