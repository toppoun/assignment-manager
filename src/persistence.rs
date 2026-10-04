//! SQLite schema v1 and domain readers. No authentication or import data enters this API.
//!
//! Domain insertion helpers are private and test-only for Phase 6B1. Production
//! writes will be purpose-specific to respect source/local field ownership.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};

use crate::domain::{
    Assignment, AssignmentId, ClassSlot, CompletionState, Course, CourseId, DayOfWeek, Deadline,
    ScheduleEntry, SubmissionTarget,
};

const SCHEMA_VERSION: i64 = 1;
const TABLES: [&str; 5] = [
    "courses",
    "assignments",
    "assignment_sources",
    "course_sources",
    "course_schedule",
];

const SCHEMA_V1: &str = r#"
CREATE TABLE courses (
    id TEXT COLLATE BINARY NOT NULL PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL CHECK (substr(created_at, -1) = 'Z'),
    updated_at TEXT NOT NULL CHECK (substr(updated_at, -1) = 'Z')
) STRICT;

CREATE TABLE assignments (
    id TEXT COLLATE BINARY NOT NULL PRIMARY KEY,
    course_id TEXT COLLATE BINARY REFERENCES courses(id) ON DELETE RESTRICT,
    title TEXT NOT NULL,
    deadline_kind TEXT,
    deadline_value TEXT,
    submission TEXT NOT NULL CHECK (
        submission IN ('unknown', 'none', 'moodle', 'webclass', 'external')
    ),
    submission_url TEXT,
    completion TEXT NOT NULL CHECK (completion IN ('incomplete', 'completed')),
    created_at TEXT NOT NULL CHECK (substr(created_at, -1) = 'Z'),
    updated_at TEXT NOT NULL CHECK (substr(updated_at, -1) = 'Z'),
    CHECK (
        (deadline_kind IS NULL AND deadline_value IS NULL)
        OR (
            deadline_kind IS NOT NULL AND deadline_value IS NOT NULL
            AND deadline_value <> ''
            AND (
                deadline_kind = 'date_only'
                OR (deadline_kind = 'date_time' AND substr(deadline_value, -1) = 'Z')
            )
        )
    )
) STRICT;

CREATE INDEX assignments_course_id_idx ON assignments(course_id);

CREATE TABLE assignment_sources (
    assignment_id TEXT COLLATE BINARY NOT NULL PRIMARY KEY
        REFERENCES assignments(id) ON DELETE CASCADE,
    origin TEXT NOT NULL CHECK (origin IN ('moodle', 'webclass', 'manual')),
    scope TEXT COLLATE BINARY NOT NULL CHECK (scope <> ''),
    external_id TEXT COLLATE BINARY NOT NULL CHECK (external_id <> ''),
    UNIQUE (origin, scope, external_id)
) STRICT;

CREATE TABLE course_sources (
    course_id TEXT COLLATE BINARY NOT NULL PRIMARY KEY
        REFERENCES courses(id) ON DELETE CASCADE,
    origin TEXT NOT NULL CHECK (origin IN ('moodle', 'webclass', 'manual')),
    external_id TEXT COLLATE BINARY NOT NULL CHECK (external_id <> ''),
    UNIQUE (origin, external_id)
) STRICT;

CREATE TABLE course_schedule (
    course_id TEXT COLLATE BINARY NOT NULL REFERENCES courses(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('timetabled', 'on_demand')),
    day TEXT CHECK (day IN ('mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun')),
    period INTEGER CHECK (period BETWEEN 0 AND 255),
    PRIMARY KEY (course_id, position),
    CHECK (
        (entry_kind = 'timetabled' AND day IS NOT NULL AND period IS NOT NULL)
        OR (entry_kind = 'on_demand' AND day IS NULL AND period IS NULL)
    )
) STRICT;
"#;

/// Fixed classifications only. Paths, SQL, row values and driver causes are discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceError {
    Open,
    Configure,
    Migration,
    UnsupportedSchemaVersion,
    Database,
    InvalidStoredData,
    Constraint,
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "could not open SQLite database",
            Self::Configure => "could not configure SQLite connection",
            Self::Migration => "could not initialize SQLite schema",
            Self::UnsupportedSchemaVersion => "unsupported SQLite schema version",
            Self::Database => "SQLite operation failed",
            Self::InvalidStoredData => "invalid stored domain data",
            Self::Constraint => "SQLite constraint violation",
        })
    }
}

impl Error for PersistenceError {}

/// Concrete synchronous SQLite store. The caller chooses the database path.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open a caller-supplied path, configure its connection and migrate empty databases.
    /// Future schema versions and nonempty version-0 databases are rejected.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let conn = Connection::open(path).map_err(|_| PersistenceError::Open)?;
        Self::from_connection(conn)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, PersistenceError> {
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    fn open_in_memory() -> Result<Self, PersistenceError> {
        let conn = Connection::open_in_memory().map_err(|_| PersistenceError::Open)?;
        Self::from_connection(conn)
    }

    /// Read an assignment by its exact, opaque internal ID; absence is `None`.
    pub fn get_assignment(
        &self,
        id: &AssignmentId,
    ) -> Result<Option<Assignment>, PersistenceError> {
        let stored = self
            .conn
            .query_row(
                "SELECT id, course_id, title, deadline_kind, deadline_value, submission,
                        submission_url, completion, created_at, updated_at
                 FROM assignments WHERE id = ?1",
                [id.as_str()],
                StoredAssignment::from_row,
            )
            .optional()
            .map_err(database_error)?;
        stored.map(StoredAssignment::decode).transpose()
    }

    /// Read a course and its ordered schedule from one consistent database snapshot.
    pub fn get_course(&self, id: &CourseId) -> Result<Option<Course>, PersistenceError> {
        // The connection is private, so callers cannot nest transactions here.
        let tx = self.conn.unchecked_transaction().map_err(database_error)?;
        let stored = tx
            .query_row(
                "SELECT id, name, created_at, updated_at FROM courses WHERE id = ?1",
                [id.as_str()],
                StoredCourse::from_row,
            )
            .optional()
            .map_err(database_error)?;
        let course = if let Some(stored) = stored {
            validate_metadata(&stored.created_at, &stored.updated_at)?;
            let mut schedule = Vec::new();
            let mut stmt = tx
                .prepare(
                    "SELECT position, entry_kind, day, period
                     FROM course_schedule WHERE course_id = ?1 ORDER BY position",
                )
                .map_err(database_error)?;
            let mut rows = stmt.query([id.as_str()]).map_err(database_error)?;
            while let Some(row) = rows.next().map_err(database_error)? {
                let position: i64 = row.get(0).map_err(database_error)?;
                if usize::try_from(position).ok() != Some(schedule.len()) {
                    return Err(PersistenceError::InvalidStoredData);
                }
                let kind: String = row.get(1).map_err(database_error)?;
                let day: Option<String> = row.get(2).map_err(database_error)?;
                let period: Option<i64> = row.get(3).map_err(database_error)?;
                schedule.push(decode_schedule(&kind, day.as_deref(), period)?);
            }
            Some(Course {
                id: CourseId::new(stored.id),
                name: stored.name,
                schedule,
            })
        } else {
            None
        };
        tx.commit().map_err(database_error)?;
        Ok(course)
    }
}

fn configure(conn: &Connection) -> Result<(), PersistenceError> {
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(|_| PersistenceError::Configure)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|_| PersistenceError::Configure)?;
    let foreign_keys: i64 = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|_| PersistenceError::Configure)?;
    if foreign_keys != 1 {
        return Err(PersistenceError::Configure);
    }
    let journal: String = conn
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .map_err(|_| PersistenceError::Configure)?;
    // SQLite keeps MEMORY journaling for an in-memory database.
    if journal != "delete" && !(journal == "memory" && conn.path() == Some("")) {
        return Err(PersistenceError::Configure);
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| PersistenceError::Configure)?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), PersistenceError> {
    // Lock before inspecting the version/tables so two first opens cannot race.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| PersistenceError::Migration)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| PersistenceError::Migration)?;
    match version {
        0 => {
            let has_tables: bool = tx
                .query_row(
                    "SELECT EXISTS (
                        SELECT 1 FROM sqlite_schema
                        WHERE type = 'table' AND name NOT GLOB 'sqlite_*'
                    )",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| PersistenceError::Migration)?;
            if has_tables {
                return Err(PersistenceError::Migration);
            }
            tx.execute_batch(SCHEMA_V1)
                .map_err(|_| PersistenceError::Migration)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|_| PersistenceError::Migration)?;
        }
        SCHEMA_VERSION => {
            for table in TABLES {
                let exists: bool = tx
                    .query_row(
                        "SELECT EXISTS (
                            SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1
                        )",
                        [table],
                        |row| row.get(0),
                    )
                    .map_err(|_| PersistenceError::Migration)?;
                if !exists {
                    return Err(PersistenceError::Migration);
                }
            }
        }
        v if v > SCHEMA_VERSION => return Err(PersistenceError::UnsupportedSchemaVersion),
        _ => return Err(PersistenceError::Migration),
    }
    tx.commit().map_err(|_| PersistenceError::Migration)
}

fn database_error(error: rusqlite::Error) -> PersistenceError {
    if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) {
        return PersistenceError::Constraint;
    }
    match error {
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::Utf8Error(..) => PersistenceError::InvalidStoredData,
        _ => PersistenceError::Database,
    }
}

struct StoredAssignment {
    id: String,
    course_id: Option<String>,
    title: String,
    deadline_kind: Option<String>,
    deadline_value: Option<String>,
    submission: String,
    submission_url: Option<String>,
    completion: String,
    created_at: String,
    updated_at: String,
}

impl StoredAssignment {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            course_id: row.get(1)?,
            title: row.get(2)?,
            deadline_kind: row.get(3)?,
            deadline_value: row.get(4)?,
            submission: row.get(5)?,
            submission_url: row.get(6)?,
            completion: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        })
    }

    fn decode(self) -> Result<Assignment, PersistenceError> {
        validate_metadata(&self.created_at, &self.updated_at)?;
        Ok(Assignment {
            id: AssignmentId::new(self.id),
            course_id: self.course_id.map(CourseId::new),
            title: self.title,
            deadline: decode_deadline(
                self.deadline_kind.as_deref(),
                self.deadline_value.as_deref(),
            )?,
            submission: decode_submission(&self.submission)?,
            submission_url: self.submission_url,
            completion: decode_completion(&self.completion)?,
        })
    }
}

struct StoredCourse {
    id: String,
    name: String,
    created_at: String,
    updated_at: String,
}

impl StoredCourse {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
        })
    }
}

fn encode_timestamp(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn decode_timestamp(value: &str) -> Result<DateTime<Utc>, PersistenceError> {
    // Chrono's ISO parser also supports signed extended years allowed by the
    // domain. Comparing the re-encoding still enforces one exact UTC format.
    let instant = value
        .parse::<DateTime<Utc>>()
        .map_err(|_| PersistenceError::InvalidStoredData)?;
    if encode_timestamp(&instant) != value {
        return Err(PersistenceError::InvalidStoredData);
    }
    Ok(instant)
}

fn validate_metadata(created_at: &str, updated_at: &str) -> Result<(), PersistenceError> {
    decode_timestamp(created_at)?;
    decode_timestamp(updated_at)?;
    Ok(())
}

#[cfg(test)]
fn encode_deadline(deadline: Option<&Deadline>) -> (Option<&'static str>, Option<String>) {
    match deadline {
        None => (None, None),
        Some(Deadline::DateOnly(date)) => {
            (Some("date_only"), Some(date.format("%Y-%m-%d").to_string()))
        }
        Some(Deadline::DateTime(instant)) => (Some("date_time"), Some(encode_timestamp(instant))),
    }
}

fn decode_deadline(
    kind: Option<&str>,
    value: Option<&str>,
) -> Result<Option<Deadline>, PersistenceError> {
    match (kind, value) {
        (None, None) => Ok(None),
        (Some("date_only"), Some(value)) => {
            let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map_err(|_| PersistenceError::InvalidStoredData)?;
            if date.format("%Y-%m-%d").to_string() != value {
                return Err(PersistenceError::InvalidStoredData);
            }
            Ok(Some(Deadline::DateOnly(date)))
        }
        (Some("date_time"), Some(value)) => Ok(Some(Deadline::DateTime(decode_timestamp(value)?))),
        _ => Err(PersistenceError::InvalidStoredData),
    }
}

#[cfg(test)]
fn encode_completion(value: CompletionState) -> &'static str {
    match value {
        CompletionState::Incomplete => "incomplete",
        CompletionState::Completed => "completed",
    }
}

fn decode_completion(value: &str) -> Result<CompletionState, PersistenceError> {
    match value {
        "incomplete" => Ok(CompletionState::Incomplete),
        "completed" => Ok(CompletionState::Completed),
        _ => Err(PersistenceError::InvalidStoredData),
    }
}

#[cfg(test)]
fn encode_submission(value: SubmissionTarget) -> &'static str {
    match value {
        SubmissionTarget::Unknown => "unknown",
        SubmissionTarget::None => "none",
        SubmissionTarget::Moodle => "moodle",
        SubmissionTarget::WebClass => "webclass",
        SubmissionTarget::External => "external",
    }
}

fn decode_submission(value: &str) -> Result<SubmissionTarget, PersistenceError> {
    match value {
        "unknown" => Ok(SubmissionTarget::Unknown),
        "none" => Ok(SubmissionTarget::None),
        "moodle" => Ok(SubmissionTarget::Moodle),
        "webclass" => Ok(SubmissionTarget::WebClass),
        "external" => Ok(SubmissionTarget::External),
        _ => Err(PersistenceError::InvalidStoredData),
    }
}

#[cfg(test)]
fn encode_day(value: DayOfWeek) -> &'static str {
    match value {
        DayOfWeek::Monday => "mon",
        DayOfWeek::Tuesday => "tue",
        DayOfWeek::Wednesday => "wed",
        DayOfWeek::Thursday => "thu",
        DayOfWeek::Friday => "fri",
        DayOfWeek::Saturday => "sat",
        DayOfWeek::Sunday => "sun",
    }
}

fn decode_day(value: &str) -> Result<DayOfWeek, PersistenceError> {
    match value {
        "mon" => Ok(DayOfWeek::Monday),
        "tue" => Ok(DayOfWeek::Tuesday),
        "wed" => Ok(DayOfWeek::Wednesday),
        "thu" => Ok(DayOfWeek::Thursday),
        "fri" => Ok(DayOfWeek::Friday),
        "sat" => Ok(DayOfWeek::Saturday),
        "sun" => Ok(DayOfWeek::Sunday),
        _ => Err(PersistenceError::InvalidStoredData),
    }
}

#[cfg(test)]
fn encode_schedule(entry: ScheduleEntry) -> (&'static str, Option<&'static str>, Option<u8>) {
    match entry {
        ScheduleEntry::OnDemand => ("on_demand", None, None),
        ScheduleEntry::Timetabled(slot) => {
            ("timetabled", Some(encode_day(slot.day)), Some(slot.period))
        }
    }
}

fn decode_schedule(
    kind: &str,
    day: Option<&str>,
    period: Option<i64>,
) -> Result<ScheduleEntry, PersistenceError> {
    match (kind, day, period) {
        ("on_demand", None, None) => Ok(ScheduleEntry::OnDemand),
        ("timetabled", Some(day), Some(period)) => Ok(ScheduleEntry::Timetabled(ClassSlot {
            day: decode_day(day)?,
            period: u8::try_from(period).map_err(|_| PersistenceError::InvalidStoredData)?,
        })),
        _ => Err(PersistenceError::InvalidStoredData),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{ffi, params};

    use super::*;
    use crate::import::OriginKind;

    const STAMP: &str = "2026-10-04T14:59:00.123456789Z";
    const PRIVATE: &str = "SYNTHETIC_PRIVATE_VALUE_6B1";

    // Intentionally absent from production builds: these insert supplied internal
    // IDs, without upsert, merge, source binding or completion update semantics.
    impl SqliteStore {
        fn insert_assignment(&self, assignment: &Assignment) -> Result<(), PersistenceError> {
            let (kind, value) = encode_deadline(assignment.deadline.as_ref());
            let timestamp = encode_timestamp(&Utc::now());
            self.conn
                .execute(
                    "INSERT INTO assignments (
                        id, course_id, title, deadline_kind, deadline_value, submission,
                        submission_url, completion, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        assignment.id.as_str(),
                        assignment.course_id.as_ref().map(CourseId::as_str),
                        assignment.title,
                        kind,
                        value,
                        encode_submission(assignment.submission),
                        assignment.submission_url,
                        encode_completion(assignment.completion),
                        timestamp,
                    ],
                )
                .map_err(database_error)?;
            Ok(())
        }

        fn insert_course(&mut self, course: &Course) -> Result<(), PersistenceError> {
            let tx = self.conn.transaction().map_err(database_error)?;
            let timestamp = encode_timestamp(&Utc::now());
            tx.execute(
                "INSERT INTO courses (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![course.id.as_str(), course.name, timestamp],
            )
            .map_err(database_error)?;
            for (position, entry) in course.schedule.iter().enumerate() {
                let position = i64::try_from(position).map_err(|_| PersistenceError::Constraint)?;
                let (kind, day, period) = encode_schedule(*entry);
                tx.execute(
                    "INSERT INTO course_schedule (course_id, position, entry_kind, day, period)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![course.id.as_str(), position, kind, day, period],
                )
                .map_err(database_error)?;
            }
            tx.commit().map_err(database_error)
        }
    }

    // Source codecs are test-only until Phase 6B2 introduces source readers/writers.
    fn encode_origin(origin: OriginKind) -> &'static str {
        match origin {
            OriginKind::Moodle => "moodle",
            OriginKind::WebClass => "webclass",
            OriginKind::Manual => "manual",
        }
    }

    fn decode_origin(value: &str) -> Result<OriginKind, PersistenceError> {
        match value {
            "moodle" => Ok(OriginKind::Moodle),
            "webclass" => Ok(OriginKind::WebClass),
            "manual" => Ok(OriginKind::Manual),
            _ => Err(PersistenceError::InvalidStoredData),
        }
    }

    struct TempDb {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TempDb {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            for _ in 0..100 {
                let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let directory = std::env::temp_dir().join(format!(
                    "assignment-manager-persistence-{}-{nonce}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&directory) {
                    Ok(()) => {
                        return Self {
                            path: directory.join("test.sqlite3"),
                            directory,
                        };
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not reserve test directory: {:?}", error.kind()),
                }
            }
            panic!("could not reserve a unique test directory");
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            // Remove only known test files, without recursive filesystem deletion.
            for name in [
                "test.sqlite3",
                "test.sqlite3-journal",
                "test.sqlite3-wal",
                "test.sqlite3-shm",
            ] {
                let _ = fs::remove_file(self.directory.join(name));
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }

    fn assignment(id: &str) -> Assignment {
        Assignment {
            id: AssignmentId::new(id),
            course_id: None,
            title: "Synthetic report 日本語 ' ?1".into(),
            deadline: None,
            submission: SubmissionTarget::Unknown,
            submission_url: Some("https://submit.example.invalid/answer?q='value'".into()),
            completion: CompletionState::Incomplete,
        }
    }

    fn course(id: &str, schedule: Vec<ScheduleEntry>) -> Course {
        Course {
            id: CourseId::new(id),
            name: "Synthetic course 日本語 ' ?1".into(),
            schedule,
        }
    }

    fn slot(day: DayOfWeek, period: u8) -> ScheduleEntry {
        ScheduleEntry::Timetabled(ClassSlot { day, period })
    }

    fn pragma_number(conn: &Connection, name: &str) -> i64 {
        conn.pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        // Only fixed test-owned table names are interpolated here.
        assert!(TABLES.contains(&table));
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn assert_constraint(result: rusqlite::Result<usize>, extended_code: i32) {
        let error = result.unwrap_err();
        assert_eq!(error.sqlite_error().unwrap().extended_code, extended_code);
        assert_eq!(database_error(error), PersistenceError::Constraint);
    }

    fn assert_safe(error: PersistenceError) {
        assert!(!error.to_string().contains(PRIVATE));
        assert!(!format!("{error:?}").contains(PRIVATE));
        assert!(error.source().is_none());
    }

    #[test]
    fn empty_database_migrates_to_five_strict_tables_and_index() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(pragma_number(&store.conn, "user_version"), 1);
        let table_count: i64 = store.conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB 'sqlite_*'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(table_count, 5);
        for table in TABLES {
            let strict: i64 = store
                .conn
                .query_row(
                    "SELECT strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(strict, 1);
        }
        let indexed_column: String = store
            .conn
            .query_row(
                "SELECT name FROM pragma_index_info('assignments_course_id_idx')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed_column, "course_id");
    }

    #[test]
    fn connection_policy_is_set_on_every_open() {
        let memory = SqliteStore::open_in_memory().unwrap();
        assert_eq!(pragma_number(&memory.conn, "foreign_keys"), 1);
        assert_eq!(pragma_number(&memory.conn, "busy_timeout"), 5000);
        assert_eq!(pragma_number(&memory.conn, "synchronous"), 2);
        let temp = TempDb::new();
        for _ in 0..2 {
            let store = SqliteStore::open(&temp.path).unwrap();
            assert_eq!(pragma_number(&store.conn, "foreign_keys"), 1);
            assert_eq!(pragma_number(&store.conn, "busy_timeout"), 5000);
            assert_eq!(pragma_number(&store.conn, "synchronous"), 2);
            let journal: String = store
                .conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(journal, "delete");
        }
    }

    #[test]
    fn configuration_rejects_foreign_keys_that_cannot_be_enabled() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN")
            .unwrap();
        assert_eq!(configure(&conn), Err(PersistenceError::Configure));
        conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn future_schema_version_is_rejected_without_changing_data() {
        let temp = TempDb::new();
        {
            let conn = Connection::open(&temp.path).unwrap();
            conn.execute_batch("CREATE TABLE future_data (value TEXT); PRAGMA user_version = 2")
                .unwrap();
            conn.execute("INSERT INTO future_data VALUES (?1)", [PRIVATE])
                .unwrap();
        }
        let error = SqliteStore::open(&temp.path).err().unwrap();
        assert_eq!(error, PersistenceError::UnsupportedSchemaVersion);
        assert_safe(error);
        let conn = Connection::open(&temp.path).unwrap();
        assert_eq!(pragma_number(&conn, "user_version"), 2);
        let value: String = conn
            .query_row("SELECT value FROM future_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, PRIVATE);
    }

    #[test]
    fn nonempty_version_zero_is_rejected_without_overwriting_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE unknown_table (value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO unknown_table VALUES (?1)", [PRIVATE])
            .unwrap();
        assert_eq!(migrate(&mut conn), Err(PersistenceError::Migration));
        assert_eq!(pragma_number(&conn, "user_version"), 0);
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let value: String = conn
            .query_row("SELECT value FROM unknown_table", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, PRIVATE);
    }

    #[test]
    fn failed_migration_rolls_back_earlier_ddl_and_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        // A view is not a user table. Its name causes the second CREATE TABLE to
        // fail after courses was created, exercising real transaction rollback.
        conn.execute_batch("CREATE VIEW assignments AS SELECT 1 AS synthetic")
            .unwrap();
        assert_eq!(migrate(&mut conn), Err(PersistenceError::Migration));
        assert!(conn.is_autocommit());
        assert_eq!(pragma_number(&conn, "user_version"), 0);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        conn.execute_batch("DROP VIEW assignments").unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(pragma_number(&conn, "user_version"), 1);
    }

    #[test]
    fn version_one_with_a_missing_required_table_is_rejected() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .conn
            .execute_batch("DROP TABLE course_sources")
            .unwrap();
        assert_eq!(migrate(&mut store.conn), Err(PersistenceError::Migration));
        assert_eq!(pragma_number(&store.conn, "user_version"), 1);
    }

    #[test]
    fn assignment_round_trip_covers_deadlines_enums_relations_and_nullable_url() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let related = course(" c_0001 Mixed CASE ", vec![]);
        store.insert_course(&related).unwrap();
        let deadlines = [
            None,
            Some(Deadline::DateOnly(
                NaiveDate::from_ymd_opt(2028, 2, 29).unwrap(),
            )),
            Some(Deadline::DateOnly(
                NaiveDate::from_ymd_opt(2026, 10, 4).unwrap(),
            )),
            Some(Deadline::DateTime(
                DateTime::parse_from_rfc3339("2026-10-05T00:00:00+09:00")
                    .unwrap()
                    .with_timezone(&Utc),
            )),
            Some(Deadline::DateTime(decode_timestamp(STAMP).unwrap())),
            Some(Deadline::DateTime(
                decode_timestamp("2026-10-04T23:59:59.999999999Z").unwrap(),
            )),
            Some(Deadline::DateTime(
                decode_timestamp("2026-10-05T00:00:00.000000001Z").unwrap(),
            )),
        ];
        for (index, deadline) in deadlines.into_iter().enumerate() {
            for submission in [
                SubmissionTarget::Unknown,
                SubmissionTarget::None,
                SubmissionTarget::Moodle,
                SubmissionTarget::WebClass,
                SubmissionTarget::External,
            ] {
                for completion in [CompletionState::Incomplete, CompletionState::Completed] {
                    let mut expected = assignment(&format!(
                        " a_{index}_{}_{} ",
                        encode_submission(submission),
                        encode_completion(completion)
                    ));
                    expected.course_id = (index % 2 == 0).then(|| related.id.clone());
                    expected.deadline = deadline.clone();
                    expected.submission = submission;
                    expected.completion = completion;
                    if index % 2 == 0 {
                        expected.submission_url = None;
                    }
                    store.insert_assignment(&expected).unwrap();
                    assert_eq!(store.get_assignment(&expected.id).unwrap(), Some(expected));
                }
            }
        }
        assert_eq!(
            store.get_assignment(&AssignmentId::new("absent")).unwrap(),
            None
        );
    }

    #[test]
    fn course_round_trip_preserves_empty_mixed_duplicate_and_extreme_schedule_entries() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let schedules = [
            vec![],
            vec![ScheduleEntry::OnDemand],
            vec![
                slot(DayOfWeek::Sunday, 255),
                ScheduleEntry::OnDemand,
                slot(DayOfWeek::Monday, 0),
                slot(DayOfWeek::Monday, 0),
                slot(DayOfWeek::Tuesday, 1),
                slot(DayOfWeek::Wednesday, 2),
                ScheduleEntry::OnDemand,
                slot(DayOfWeek::Thursday, 3),
                slot(DayOfWeek::Friday, 4),
                slot(DayOfWeek::Saturday, 5),
                slot(DayOfWeek::Monday, 6),
            ],
        ];
        for (index, schedule) in schedules.into_iter().enumerate() {
            let expected = course(&format!(" c_{index} "), schedule);
            store.insert_course(&expected).unwrap();
            assert_eq!(store.get_course(&expected.id).unwrap(), Some(expected));
        }
        assert_eq!(store.get_course(&CourseId::new("absent")).unwrap(), None);
    }

    #[test]
    fn opaque_ids_and_empty_domain_strings_are_not_normalized_or_restricted() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for id in ["", "x", "X", " x ", "0001"] {
            let mut expected_course = course(id, vec![]);
            expected_course.name.clear();
            store.insert_course(&expected_course).unwrap();
            let mut expected_assignment = assignment(id);
            expected_assignment.title.clear();
            expected_assignment.course_id = Some(expected_course.id.clone());
            expected_assignment.submission_url = Some(String::new());
            store.insert_assignment(&expected_assignment).unwrap();
            assert_eq!(
                store.get_course(&expected_course.id).unwrap(),
                Some(expected_course)
            );
            assert_eq!(
                store.get_assignment(&expected_assignment.id).unwrap(),
                Some(expected_assignment)
            );
        }
    }

    #[test]
    fn metadata_is_canonical_and_remains_outside_domain_values() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let expected_course = course("c_metadata", vec![]);
        let expected_assignment = assignment("a_metadata");
        let before = Utc::now();
        store.insert_course(&expected_course).unwrap();
        store.insert_assignment(&expected_assignment).unwrap();
        let after = Utc::now();
        for table in ["courses", "assignments"] {
            let (created, updated): (String, String) = store
                .conn
                .query_row(
                    &format!("SELECT created_at, updated_at FROM {table}"),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(created, updated);
            assert_eq!(created.len(), 30);
            let instant = decode_timestamp(&created).unwrap();
            assert!(before <= instant && instant <= after);
        }
        assert_eq!(
            store.get_course(&expected_course.id).unwrap(),
            Some(expected_course)
        );
        assert_eq!(
            store.get_assignment(&expected_assignment.id).unwrap(),
            Some(expected_assignment)
        );
        assert_eq!(encode_timestamp(&decode_timestamp(STAMP).unwrap()), STAMP);
    }

    #[test]
    fn schedule_write_failure_rolls_back_course_and_all_schedule_rows() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_second_slot BEFORE INSERT ON course_schedule
             WHEN NEW.position = 1 BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;",
            )
            .unwrap();
        let expected = course(
            "c_atomic",
            vec![ScheduleEntry::OnDemand, slot(DayOfWeek::Monday, 1)],
        );
        assert_eq!(
            store.insert_course(&expected),
            Err(PersistenceError::Constraint)
        );
        assert_eq!(count(&store.conn, "courses"), 0);
        assert_eq!(count(&store.conn, "course_schedule"), 0);
        assert!(store.conn.is_autocommit());
    }

    #[test]
    fn missing_course_is_an_fk_error_and_is_never_created_implicitly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut expected = assignment("a_missing_course");
        expected.course_id = Some(CourseId::new("missing"));
        assert_eq!(
            store.insert_assignment(&expected),
            Err(PersistenceError::Constraint)
        );
        assert_eq!(count(&store.conn, "courses"), 0);
        assert_eq!(count(&store.conn, "assignments"), 0);
    }

    #[test]
    fn file_reopen_restores_domain_metadata_and_existing_schema() {
        let temp = TempDb::new();
        let expected_course = course(
            "c_reopen",
            vec![
                slot(DayOfWeek::Friday, 255),
                ScheduleEntry::OnDemand,
                slot(DayOfWeek::Monday, 0),
            ],
        );
        let mut expected_assignment = assignment("a_reopen");
        expected_assignment.course_id = Some(expected_course.id.clone());
        expected_assignment.deadline = Some(Deadline::DateTime(decode_timestamp(STAMP).unwrap()));
        expected_assignment.submission = SubmissionTarget::WebClass;
        expected_assignment.completion = CompletionState::Completed;
        let schema;
        let metadata;
        {
            let mut store = SqliteStore::open(&temp.path).unwrap();
            store.insert_course(&expected_course).unwrap();
            store.insert_assignment(&expected_assignment).unwrap();
            schema = store
                .conn
                .query_row(
                    "SELECT group_concat(sql, ';') FROM sqlite_schema",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            metadata = store
                .conn
                .query_row(
                    "SELECT created_at, updated_at FROM assignments",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap();
        }
        let store = SqliteStore::open(&temp.path).unwrap();
        assert_eq!(pragma_number(&store.conn, "user_version"), 1);
        assert_eq!(
            store.get_course(&expected_course.id).unwrap(),
            Some(expected_course)
        );
        assert_eq!(
            store.get_assignment(&expected_assignment.id).unwrap(),
            Some(expected_assignment)
        );
        let reopened_schema: String = store
            .conn
            .query_row(
                "SELECT group_concat(sql, ';') FROM sqlite_schema",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let reopened_metadata: (String, String) = store
            .conn
            .query_row(
                "SELECT created_at, updated_at FROM assignments",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reopened_schema, schema);
        assert_eq!(reopened_metadata, metadata);
    }

    #[test]
    fn deadline_storage_uses_explicit_codes_and_exact_utc_nanoseconds() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut expected = assignment("a_date");
        for (deadline, kind, value) in [
            (None, None, None),
            (
                Some(Deadline::DateOnly(
                    NaiveDate::from_ymd_opt(2028, 2, 29).unwrap(),
                )),
                Some("date_only"),
                Some("2028-02-29"),
            ),
            (
                Some(Deadline::DateTime(decode_timestamp(STAMP).unwrap())),
                Some("date_time"),
                Some(STAMP),
            ),
        ] {
            expected.deadline = deadline;
            store.insert_assignment(&expected).unwrap();
            let stored: (Option<String>, Option<String>) = store
                .conn
                .query_row(
                    "SELECT deadline_kind, deadline_value FROM assignments WHERE id = ?1",
                    [expected.id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(stored.0.as_deref(), kind);
            assert_eq!(stored.1.as_deref(), value);
            store.conn.execute("DELETE FROM assignments", []).unwrap();
        }
    }

    #[test]
    fn deadline_codec_rejects_invalid_values_and_null_mismatches() {
        for (kind, value) in [
            (Some("private_invalid_kind"), Some(STAMP)),
            (Some("date_only"), Some("2026-02-29")),
            (Some("date_only"), Some("2026-2-01")),
            (Some("date_only"), Some("")),
            (Some("date_time"), Some("not-a-datetimeZ")),
            (Some("date_time"), Some("2026-10-04T14:59:00Z")),
            (Some("date_time"), Some("2026-10-04T14:59:00.1234567890Z")),
            (Some("date_time"), Some("2026-10-04t14:59:00.123456789z")),
            (
                Some("date_time"),
                Some("2026-10-04T14:59:00.123456789+00:00"),
            ),
            (
                Some("date_time"),
                Some("2026-10-04T23:59:00.123456789+09:00"),
            ),
            (None, Some(STAMP)),
            (Some("date_only"), None),
        ] {
            assert_eq!(
                decode_deadline(kind, value),
                Err(PersistenceError::InvalidStoredData)
            );
        }
    }

    #[test]
    fn deadline_codec_preserves_the_chrono_domain_date_range_and_leap_seconds() {
        let store = SqliteStore::open_in_memory().unwrap();
        let dates = [
            NaiveDate::MIN,
            NaiveDate::from_ymd_opt(-1, 12, 31).unwrap(),
            NaiveDate::from_ymd_opt(0, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(10_000, 1, 1).unwrap(),
            NaiveDate::MAX,
        ];
        for (index, date) in dates.into_iter().enumerate() {
            for deadline in [
                Deadline::DateOnly(date),
                Deadline::DateTime(
                    date.and_hms_nano_opt(23, 59, 59, 123_456_789)
                        .unwrap()
                        .and_utc(),
                ),
            ] {
                let (kind, value) = encode_deadline(Some(&deadline));
                assert_eq!(
                    decode_deadline(kind, value.as_deref()),
                    Ok(Some(deadline.clone()))
                );
                let mut expected = assignment(&format!("a_range_{index}_{}", kind.unwrap()));
                expected.deadline = Some(deadline);
                store.insert_assignment(&expected).unwrap();
                assert_eq!(store.get_assignment(&expected.id).unwrap(), Some(expected));
            }
        }
        let instant = NaiveDate::from_ymd_opt(2016, 12, 31)
            .unwrap()
            .and_hms_nano_opt(23, 59, 59, 1_123_456_789)
            .unwrap()
            .and_utc();
        assert_eq!(decode_timestamp(&encode_timestamp(&instant)), Ok(instant));
    }

    #[test]
    fn explicit_enum_codecs_round_trip_and_reject_unknown_codes() {
        for (target, code) in [
            (SubmissionTarget::Unknown, "unknown"),
            (SubmissionTarget::None, "none"),
            (SubmissionTarget::Moodle, "moodle"),
            (SubmissionTarget::WebClass, "webclass"),
            (SubmissionTarget::External, "external"),
        ] {
            assert_eq!(encode_submission(target), code);
            assert_eq!(decode_submission(code), Ok(target));
        }
        for (state, code) in [
            (CompletionState::Incomplete, "incomplete"),
            (CompletionState::Completed, "completed"),
        ] {
            assert_eq!(encode_completion(state), code);
            assert_eq!(decode_completion(code), Ok(state));
        }
        for (origin, code) in [
            (OriginKind::Moodle, "moodle"),
            (OriginKind::WebClass, "webclass"),
            (OriginKind::Manual, "manual"),
        ] {
            assert_eq!(encode_origin(origin), code);
            assert_eq!(decode_origin(code), Ok(origin));
        }
        for (day, code) in [
            (DayOfWeek::Monday, "mon"),
            (DayOfWeek::Tuesday, "tue"),
            (DayOfWeek::Wednesday, "wed"),
            (DayOfWeek::Thursday, "thu"),
            (DayOfWeek::Friday, "fri"),
            (DayOfWeek::Saturday, "sat"),
            (DayOfWeek::Sunday, "sun"),
        ] {
            assert_eq!(encode_day(day), code);
            assert_eq!(decode_day(code), Ok(day));
        }
        for unknown in ["", "UNKNOWN", "0", PRIVATE] {
            assert_eq!(
                decode_submission(unknown),
                Err(PersistenceError::InvalidStoredData)
            );
            assert_eq!(
                decode_completion(unknown),
                Err(PersistenceError::InvalidStoredData)
            );
            assert_eq!(
                decode_origin(unknown),
                Err(PersistenceError::InvalidStoredData)
            );
            assert_eq!(
                decode_day(unknown),
                Err(PersistenceError::InvalidStoredData)
            );
        }
    }

    #[test]
    fn schedule_codec_rejects_unknown_or_inconsistent_entries() {
        for (kind, day, period) in [
            (PRIVATE, None, None),
            ("on_demand", Some("mon"), None),
            ("on_demand", None, Some(0)),
            ("timetabled", None, Some(1)),
            ("timetabled", Some("mon"), None),
            ("timetabled", Some(PRIVATE), Some(1)),
            ("timetabled", Some("mon"), Some(-1)),
            ("timetabled", Some("mon"), Some(256)),
        ] {
            assert_eq!(
                decode_schedule(kind, day, period),
                Err(PersistenceError::InvalidStoredData)
            );
        }
    }

    #[test]
    fn readers_reject_corrupted_deadline_enums_and_metadata_without_fallback() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let expected = assignment("a_corrupt");
        let expected_course = course("c_corrupt", vec![]);
        store.insert_assignment(&expected).unwrap();
        store.insert_course(&expected_course).unwrap();
        // These updates emulate damaged/externally-written rows. This pragma is
        // never set by production code, and every reader still validates codes.
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        for (kind, value) in [
            (Some(PRIVATE), Some(STAMP)),
            (Some("date_only"), Some("2026-02-29")),
            (Some("date_time"), Some("invalidZ")),
            (None, Some(STAMP)),
            (Some("date_only"), None),
        ] {
            store
                .conn
                .execute(
                    "UPDATE assignments SET deadline_kind = ?1, deadline_value = ?2",
                    params![kind, value],
                )
                .unwrap();
            assert_eq!(
                store.get_assignment(&expected.id),
                Err(PersistenceError::InvalidStoredData)
            );
        }
        store
            .conn
            .execute_batch("UPDATE assignments SET deadline_kind = NULL, deadline_value = NULL")
            .unwrap();
        for column in ["submission", "completion", "created_at", "updated_at"] {
            let original: String = store
                .conn
                .query_row(&format!("SELECT {column} FROM assignments"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            store
                .conn
                .execute(&format!("UPDATE assignments SET {column} = ?1"), [PRIVATE])
                .unwrap();
            let error = store.get_assignment(&expected.id).unwrap_err();
            assert_eq!(error, PersistenceError::InvalidStoredData);
            assert_safe(error);
            store
                .conn
                .execute(&format!("UPDATE assignments SET {column} = ?1"), [original])
                .unwrap();
        }
        for column in ["created_at", "updated_at"] {
            let original: String = store
                .conn
                .query_row(&format!("SELECT {column} FROM courses"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            for invalid in [
                PRIVATE,
                "2026-10-04T14:59:00Z",
                "2026-10-04T14:59:00.123456789+00:00",
                "invalidZ",
            ] {
                store
                    .conn
                    .execute(&format!("UPDATE courses SET {column} = ?1"), [invalid])
                    .unwrap();
                assert_eq!(
                    store.get_course(&expected_course.id),
                    Err(PersistenceError::InvalidStoredData)
                );
            }
            store
                .conn
                .execute(&format!("UPDATE courses SET {column} = ?1"), [original])
                .unwrap();
        }
    }

    #[test]
    fn reader_rejects_schedule_gaps_instead_of_compacting() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let expected = course("c_gap", vec![ScheduleEntry::OnDemand; 3]);
        store.insert_course(&expected).unwrap();
        store
            .conn
            .execute(
                "UPDATE course_schedule SET position = 3 WHERE position = 2",
                [],
            )
            .unwrap();
        assert_eq!(
            store.get_course(&expected.id),
            Err(PersistenceError::InvalidStoredData)
        );
        store
            .conn
            .execute("DELETE FROM course_schedule WHERE position = 0", [])
            .unwrap();
        assert_eq!(
            store.get_course(&expected.id),
            Err(PersistenceError::InvalidStoredData)
        );
    }

    #[test]
    fn reader_rejects_schedule_corruption_when_checks_are_bypassed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let expected = course("c_corrupt_schedule", vec![ScheduleEntry::OnDemand]);
        store.insert_course(&expected).unwrap();
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        for (kind, day, period) in [
            (PRIVATE, None, None),
            ("on_demand", Some("mon"), None),
            ("timetabled", Some(PRIVATE), Some(1)),
            ("timetabled", Some("mon"), Some(256)),
        ] {
            store
                .conn
                .execute(
                    "UPDATE course_schedule SET entry_kind = ?1, day = ?2, period = ?3",
                    params![kind, day, period],
                )
                .unwrap();
            assert_eq!(
                store.get_course(&expected.id),
                Err(PersistenceError::InvalidStoredData)
            );
        }
    }

    #[test]
    fn assignment_source_keys_are_unique_and_each_assignment_has_at_most_one_binding() {
        let store = SqliteStore::open_in_memory().unwrap();
        for id in ["a1", "a2", "a3", "a4", "a5"] {
            store.insert_assignment(&assignment(id)).unwrap();
        }
        let sql = "INSERT INTO assignment_sources (assignment_id, origin, scope, external_id) VALUES (?1, ?2, ?3, ?4)";
        store
            .conn
            .execute(
                sql,
                params![
                    "a1",
                    encode_origin(OriginKind::WebClass),
                    " group-01 ",
                    "0001"
                ],
            )
            .unwrap();
        assert_constraint(
            store
                .conn
                .execute(sql, params!["a2", "webclass", " group-01 ", "0001"]),
            ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        assert_constraint(
            store
                .conn
                .execute(sql, params!["a1", "manual", "other", "other"]),
            ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
        );
        // Complete keys are binary/opaque; changing case, scope or origin is distinct.
        store
            .conn
            .execute(sql, params!["a2", "webclass", "group-01", "0001"])
            .unwrap();
        store
            .conn
            .execute(sql, params!["a3", "webclass", " GROUP-01 ", "0001"])
            .unwrap();
        store
            .conn
            .execute(sql, params!["a4", "moodle", " group-01 ", "0001"])
            .unwrap();
        store
            .conn
            .execute(sql, params!["a5", "webclass", " group-01 ", "001"])
            .unwrap();
        assert_eq!(count(&store.conn, "assignment_sources"), 5);
    }

    #[test]
    fn course_source_keys_are_unique_and_each_course_has_at_most_one_binding() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for id in ["c1", "c2", "c3", "c4"] {
            store.insert_course(&course(id, vec![])).unwrap();
        }
        let sql = "INSERT INTO course_sources (course_id, origin, external_id) VALUES (?1, ?2, ?3)";
        store
            .conn
            .execute(sql, params!["c1", "webclass", " Group-01 "])
            .unwrap();
        assert_constraint(
            store
                .conn
                .execute(sql, params!["c2", "webclass", " Group-01 "]),
            ffi::SQLITE_CONSTRAINT_UNIQUE,
        );
        assert_constraint(
            store.conn.execute(sql, params!["c1", "manual", "other"]),
            ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
        );
        store
            .conn
            .execute(sql, params!["c2", "webclass", " group-01 "])
            .unwrap();
        store
            .conn
            .execute(sql, params!["c3", "webclass", "Group-01"])
            .unwrap();
        store
            .conn
            .execute(sql, params!["c4", "moodle", " Group-01 "])
            .unwrap();
        assert_eq!(count(&store.conn, "course_sources"), 4);
    }

    #[test]
    fn all_child_tables_enforce_foreign_keys() {
        let store = SqliteStore::open_in_memory().unwrap();
        for sql in [
            "INSERT INTO assignment_sources VALUES ('missing', 'manual', 'scope', 'id')",
            "INSERT INTO course_sources VALUES ('missing', 'manual', 'id')",
            "INSERT INTO course_schedule VALUES ('missing', 0, 'on_demand', NULL, NULL)",
            "INSERT INTO assignments VALUES ('a', 'missing', '', NULL, NULL, 'unknown', NULL, 'incomplete', '2026-10-04T00:00:00.000000000Z', '2026-10-04T00:00:00.000000000Z')",
        ] {
            assert_constraint(
                store.conn.execute(sql, []),
                ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            );
        }
    }

    #[test]
    fn explicit_parent_deletion_enforces_restrict_and_cascade() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let related = course(
            "c_delete",
            vec![ScheduleEntry::OnDemand, slot(DayOfWeek::Sunday, 255)],
        );
        store.insert_course(&related).unwrap();
        let mut expected = assignment("a_delete");
        expected.course_id = Some(related.id.clone());
        store.insert_assignment(&expected).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO assignment_sources VALUES (?1, 'manual', 'scope', 'id')",
                [expected.id.as_str()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO course_sources VALUES (?1, 'webclass', 'group')",
                [related.id.as_str()],
            )
            .unwrap();
        assert_constraint(
            store
                .conn
                .execute("DELETE FROM courses WHERE id = ?1", [related.id.as_str()]),
            ffi::SQLITE_CONSTRAINT_TRIGGER,
        );
        assert_eq!(count(&store.conn, "courses"), 1);
        assert_eq!(count(&store.conn, "course_schedule"), 2);
        assert_eq!(count(&store.conn, "course_sources"), 1);
        store
            .conn
            .execute(
                "DELETE FROM assignments WHERE id = ?1",
                [expected.id.as_str()],
            )
            .unwrap();
        assert_eq!(count(&store.conn, "assignment_sources"), 0);
        store
            .conn
            .execute("DELETE FROM courses WHERE id = ?1", [related.id.as_str()])
            .unwrap();
        assert_eq!(count(&store.conn, "course_sources"), 0);
        assert_eq!(count(&store.conn, "course_schedule"), 0);
    }

    #[test]
    fn check_constraints_reject_invalid_enum_codes_and_deadline_shapes() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.insert_assignment(&assignment("a_check")).unwrap();
        store
            .insert_course(&course("c_check", vec![ScheduleEntry::OnDemand]))
            .unwrap();
        store.conn.execute_batch("INSERT INTO assignment_sources VALUES ('a_check', 'manual', 'scope', 'id'); INSERT INTO course_sources VALUES ('c_check', 'manual', 'id');").unwrap();
        for sql in [
            "UPDATE assignments SET submission = ?1",
            "UPDATE assignments SET completion = ?1",
            "UPDATE assignment_sources SET origin = ?1",
            "UPDATE course_sources SET origin = ?1",
            "UPDATE course_schedule SET entry_kind = ?1",
            "UPDATE course_schedule SET day = ?1",
        ] {
            assert_constraint(
                store.conn.execute(sql, [PRIVATE]),
                ffi::SQLITE_CONSTRAINT_CHECK,
            );
        }
        for (kind, value) in [
            (Some("invalid"), Some("value")),
            (None, Some(STAMP)),
            (Some("date_only"), None),
            (Some("date_only"), Some("")),
            (Some("date_time"), Some("2026-10-04T14:59:00+00:00")),
        ] {
            assert_constraint(
                store.conn.execute(
                    "UPDATE assignments SET deadline_kind = ?1, deadline_value = ?2",
                    params![kind, value],
                ),
                ffi::SQLITE_CONSTRAINT_CHECK,
            );
        }
        for sql in [
            "UPDATE assignment_sources SET scope = ''",
            "UPDATE assignment_sources SET external_id = ''",
            "UPDATE course_sources SET external_id = ''",
            "UPDATE course_schedule SET position = -1",
            "UPDATE course_schedule SET entry_kind = 'timetabled'",
            "UPDATE course_schedule SET period = 0",
            "UPDATE course_schedule SET entry_kind = 'timetabled', day = 'mon', period = -1",
            "UPDATE course_schedule SET entry_kind = 'timetabled', day = 'mon', period = 256",
            "UPDATE assignments SET created_at = 'no UTC suffix'",
            "UPDATE courses SET updated_at = 'no UTC suffix'",
        ] {
            assert_constraint(store.conn.execute(sql, []), ffi::SQLITE_CONSTRAINT_CHECK);
        }
        assert_constraint(
            store
                .conn
                .execute("UPDATE course_schedule SET position = 'not an integer'", []),
            ffi::SQLITE_CONSTRAINT_DATATYPE,
        );
    }

    #[test]
    fn public_errors_have_fixed_display_debug_and_no_source() {
        for (error, debug, display) in [
            (
                PersistenceError::Open,
                "Open",
                "could not open SQLite database",
            ),
            (
                PersistenceError::Configure,
                "Configure",
                "could not configure SQLite connection",
            ),
            (
                PersistenceError::Migration,
                "Migration",
                "could not initialize SQLite schema",
            ),
            (
                PersistenceError::UnsupportedSchemaVersion,
                "UnsupportedSchemaVersion",
                "unsupported SQLite schema version",
            ),
            (
                PersistenceError::Database,
                "Database",
                "SQLite operation failed",
            ),
            (
                PersistenceError::InvalidStoredData,
                "InvalidStoredData",
                "invalid stored domain data",
            ),
            (
                PersistenceError::Constraint,
                "Constraint",
                "SQLite constraint violation",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.to_string(), display);
            assert_safe(error);
        }
        let raw = rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CONSTRAINT_CHECK),
            Some(PRIVATE.into()),
        );
        assert_safe(database_error(raw));
        assert_safe(database_error(rusqlite::Error::InvalidPath(PathBuf::from(
            PRIVATE,
        ))));
        assert_safe(database_error(rusqlite::Error::IntegralValueOutOfRange(
            0,
            i64::MAX,
        )));
    }

    #[test]
    fn path_sql_parameter_and_trigger_payloads_do_not_escape_error_boundary() {
        let temp = TempDb::new();
        let error = SqliteStore::open(temp.path.join(PRIVATE)).err().unwrap();
        assert_eq!(error, PersistenceError::Open);
        assert_safe(error);
        let mut store = SqliteStore::open_in_memory().unwrap();
        // Even SQLite messages explicitly containing row data must be discarded.
        store.conn.execute_batch("CREATE TRIGGER reject_assignment BEFORE INSERT ON assignments BEGIN SELECT RAISE(ABORT, NEW.title || NEW.submission_url); END;").unwrap();
        let mut private_assignment = assignment(PRIVATE);
        private_assignment.title = PRIVATE.into();
        private_assignment.submission_url =
            Some(format!("https://submit.example.invalid/{PRIVATE}"));
        let error = store.insert_assignment(&private_assignment).unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        store.conn.execute_batch("CREATE TRIGGER reject_course BEFORE INSERT ON courses BEGIN SELECT RAISE(ABORT, NEW.name); END;").unwrap();
        let mut private_course = course(PRIVATE, vec![]);
        private_course.name = PRIVATE.into();
        assert_safe(store.insert_course(&private_course).unwrap_err());
        store.conn.execute_batch("DROP TABLE assignments").unwrap();
        let error = store.get_assignment(&private_assignment.id).unwrap_err();
        assert_eq!(error, PersistenceError::Database);
        assert_safe(error);
    }
}
