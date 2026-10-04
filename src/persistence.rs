//! SQLite schema v1, entity creation/binding, validated import and local completion.
//!
//! Bindings accept non-secret logical identifiers only. Authentication values and
//! feed URLs must never be passed as identities. Import batches are not source
//! snapshots: absent candidates never trigger deletion or presence updates.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior, ffi, params};

use crate::domain::{
    Assignment, AssignmentId, ClassSlot, CompletionState, Course, CourseId, DayOfWeek, Deadline,
    ScheduleEntry, SubmissionTarget,
};
use crate::import::{AssignmentCandidateBatch, ExternalRecordKey, OriginKind};

const SCHEMA_VERSION: i64 = 1;
const ID_INSERT_ATTEMPTS: usize = 4;
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
    IdGenerationFailed,
    InvalidExternalIdentity,
    EntityAlreadyBound,
    ExternalIdentityAlreadyBound,
    NotFound,
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
            Self::IdGenerationFailed => "could not allocate an internal ID",
            Self::InvalidExternalIdentity => "external identity is empty or whitespace-only",
            Self::EntityAlreadyBound => "entity already has a source binding",
            Self::ExternalIdentityAlreadyBound => "external identity already has a binding",
            Self::NotFound => "binding target does not exist",
        })
    }
}

impl Error for PersistenceError {}

/// Fields for a normal hand-entered assignment. Completion starts as `Incomplete`;
/// no external identity or caller-supplied internal ID is accepted.
pub struct ManualAssignmentInput {
    pub title: String,
    pub deadline: Option<Deadline>,
    pub course_id: Option<CourseId>,
    pub submission: SubmissionTarget,
    pub submission_url: Option<String>,
}

/// Domain outcomes from a committed batch, without entity data or identities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssignmentImportSummary {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
}

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
        get_assignment_tx(&self.conn, id)
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
        let course = stored.map(|row| row.decode(&tx)).transpose()?;
        tx.commit().map_err(database_error)?;
        Ok(course)
    }

    /// Create a hand-entered assignment with a generated ID and no source binding.
    /// A supplied course must already exist; completion always starts as `Incomplete`.
    pub fn create_manual_assignment(
        &mut self,
        input: ManualAssignmentInput,
    ) -> Result<Assignment, PersistenceError> {
        self.create_assignment_inner(input, None)
    }

    // Optional binding is an internal atomic operation, not a candidate import API.
    fn create_assignment_inner(
        &mut self,
        input: ManualAssignmentInput,
        source: Option<&ExternalRecordKey>,
    ) -> Result<Assignment, PersistenceError> {
        if let Some(key) = source {
            validate_external_key(key)?;
        }
        let tx = self.conn.transaction().map_err(database_error)?;
        let timestamp = encode_timestamp(&Utc::now());
        let mut assignment = Assignment {
            id: AssignmentId::new(""),
            course_id: input.course_id,
            title: input.title,
            deadline: input.deadline,
            submission: input.submission,
            submission_url: input.submission_url,
            completion: CompletionState::Incomplete,
        };
        insert_new_assignment_tx(&tx, &mut assignment, source, &timestamp)?;
        tx.commit().map_err(database_error)?;
        Ok(assignment)
    }

    /// Create a course with a generated ID and an ordered schedule, without binding.
    pub fn create_course(
        &mut self,
        name: impl Into<String>,
        schedule: Vec<ScheduleEntry>,
    ) -> Result<Course, PersistenceError> {
        self.create_course_inner(name.into(), schedule, None)
    }

    /// Atomically create a course, schedule and source binding. `external_id` must
    /// be a non-secret logical course ID (WebClass uses its opaque `group_id`).
    /// Moodle feed labels are assignment scopes, not course identifiers.
    pub fn create_course_with_source(
        &mut self,
        name: impl Into<String>,
        schedule: Vec<ScheduleEntry>,
        origin: OriginKind,
        external_id: &str,
    ) -> Result<Course, PersistenceError> {
        self.create_course_inner(name.into(), schedule, Some((origin, external_id)))
    }

    fn create_course_inner(
        &mut self,
        name: String,
        schedule: Vec<ScheduleEntry>,
        source: Option<(OriginKind, &str)>,
    ) -> Result<Course, PersistenceError> {
        if let Some((_, external_id)) = source {
            validate_external_identity(external_id)?;
        }
        let tx = self.conn.transaction().map_err(database_error)?;
        let timestamp = encode_timestamp(&Utc::now());
        let mut course = Course {
            id: CourseId::new(""),
            name,
            schedule,
        };
        // Only the entity PK insert is retried, never schedule or binding errors.
        insert_with_generated_id(
            || generate_internal_id(&tx, "c_"),
            |id| {
                course.id = CourseId::new(id);
                insert_course_row(&tx, &course, &timestamp)
            },
        )?;
        insert_course_schedule(&tx, &course)?;
        if let Some((origin, external_id)) = source {
            insert_course_binding(&tx, &course.id, origin, external_id)?;
        }
        tx.commit().map_err(database_error)?;
        Ok(course)
    }

    /// Bind the exact existing assignment chosen by the caller. A second binding
    /// or an already-used complete key is rejected, including identical rebinds.
    /// Moodle keys use a non-secret feed label as scope and VEVENT UID as ID;
    /// never supply a feed/token URL, cookie, credential or authentication payload.
    pub fn bind_assignment_source(
        &mut self,
        id: &AssignmentId,
        key: &ExternalRecordKey,
    ) -> Result<(), PersistenceError> {
        validate_external_key(key)?;
        let tx = self.conn.transaction().map_err(database_error)?;
        insert_assignment_binding(&tx, id, key)?;
        tx.commit().map_err(database_error)
    }

    /// Find an assignment by its complete, case-sensitive, unnormalized key.
    /// Absence is `None`; multiple matches are invalid stored data.
    pub fn find_assignment_by_external_key(
        &self,
        key: &ExternalRecordKey,
    ) -> Result<Option<Assignment>, PersistenceError> {
        validate_external_key(key)?;
        find_assignment_by_external_key_tx(&self.conn, key)
    }

    /// Bind an explicitly selected existing course to a non-secret logical ID.
    /// No matching by name or implicit course creation is performed.
    pub fn bind_course_source(
        &mut self,
        id: &CourseId,
        origin: OriginKind,
        external_id: &str,
    ) -> Result<(), PersistenceError> {
        validate_external_identity(external_id)?;
        let tx = self.conn.transaction().map_err(database_error)?;
        insert_course_binding(&tx, id, origin, external_id)?;
        tx.commit().map_err(database_error)
    }

    /// Find a course and schedule by exact origin and non-secret external course ID.
    pub fn find_course_by_source(
        &self,
        origin: OriginKind,
        external_id: &str,
    ) -> Result<Option<Course>, PersistenceError> {
        validate_external_identity(external_id)?;
        let tx = self.conn.unchecked_transaction().map_err(database_error)?;
        let course = find_course_by_source_tx(&tx, origin, external_id)?;
        tx.commit().map_err(database_error)?;
        Ok(course)
    }

    /// Resolve only an already-bound WebClass group to its internal course ID.
    pub fn find_webclass_course_id(
        &self,
        group_id: &str,
    ) -> Result<Option<CourseId>, PersistenceError> {
        Ok(self
            .find_course_by_source(OriginKind::WebClass, group_id)?
            .map(|course| course.id))
    }

    /// Atomically persist a validated batch using its complete external keys.
    /// WebClass scopes resolve through existing group/course bindings; Moodle feed
    /// labels and manual scopes do not imply courses. Unresolved existing courses
    /// are retained by the domain merge helper. Absent candidates are untouched.
    /// The summary is returned only after the whole batch commits.
    ///
    /// Unchecked candidate vectors do not cross this boundary:
    /// ```compile_fail
    /// use assignment_manager::import::AssignmentCandidate;
    /// use assignment_manager::persistence::SqliteStore;
    /// let mut store = SqliteStore::open(":memory:").unwrap();
    /// let unchecked = Vec::<AssignmentCandidate>::new();
    /// store.import_assignment_batch(&unchecked);
    /// ```
    pub fn import_assignment_batch(
        &mut self,
        batch: &AssignmentCandidateBatch,
    ) -> Result<AssignmentImportSummary, PersistenceError> {
        if batch.as_slice().is_empty() {
            return Ok(AssignmentImportSummary::default());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let timestamp = encode_timestamp(&Utc::now());
        let mut summary = AssignmentImportSummary::default();
        for candidate in batch.as_slice() {
            let resolved_course_id = match candidate.external_key.origin {
                OriginKind::WebClass => {
                    find_webclass_course_id_tx(&tx, &candidate.external_key.scope)?
                }
                OriginKind::Moodle | OriginKind::Manual => None,
            };
            if let Some(mut assignment) =
                find_assignment_by_external_key_tx(&tx, &candidate.external_key)?
            {
                let before = assignment.clone();
                candidate.apply_source_update(&mut assignment, resolved_course_id);
                if assignment == before {
                    summary.unchanged += 1;
                } else {
                    update_assignment_source_fields_tx(&tx, &assignment, &timestamp)?;
                    summary.updated += 1;
                }
            } else {
                let mut assignment = candidate
                    .clone()
                    .into_assignment(AssignmentId::new(""), resolved_course_id);
                insert_new_assignment_tx(
                    &tx,
                    &mut assignment,
                    Some(&candidate.external_key),
                    &timestamp,
                )?;
                summary.created += 1;
            }
        }
        tx.commit().map_err(database_error)?;
        Ok(summary)
    }

    /// Change local completion independently of source import. Identical state is
    /// a no-op, including `updated_at`; a missing assignment returns `NotFound`.
    pub fn set_completion(
        &mut self,
        id: &AssignmentId,
        completion: CompletionState,
    ) -> Result<(), PersistenceError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let assignment = get_assignment_tx(&tx, id)?.ok_or(PersistenceError::NotFound)?;
        if assignment.completion != completion {
            let timestamp = encode_timestamp(&Utc::now());
            let changed = tx
                .execute(
                    "UPDATE assignments SET completion = ?1, updated_at = ?2 WHERE id = ?3",
                    params![encode_completion(completion), timestamp, id.as_str()],
                )
                .map_err(database_error)?;
            if changed != 1 {
                return Err(PersistenceError::InvalidStoredData);
            }
        }
        tx.commit().map_err(database_error)
    }
}

// These helpers reuse the caller's connection/transaction without starting one.
fn get_assignment_tx(
    conn: &Connection,
    id: &AssignmentId,
) -> Result<Option<Assignment>, PersistenceError> {
    let stored = conn
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

fn find_assignment_by_external_key_tx(
    conn: &Connection,
    key: &ExternalRecordKey,
) -> Result<Option<Assignment>, PersistenceError> {
    // LEFT JOIN exposes orphan bindings as invalid NULL domain columns rather
    // than treating the occupied key as a new assignment. query_one rejects duplicates.
    let stored = conn
        .query_one(
            "SELECT a.id, a.course_id, a.title, a.deadline_kind, a.deadline_value,
                a.submission, a.submission_url, a.completion, a.created_at, a.updated_at
         FROM assignment_sources s LEFT JOIN assignments a ON a.id = s.assignment_id
         WHERE s.origin = ?1 AND s.scope = ?2 AND s.external_id = ?3",
            params![encode_origin(key.origin), key.scope, key.id],
            StoredAssignment::from_row,
        )
        .optional()
        .map_err(database_error)?;
    stored.map(StoredAssignment::decode).transpose()
}

fn find_course_by_source_tx(
    conn: &Connection,
    origin: OriginKind,
    external_id: &str,
) -> Result<Option<Course>, PersistenceError> {
    let stored = conn
        .query_one(
            "SELECT c.id, c.name, c.created_at, c.updated_at
         FROM course_sources s LEFT JOIN courses c ON c.id = s.course_id
         WHERE s.origin = ?1 AND s.external_id = ?2",
            params![encode_origin(origin), external_id],
            StoredCourse::from_row,
        )
        .optional()
        .map_err(database_error)?;
    stored.map(|row| row.decode(conn)).transpose()
}

fn find_webclass_course_id_tx(
    conn: &Connection,
    group_id: &str,
) -> Result<Option<CourseId>, PersistenceError> {
    Ok(find_course_by_source_tx(conn, OriginKind::WebClass, group_id)?.map(|course| course.id))
}

fn insert_new_assignment_tx(
    conn: &Connection,
    assignment: &mut Assignment,
    source: Option<&ExternalRecordKey>,
    timestamp: &str,
) -> Result<(), PersistenceError> {
    insert_with_generated_id(
        || generate_internal_id(conn, "a_"),
        |id| {
            assignment.id = AssignmentId::new(id);
            insert_assignment_row(conn, assignment, timestamp)
        },
    )?;
    if let Some(key) = source {
        insert_assignment_binding(conn, &assignment.id, key)?;
    }
    Ok(())
}

fn update_assignment_source_fields_tx(
    conn: &Connection,
    assignment: &Assignment,
    timestamp: &str,
) -> Result<(), PersistenceError> {
    let (kind, value) = encode_deadline(assignment.deadline.as_ref());
    // Values are already merged by AssignmentCandidate::apply_source_update.
    // Local completion, created_at and the internal ID are never assigned here.
    let changed = conn
        .execute(
            "UPDATE assignments SET course_id = ?1, title = ?2, deadline_kind = ?3,
                deadline_value = ?4, submission = ?5, submission_url = ?6, updated_at = ?7
         WHERE id = ?8",
            params![
                assignment.course_id.as_ref().map(CourseId::as_str),
                assignment.title,
                kind,
                value,
                encode_submission(assignment.submission),
                assignment.submission_url,
                timestamp,
                assignment.id.as_str()
            ],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(PersistenceError::InvalidStoredData);
    }
    Ok(())
}

fn generate_internal_id(conn: &Connection, prefix: &str) -> rusqlite::Result<String> {
    conn.query_one("SELECT ?1 || lower(hex(randomblob(16)))", [prefix], |row| {
        row.get(0)
    })
}

fn insert_with_generated_id(
    mut generate: impl FnMut() -> rusqlite::Result<String>,
    mut insert: impl FnMut(&str) -> rusqlite::Result<()>,
) -> Result<(), PersistenceError> {
    for _ in 0..ID_INSERT_ATTEMPTS {
        let id = generate().map_err(|_| PersistenceError::IdGenerationFailed)?;
        match insert(&id) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.sqlite_error().is_some_and(|error| {
                    error.extended_code == ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                }) =>
            {
                continue;
            }
            Err(error) => return Err(database_error(error)),
        }
    }
    Err(PersistenceError::IdGenerationFailed)
}

fn insert_assignment_row(
    conn: &Connection,
    assignment: &Assignment,
    timestamp: &str,
) -> rusqlite::Result<()> {
    let (kind, value) = encode_deadline(assignment.deadline.as_ref());
    conn.execute(
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
            timestamp
        ],
    )
    .map(|_| ())
}

fn insert_course_row(conn: &Connection, course: &Course, timestamp: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO courses (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![course.id.as_str(), course.name, timestamp],
    )
    .map(|_| ())
}

fn insert_course_schedule(conn: &Connection, course: &Course) -> Result<(), PersistenceError> {
    for (position, entry) in course.schedule.iter().enumerate() {
        let position = i64::try_from(position).map_err(|_| PersistenceError::Constraint)?;
        let (kind, day, period) = encode_schedule(*entry);
        conn.execute(
            "INSERT INTO course_schedule (course_id, position, entry_kind, day, period)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![course.id.as_str(), position, kind, day, period],
        )
        .map_err(database_error)?;
    }
    Ok(())
}

fn validate_external_identity(value: &str) -> Result<(), PersistenceError> {
    if value.trim().is_empty() {
        Err(PersistenceError::InvalidExternalIdentity)
    } else {
        Ok(())
    }
}

fn validate_external_key(key: &ExternalRecordKey) -> Result<(), PersistenceError> {
    validate_external_identity(&key.scope)?;
    validate_external_identity(&key.id)
}

fn insert_assignment_binding(
    conn: &Connection,
    id: &AssignmentId,
    key: &ExternalRecordKey,
) -> Result<(), PersistenceError> {
    conn.execute(
        "INSERT INTO assignment_sources (assignment_id, origin, scope, external_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![id.as_str(), encode_origin(key.origin), key.scope, key.id],
    )
    .map_err(binding_error)?;
    Ok(())
}

fn insert_course_binding(
    conn: &Connection,
    id: &CourseId,
    origin: OriginKind,
    external_id: &str,
) -> Result<(), PersistenceError> {
    conn.execute(
        "INSERT INTO course_sources (course_id, origin, external_id) VALUES (?1, ?2, ?3)",
        params![id.as_str(), encode_origin(origin), external_id],
    )
    .map_err(binding_error)?;
    Ok(())
}

fn binding_error(error: rusqlite::Error) -> PersistenceError {
    match error.sqlite_error().map(|error| error.extended_code) {
        Some(ffi::SQLITE_CONSTRAINT_PRIMARYKEY) => PersistenceError::EntityAlreadyBound,
        Some(ffi::SQLITE_CONSTRAINT_UNIQUE) => PersistenceError::ExternalIdentityAlreadyBound,
        Some(ffi::SQLITE_CONSTRAINT_FOREIGNKEY) => PersistenceError::NotFound,
        _ => database_error(error),
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
        | rusqlite::Error::Utf8Error(..)
        | rusqlite::Error::QueryReturnedMoreThanOneRow => PersistenceError::InvalidStoredData,
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

    fn decode(self, conn: &Connection) -> Result<Course, PersistenceError> {
        validate_metadata(&self.created_at, &self.updated_at)?;
        let mut schedule = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT position, entry_kind, day, period
             FROM course_schedule WHERE course_id = ?1 ORDER BY position",
            )
            .map_err(database_error)?;
        let mut rows = stmt.query([&self.id]).map_err(database_error)?;
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
        Ok(Course {
            id: CourseId::new(self.id),
            name: self.name,
            schedule,
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

fn encode_schedule(entry: ScheduleEntry) -> (&'static str, Option<&'static str>, Option<u8>) {
    match entry {
        ScheduleEntry::OnDemand => ("on_demand", None, None),
        ScheduleEntry::Timetabled(slot) => {
            ("timetabled", Some(encode_day(slot.day)), Some(slot.period))
        }
    }
}

fn encode_origin(origin: OriginKind) -> &'static str {
    match origin {
        OriginKind::Moodle => "moodle",
        OriginKind::WebClass => "webclass",
        OriginKind::Manual => "manual",
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

    use super::*;
    use crate::import::AssignmentCandidate;

    const STAMP: &str = "2026-10-04T14:59:00.123456789Z";
    const PRIVATE: &str = "SYNTHETIC_PRIVATE_VALUE_6B1";

    // Test-only wrappers retain synthetic IDs for the Phase 6B1 tests, while
    // exercising the same private row writers used by production creation.
    impl SqliteStore {
        fn insert_assignment(&self, assignment: &Assignment) -> Result<(), PersistenceError> {
            let timestamp = encode_timestamp(&Utc::now());
            insert_assignment_row(&self.conn, assignment, &timestamp).map_err(database_error)
        }

        fn insert_course(&mut self, course: &Course) -> Result<(), PersistenceError> {
            let tx = self.conn.transaction().map_err(database_error)?;
            let timestamp = encode_timestamp(&Utc::now());
            insert_course_row(&tx, course, &timestamp).map_err(database_error)?;
            insert_course_schedule(&tx, course)?;
            tx.commit().map_err(database_error)
        }
    }

    // Binding readers filter by the production encoder; no public raw source reader.
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
            (
                PersistenceError::IdGenerationFailed,
                "IdGenerationFailed",
                "could not allocate an internal ID",
            ),
            (
                PersistenceError::InvalidExternalIdentity,
                "InvalidExternalIdentity",
                "external identity is empty or whitespace-only",
            ),
            (
                PersistenceError::EntityAlreadyBound,
                "EntityAlreadyBound",
                "entity already has a source binding",
            ),
            (
                PersistenceError::ExternalIdentityAlreadyBound,
                "ExternalIdentityAlreadyBound",
                "external identity already has a binding",
            ),
            (
                PersistenceError::NotFound,
                "NotFound",
                "binding target does not exist",
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

    fn manual_input() -> ManualAssignmentInput {
        ManualAssignmentInput {
            title: "Hand-entered report 日本語 ' ?1".into(),
            deadline: Some(Deadline::DateTime(decode_timestamp(STAMP).unwrap())),
            course_id: None,
            submission: SubmissionTarget::External,
            submission_url: Some("https://submit.example.invalid/manual".into()),
        }
    }

    fn external_key(origin: OriginKind, scope: &str, id: &str) -> ExternalRecordKey {
        ExternalRecordKey {
            origin,
            scope: scope.into(),
            id: id.into(),
        }
    }

    fn assert_generated_id(id: &str, prefix: &str) {
        let hex = id.strip_prefix(prefix).unwrap();
        assert_eq!(hex.len(), 32);
        assert!(
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn production_manual_creation_generates_distinct_ids_without_fake_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..16 {
            let created_course = store.create_course("", vec![]).unwrap();
            assert_generated_id(created_course.id.as_str(), "c_");
            assert!(ids.insert(created_course.id.as_str().to_owned()));
            assert_eq!(
                store.get_course(&created_course.id).unwrap(),
                Some(created_course.clone())
            );
            let mut input = manual_input();
            input.course_id = Some(created_course.id);
            let created = store.create_manual_assignment(input).unwrap();
            assert_generated_id(created.id.as_str(), "a_");
            assert!(ids.insert(created.id.as_str().to_owned()));
            assert_eq!(created.completion, CompletionState::Incomplete);
            assert_eq!(created.title, manual_input().title);
            assert_eq!(created.deadline, manual_input().deadline);
            assert_eq!(created.submission, SubmissionTarget::External);
            assert_eq!(created.submission_url, manual_input().submission_url);
            assert_eq!(store.get_assignment(&created.id).unwrap(), Some(created));
        }
        assert_eq!(count(&store.conn, "assignments"), 16);
        assert_eq!(count(&store.conn, "courses"), 16);
        assert_eq!(count(&store.conn, "assignment_sources"), 0);
        assert_eq!(count(&store.conn, "course_sources"), 0);
    }

    #[test]
    fn production_creation_and_bindings_preserve_metadata_and_survive_reopen() {
        let temp = TempDb::new();
        let created_course;
        let manual;
        let bound;
        let key = external_key(OriginKind::Moodle, "cis-feed", "Opaque-0001@calendar");
        let group_id = " Group-0001 ";
        let timestamps;
        {
            let mut store = SqliteStore::open(&temp.path).unwrap();
            let before = Utc::now();
            created_course = store
                .create_course_with_source(
                    "Course",
                    vec![
                        slot(DayOfWeek::Sunday, 255),
                        ScheduleEntry::OnDemand,
                        slot(DayOfWeek::Monday, 0),
                        slot(DayOfWeek::Monday, 0),
                    ],
                    OriginKind::WebClass,
                    group_id,
                )
                .unwrap();
            let mut input = manual_input();
            input.course_id = Some(created_course.id.clone());
            manual = store.create_manual_assignment(input).unwrap();
            let mut input = manual_input();
            input.deadline = None;
            input.submission = SubmissionTarget::None;
            input.submission_url = None;
            bound = store.create_manual_assignment(input).unwrap();
            assert_eq!(count(&store.conn, "assignment_sources"), 0);
            let after = Utc::now();
            timestamps = [
                store
                    .conn
                    .query_row(
                        "SELECT created_at, updated_at FROM courses WHERE id = ?1",
                        [created_course.id.as_str()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .unwrap(),
                store
                    .conn
                    .query_row(
                        "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                        [manual.id.as_str()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .unwrap(),
                store
                    .conn
                    .query_row(
                        "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                        [bound.id.as_str()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .unwrap(),
            ];
            for (created, updated) in &timestamps {
                assert_eq!(created, updated);
                let instant = decode_timestamp(created).unwrap();
                assert!(before <= instant && instant <= after);
                assert_eq!(encode_timestamp(&instant), *created);
            }
            store.bind_assignment_source(&bound.id, &key).unwrap();
            // Binding changes only the source relation, not domain or metadata.
            assert_eq!(
                store.get_assignment(&bound.id).unwrap(),
                Some(bound.clone())
            );
        }
        let store = SqliteStore::open(&temp.path).unwrap();
        assert_eq!(
            store.get_assignment(&manual.id).unwrap(),
            Some(manual.clone())
        );
        assert_eq!(manual.completion, CompletionState::Incomplete);
        assert_eq!(
            store.get_assignment(&bound.id).unwrap(),
            Some(bound.clone())
        );
        assert_eq!(
            store.find_assignment_by_external_key(&key).unwrap(),
            Some(bound.clone())
        );
        assert_eq!(
            store
                .find_course_by_source(OriginKind::WebClass, group_id)
                .unwrap(),
            Some(created_course.clone())
        );
        assert_eq!(
            store.find_webclass_course_id(group_id).unwrap(),
            Some(created_course.id.clone())
        );
        let restored = [
            store
                .conn
                .query_row(
                    "SELECT created_at, updated_at FROM courses WHERE id = ?1",
                    [created_course.id.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            store
                .conn
                .query_row(
                    "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                    [manual.id.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            store
                .conn
                .query_row(
                    "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                    [bound.id.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
        ];
        assert_eq!(restored, timestamps);
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
    }

    #[test]
    fn assignment_binding_and_lookup_use_complete_opaque_keys() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for key in [
            external_key(OriginKind::WebClass, " Group-01 ", " AbC0001 "),
            external_key(OriginKind::Moodle, " Group-01 ", " AbC0001 "),
            external_key(OriginKind::Manual, " Group-01 ", " AbC0001 "),
            external_key(OriginKind::WebClass, " group-01 ", " AbC0001 "),
            external_key(OriginKind::WebClass, " Group-01 ", " abc0001 "),
            external_key(OriginKind::WebClass, "Group-01", " AbC0001 "),
            external_key(OriginKind::WebClass, " Group-01 ", "AbC0001"),
        ] {
            assert_eq!(store.find_assignment_by_external_key(&key).unwrap(), None);
            let created = store.create_manual_assignment(manual_input()).unwrap();
            store.bind_assignment_source(&created.id, &key).unwrap();
            assert_eq!(
                store.find_assignment_by_external_key(&key).unwrap(),
                Some(created.clone())
            );
            let raw: (String, String, String) = store.conn.query_row(
                "SELECT origin, scope, external_id FROM assignment_sources WHERE assignment_id = ?1",
                [created.id.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(raw, (encode_origin(key.origin).into(), key.scope, key.id));
        }
        assert_eq!(count(&store.conn, "assignments"), 7);
        assert_eq!(count(&store.conn, "assignment_sources"), 7);
        assert_eq!(
            store
                .find_assignment_by_external_key(&external_key(
                    OriginKind::WebClass,
                    "unknown",
                    "unknown"
                ))
                .unwrap(),
            None
        );
    }

    #[test]
    fn assignment_binding_conflicts_have_distinct_safe_errors_and_preserve_entities() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first = store.create_manual_assignment(manual_input()).unwrap();
        let second = store.create_manual_assignment(manual_input()).unwrap();
        let key = external_key(OriginKind::Manual, "manual-logical-source", "0001");
        store.bind_assignment_source(&first.id, &key).unwrap();
        assert_eq!(
            store.bind_assignment_source(&second.id, &key),
            Err(PersistenceError::ExternalIdentityAlreadyBound)
        );
        assert_eq!(
            store.bind_assignment_source(
                &first.id,
                &external_key(OriginKind::Manual, "other", "other")
            ),
            Err(PersistenceError::EntityAlreadyBound)
        );
        assert!(store.bind_assignment_source(&first.id, &key).is_err());
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
        assert_eq!(store.get_assignment(&first.id).unwrap(), Some(first));
        assert_eq!(store.get_assignment(&second.id).unwrap(), Some(second));
    }

    #[test]
    fn course_bindings_support_exact_origin_keys_and_explicit_late_binding() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let webclass = store
            .create_course_with_source(
                "Same name",
                vec![ScheduleEntry::OnDemand],
                OriginKind::WebClass,
                " Group-01 ",
            )
            .unwrap();
        let moodle = store
            .create_course_with_source("Same name", vec![], OriginKind::Moodle, " Group-01 ")
            .unwrap();
        assert_eq!(
            store
                .find_course_by_source(OriginKind::WebClass, " Group-01 ")
                .unwrap(),
            Some(webclass.clone())
        );
        assert_eq!(
            store
                .find_course_by_source(OriginKind::Moodle, " Group-01 ")
                .unwrap(),
            Some(moodle)
        );
        assert_eq!(
            store.find_webclass_course_id(" Group-01 ").unwrap(),
            Some(webclass.id.clone())
        );
        for distinct in [" group-01 ", "Group-01", " Group-1 "] {
            assert_eq!(store.find_webclass_course_id(distinct).unwrap(), None);
            let created = store
                .create_course_with_source("Same name", vec![], OriginKind::WebClass, distinct)
                .unwrap();
            assert_eq!(
                store.find_webclass_course_id(distinct).unwrap(),
                Some(created.id)
            );
        }
        let manual = store
            .create_course("Same name", vec![slot(DayOfWeek::Friday, 4)])
            .unwrap();
        assert_eq!(
            store
                .find_course_by_source(OriginKind::Manual, "manual-course")
                .unwrap(),
            None
        );
        store
            .bind_course_source(&manual.id, OriginKind::Manual, "manual-course")
            .unwrap();
        assert_eq!(
            store
                .find_course_by_source(OriginKind::Manual, "manual-course")
                .unwrap(),
            Some(manual.clone())
        );
        assert_eq!(
            store.bind_course_source(&manual.id, OriginKind::WebClass, "other"),
            Err(PersistenceError::EntityAlreadyBound)
        );
        let unbound = store.create_course("Same name", vec![]).unwrap();
        assert_eq!(
            store.bind_course_source(&unbound.id, OriginKind::WebClass, " Group-01 "),
            Err(PersistenceError::ExternalIdentityAlreadyBound)
        );
        assert_eq!(
            store
                .find_course_by_source(OriginKind::WebClass, "unknown")
                .unwrap(),
            None
        );
        assert_eq!(count(&store.conn, "course_sources"), 6);
        assert_eq!(store.get_course(&manual.id).unwrap(), Some(manual));
    }

    #[test]
    fn all_public_identity_boundaries_reject_blank_values_without_normalization() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let created = store.create_manual_assignment(manual_input()).unwrap();
        let created_course = store.create_course("Course", vec![]).unwrap();
        for blank in ["", " ", "\t\n", "\u{3000}", "\u{2003}"] {
            for key in [
                external_key(OriginKind::Manual, blank, "id"),
                external_key(OriginKind::Manual, "scope", blank),
            ] {
                assert_eq!(
                    store.bind_assignment_source(&created.id, &key),
                    Err(PersistenceError::InvalidExternalIdentity)
                );
                assert_eq!(
                    store.find_assignment_by_external_key(&key),
                    Err(PersistenceError::InvalidExternalIdentity)
                );
                assert_eq!(
                    store.create_assignment_inner(manual_input(), Some(&key)),
                    Err(PersistenceError::InvalidExternalIdentity)
                );
            }
            assert_eq!(
                store.bind_course_source(&created_course.id, OriginKind::WebClass, blank),
                Err(PersistenceError::InvalidExternalIdentity)
            );
            assert_eq!(
                store.find_course_by_source(OriginKind::WebClass, blank),
                Err(PersistenceError::InvalidExternalIdentity)
            );
            assert_eq!(
                store.find_webclass_course_id(blank),
                Err(PersistenceError::InvalidExternalIdentity)
            );
            assert_eq!(
                store.create_course_with_source(
                    "Course",
                    vec![ScheduleEntry::OnDemand],
                    OriginKind::WebClass,
                    blank
                ),
                Err(PersistenceError::InvalidExternalIdentity)
            );
        }
        assert_eq!(count(&store.conn, "assignments"), 1);
        assert_eq!(count(&store.conn, "courses"), 1);
        assert_eq!(count(&store.conn, "assignment_sources"), 0);
        assert_eq!(count(&store.conn, "course_sources"), 0);
        assert_eq!(count(&store.conn, "course_schedule"), 0);
    }

    #[test]
    fn binding_missing_entities_is_rejected_without_implicit_creation() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let key = external_key(OriginKind::Manual, "scope", "id");
        assert_eq!(
            store.bind_assignment_source(&AssignmentId::new("absent"), &key),
            Err(PersistenceError::NotFound)
        );
        assert_eq!(
            store.bind_course_source(&CourseId::new("absent"), OriginKind::WebClass, "group"),
            Err(PersistenceError::NotFound)
        );
        for table in TABLES {
            assert_eq!(count(&store.conn, table), 0);
        }
        assert!(store.conn.is_autocommit());
        let created = store.create_manual_assignment(manual_input()).unwrap();
        store.bind_assignment_source(&created.id, &key).unwrap();
        assert_eq!(
            store.find_assignment_by_external_key(&key).unwrap(),
            Some(created)
        );
    }

    #[test]
    fn production_course_binding_failure_rolls_back_entity_and_schedule() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .create_course_with_source(
                "Existing",
                vec![ScheduleEntry::OnDemand],
                OriginKind::WebClass,
                "group",
            )
            .unwrap();
        assert_eq!(
            store.create_course_with_source(
                "Rejected",
                vec![slot(DayOfWeek::Monday, 0), slot(DayOfWeek::Sunday, 255)],
                OriginKind::WebClass,
                "group"
            ),
            Err(PersistenceError::ExternalIdentityAlreadyBound)
        );
        assert_eq!(count(&store.conn, "courses"), 1);
        assert_eq!(count(&store.conn, "course_schedule"), 1);
        assert_eq!(count(&store.conn, "course_sources"), 1);
        assert_eq!(
            store
                .find_course_by_source(OriginKind::WebClass, "group")
                .unwrap(),
            Some(first)
        );
        assert!(store.conn.is_autocommit());
    }

    #[test]
    fn production_assignment_optional_binding_is_atomic() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let key = external_key(OriginKind::Manual, "scope", "id");
        let first = store
            .create_assignment_inner(manual_input(), Some(&key))
            .unwrap();
        assert_generated_id(first.id.as_str(), "a_");
        assert_eq!(first.completion, CompletionState::Incomplete);
        assert_eq!(
            store.find_assignment_by_external_key(&key).unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            store.create_assignment_inner(manual_input(), Some(&key)),
            Err(PersistenceError::ExternalIdentityAlreadyBound)
        );
        assert_eq!(count(&store.conn, "assignments"), 1);
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
        assert_eq!(store.get_assignment(&first.id).unwrap(), Some(first));
        assert!(store.conn.is_autocommit());
    }

    #[test]
    fn production_course_schedule_failure_rolls_back_before_source_binding() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.conn.execute_batch("CREATE TRIGGER fail_slot BEFORE INSERT ON course_schedule WHEN NEW.position = 1 BEGIN SELECT RAISE(ABORT, 'synthetic'); END;").unwrap();
        assert_eq!(
            store.create_course_with_source(
                "Rejected",
                vec![ScheduleEntry::OnDemand; 2],
                OriginKind::WebClass,
                "group"
            ),
            Err(PersistenceError::Constraint)
        );
        assert_eq!(count(&store.conn, "courses"), 0);
        assert_eq!(count(&store.conn, "course_schedule"), 0);
        assert_eq!(count(&store.conn, "course_sources"), 0);
        assert_eq!(store.find_webclass_course_id("group").unwrap(), None);
        assert!(store.conn.is_autocommit());
    }

    #[test]
    fn production_manual_creation_requires_an_existing_course() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut input = manual_input();
        input.course_id = Some(CourseId::new("absent"));
        assert_eq!(
            store.create_manual_assignment(input),
            Err(PersistenceError::Constraint)
        );
        for table in TABLES {
            assert_eq!(count(&store.conn, table), 0);
        }
    }

    #[test]
    fn id_retry_handles_real_primary_key_collisions_for_both_entity_tables() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for prefix in ["a_", "c_"] {
            let duplicate = format!("{prefix}{}", "0".repeat(32));
            let fresh = format!("{prefix}{}", "1".repeat(32));
            if prefix == "a_" {
                store.insert_assignment(&assignment(&duplicate)).unwrap();
            } else {
                store.insert_course(&course(&duplicate, vec![])).unwrap();
            }
            let mut ids = [duplicate.as_str(), duplicate.as_str(), fresh.as_str()].into_iter();
            let mut attempts = 0;
            let insert = |id: &str| {
                attempts += 1;
                if prefix == "a_" {
                    insert_assignment_row(&store.conn, &assignment(id), STAMP)
                } else {
                    insert_course_row(&store.conn, &course(id, vec![]), STAMP)
                }
            };
            assert_eq!(
                insert_with_generated_id(|| Ok(ids.next().unwrap().into()), insert),
                Ok(())
            );
            assert_eq!(attempts, 3);
            assert_eq!(
                count(
                    &store.conn,
                    if prefix == "a_" {
                        "assignments"
                    } else {
                        "courses"
                    }
                ),
                2
            );
        }
    }

    #[test]
    fn id_retry_is_bounded_and_exhaustion_does_not_expose_the_id() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.insert_assignment(&assignment(PRIVATE)).unwrap();
        let mut attempts = 0;
        let result = insert_with_generated_id(
            || Ok(PRIVATE.into()),
            |id| {
                attempts += 1;
                insert_assignment_row(&store.conn, &assignment(id), STAMP)
            },
        );
        let error = result.unwrap_err();
        assert_eq!(error, PersistenceError::IdGenerationFailed);
        assert_eq!(attempts, ID_INSERT_ATTEMPTS);
        assert_eq!(count(&store.conn, "assignments"), 1);
        assert_safe(error);
    }

    #[test]
    fn id_retry_does_not_retry_other_constraints_or_database_errors() {
        for code in [
            ffi::SQLITE_CONSTRAINT_UNIQUE,
            ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            ffi::SQLITE_CONSTRAINT_CHECK,
            ffi::SQLITE_CONSTRAINT_TRIGGER,
            ffi::SQLITE_BUSY,
        ] {
            let mut attempts = 0;
            let result = insert_with_generated_id(
                || Ok(PRIVATE.into()),
                |_| {
                    attempts += 1;
                    Err(rusqlite::Error::SqliteFailure(
                        ffi::Error::new(code),
                        Some(PRIVATE.into()),
                    ))
                },
            );
            assert_eq!(attempts, 1);
            let error = result.unwrap_err();
            assert_eq!(
                error,
                if code == ffi::SQLITE_BUSY {
                    PersistenceError::Database
                } else {
                    PersistenceError::Constraint
                }
            );
            assert_safe(error);
        }
        let mut inserts = 0;
        let result = insert_with_generated_id(
            || Err(rusqlite::Error::InvalidPath(PathBuf::from(PRIVATE))),
            |_| {
                inserts += 1;
                Ok(())
            },
        );
        assert_eq!(result, Err(PersistenceError::IdGenerationFailed));
        assert_eq!(inserts, 0);
        assert_safe(result.unwrap_err());
    }

    #[test]
    fn external_lookups_reject_multiple_matches_in_a_corrupted_schema() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let assignments = [
            store.create_manual_assignment(manual_input()).unwrap(),
            store.create_manual_assignment(manual_input()).unwrap(),
        ];
        let courses = [
            store.create_course("Course", vec![]).unwrap(),
            store.create_course("Course", vec![]).unwrap(),
        ];
        // Deliberately replace only source tables to simulate missing constraints.
        store.conn.execute_batch("DROP TABLE assignment_sources; CREATE TABLE assignment_sources (assignment_id TEXT, origin TEXT, scope TEXT, external_id TEXT); DROP TABLE course_sources; CREATE TABLE course_sources (course_id TEXT, origin TEXT, external_id TEXT);").unwrap();
        for entity in assignments {
            store
                .conn
                .execute(
                    "INSERT INTO assignment_sources VALUES (?1, 'manual', 'scope', 'id')",
                    [entity.id.as_str()],
                )
                .unwrap();
        }
        for entity in courses {
            store
                .conn
                .execute(
                    "INSERT INTO course_sources VALUES (?1, 'webclass', 'group')",
                    [entity.id.as_str()],
                )
                .unwrap();
        }
        assert_eq!(
            store.find_assignment_by_external_key(&external_key(OriginKind::Manual, "scope", "id")),
            Err(PersistenceError::InvalidStoredData)
        );
        assert_eq!(
            store.find_course_by_source(OriginKind::WebClass, "group"),
            Err(PersistenceError::InvalidStoredData)
        );
        assert_eq!(
            store.find_webclass_course_id("group"),
            Err(PersistenceError::InvalidStoredData)
        );
    }

    #[test]
    fn external_lookups_use_domain_validation_including_metadata_and_schedule() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let created = store.create_manual_assignment(manual_input()).unwrap();
        let key = external_key(OriginKind::Manual, "scope", "id");
        store.bind_assignment_source(&created.id, &key).unwrap();
        let created_course = store
            .create_course_with_source(
                "Course",
                vec![ScheduleEntry::OnDemand; 3],
                OriginKind::WebClass,
                "group",
            )
            .unwrap();
        store
            .conn
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        store
            .conn
            .execute("UPDATE assignments SET submission = ?1", [PRIVATE])
            .unwrap();
        assert_eq!(
            store.find_assignment_by_external_key(&key),
            Err(PersistenceError::InvalidStoredData)
        );
        store
            .conn
            .execute(
                "UPDATE assignments SET submission = 'external', created_at = ?1",
                [PRIVATE],
            )
            .unwrap();
        assert_eq!(
            store.find_assignment_by_external_key(&key),
            Err(PersistenceError::InvalidStoredData)
        );
        store
            .conn
            .execute(
                "UPDATE course_schedule SET position = 3 WHERE course_id = ?1 AND position = 2",
                [created_course.id.as_str()],
            )
            .unwrap();
        assert_eq!(
            store.find_course_by_source(OriginKind::WebClass, "group"),
            Err(PersistenceError::InvalidStoredData)
        );
        assert_eq!(
            store.find_webclass_course_id("group"),
            Err(PersistenceError::InvalidStoredData)
        );
    }

    #[test]
    fn production_binding_errors_discard_external_identity_and_sqlite_messages() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let created = store.create_manual_assignment(manual_input()).unwrap();
        let created_course = store.create_course("Course", vec![]).unwrap();
        store.conn.execute_batch("CREATE TRIGGER reject_assignment_binding BEFORE INSERT ON assignment_sources BEGIN SELECT RAISE(ABORT, NEW.scope || NEW.external_id); END; CREATE TRIGGER reject_course_binding BEFORE INSERT ON course_sources BEGIN SELECT RAISE(ABORT, NEW.external_id); END;").unwrap();
        let key = external_key(OriginKind::Manual, PRIVATE, PRIVATE);
        let error = store.bind_assignment_source(&created.id, &key).unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        let error = store
            .bind_course_source(&created_course.id, OriginKind::Manual, PRIVATE)
            .unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        assert_eq!(count(&store.conn, "assignment_sources"), 0);
        assert_eq!(count(&store.conn, "course_sources"), 0);
        assert_safe(binding_error(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
            Some(PRIVATE.into()),
        )));
        assert_safe(binding_error(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CONSTRAINT_UNIQUE),
            Some(PRIVATE.into()),
        )));
        assert_safe(binding_error(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some(PRIVATE.into()),
        )));
    }

    fn import_candidate(origin: OriginKind, scope: &str, id: &str) -> AssignmentCandidate {
        AssignmentCandidate {
            external_key: external_key(origin, scope, id),
            title: "Source report 日本語 ' ?1".into(),
            deadline: Some(Deadline::DateTime(decode_timestamp(STAMP).unwrap())),
            submission: SubmissionTarget::WebClass,
            submission_url: Some("https://submit.example.invalid/source".into()),
        }
    }

    fn batch(candidates: Vec<AssignmentCandidate>) -> AssignmentCandidateBatch {
        AssignmentCandidateBatch::try_new(candidates).unwrap()
    }

    fn assignment_metadata(store: &SqliteStore, id: &AssignmentId) -> (String, String) {
        store
            .conn
            .query_row(
                "SELECT created_at, updated_at FROM assignments WHERE id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    fn age_updated_at(store: &SqliteStore, id: &AssignmentId) {
        // A deterministic old timestamp avoids sleeps/clock-resolution assumptions.
        store
            .conn
            .execute(
                "UPDATE assignments SET updated_at = ?1 WHERE id = ?2",
                params!["2000-01-01T00:00:00.000000001Z", id.as_str()],
            )
            .unwrap();
    }

    #[test]
    fn validated_import_creates_bound_assignments_with_generated_ids_and_exact_fields() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let candidates = batch(vec![
            import_candidate(OriginKind::Moodle, "cis-feed", "0001@calendar"),
            import_candidate(OriginKind::WebClass, "group", "0001"),
            import_candidate(OriginKind::Manual, "manual-logical-source", "0001"),
        ]);
        let before = Utc::now();
        assert_eq!(
            store.import_assignment_batch(&candidates).unwrap(),
            AssignmentImportSummary {
                created: 3,
                ..Default::default()
            }
        );
        let after = Utc::now();
        for candidate in candidates.as_slice() {
            let stored = store
                .find_assignment_by_external_key(&candidate.external_key)
                .unwrap()
                .unwrap();
            assert_generated_id(stored.id.as_str(), "a_");
            assert_eq!(stored.title, candidate.title);
            assert_eq!(stored.deadline, candidate.deadline);
            assert_eq!(stored.submission, candidate.submission);
            assert_eq!(stored.submission_url, candidate.submission_url);
            assert_eq!(stored.completion, CompletionState::Incomplete);
            assert_eq!(stored.course_id, None);
            assert_eq!(
                store.get_assignment(&stored.id).unwrap(),
                Some(stored.clone())
            );
            let (created, updated) = assignment_metadata(&store, &stored.id);
            assert_eq!(created, updated);
            let instant = decode_timestamp(&created).unwrap();
            assert!(before <= instant && instant <= after);
        }
        assert_eq!(count(&store.conn, "assignments"), 3);
        assert_eq!(count(&store.conn, "assignment_sources"), 3);
        assert_eq!(count(&store.conn, "courses"), 0);
    }

    #[test]
    fn reimport_updates_source_fields_and_preserves_identity_created_at_and_completion() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut candidate = import_candidate(OriginKind::WebClass, "group", "id");
        store
            .import_assignment_batch(&batch(vec![candidate.clone()]))
            .unwrap();
        let original = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        store
            .set_completion(&original.id, CompletionState::Completed)
            .unwrap();
        age_updated_at(&store, &original.id);
        let metadata = assignment_metadata(&store, &original.id);
        candidate.title = "Changed source title".into();
        candidate.deadline = Some(Deadline::DateOnly(
            NaiveDate::from_ymd_opt(2028, 2, 29).unwrap(),
        ));
        let before = Utc::now();
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap(),
            AssignmentImportSummary {
                updated: 1,
                ..Default::default()
            }
        );
        let after = Utc::now();
        let updated = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(updated.id, original.id);
        assert_eq!(updated.title, candidate.title);
        assert_eq!(updated.deadline, candidate.deadline);
        assert_eq!(updated.completion, CompletionState::Completed);
        let current = assignment_metadata(&store, &updated.id);
        assert_eq!(current.0, metadata.0);
        assert_ne!(current.1, metadata.1);
        let instant = decode_timestamp(&current.1).unwrap();
        assert!(before <= instant && instant <= after);
        assert_eq!(count(&store.conn, "assignments"), 1);
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
    }

    #[test]
    fn unchanged_and_semantically_unchanged_imports_do_not_execute_update() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let candidate = import_candidate(OriginKind::WebClass, "group", "id");
        store
            .import_assignment_batch(&batch(vec![candidate.clone()]))
            .unwrap();
        let original = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        store
            .set_completion(&original.id, CompletionState::Completed)
            .unwrap();
        age_updated_at(&store, &original.id);
        let original = store.get_assignment(&original.id).unwrap().unwrap();
        let metadata = assignment_metadata(&store, &original.id);
        store.conn.execute_batch("CREATE TRIGGER reject_noop_updates BEFORE UPDATE ON assignments BEGIN SELECT RAISE(ABORT, 'an UPDATE should not execute'); END;").unwrap();
        let changes = store.conn.total_changes();
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap(),
            AssignmentImportSummary {
                unchanged: 1,
                ..Default::default()
            }
        );
        let mut unknown = candidate;
        unknown.submission = SubmissionTarget::Unknown;
        unknown.submission_url = None;
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![unknown]))
                .unwrap(),
            AssignmentImportSummary {
                unchanged: 1,
                ..Default::default()
            }
        );
        assert_eq!(store.conn.total_changes(), changes);
        assert_eq!(
            store.get_assignment(&original.id).unwrap(),
            Some(original.clone())
        );
        assert_eq!(assignment_metadata(&store, &original.id), metadata);
    }

    #[test]
    fn completion_local_update_changes_only_completion_and_timestamp_and_survives_import() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let candidate = import_candidate(OriginKind::WebClass, "group", "id");
        let candidates = batch(vec![candidate.clone()]);
        store.import_assignment_batch(&candidates).unwrap();
        let mut expected = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(expected.completion, CompletionState::Incomplete);
        age_updated_at(&store, &expected.id);
        let metadata = assignment_metadata(&store, &expected.id);
        let changes = store.conn.total_changes();
        store
            .set_completion(&expected.id, CompletionState::Incomplete)
            .unwrap();
        assert_eq!(store.conn.total_changes(), changes);
        assert_eq!(assignment_metadata(&store, &expected.id), metadata);
        let before = Utc::now();
        store
            .set_completion(&expected.id, CompletionState::Completed)
            .unwrap();
        let after = Utc::now();
        expected.completion = CompletionState::Completed;
        assert_eq!(
            store.get_assignment(&expected.id).unwrap(),
            Some(expected.clone())
        );
        let completed_metadata = assignment_metadata(&store, &expected.id);
        assert_eq!(completed_metadata.0, metadata.0);
        assert_ne!(completed_metadata.1, metadata.1);
        let instant = decode_timestamp(&completed_metadata.1).unwrap();
        assert!(before <= instant && instant <= after);
        store.conn.execute_batch("CREATE TRIGGER reject_noop_completion BEFORE UPDATE ON assignments BEGIN SELECT RAISE(ABORT, 'an UPDATE should not execute'); END;").unwrap();
        let changes = store.conn.total_changes();
        store
            .set_completion(&expected.id, CompletionState::Completed)
            .unwrap();
        assert_eq!(
            store
                .import_assignment_batch(&candidates)
                .unwrap()
                .unchanged,
            1
        );
        assert_eq!(store.conn.total_changes(), changes);
        assert_eq!(
            assignment_metadata(&store, &expected.id),
            completed_metadata
        );
        assert_eq!(
            store.get_assignment(&expected.id).unwrap(),
            Some(expected.clone())
        );
        let error = store
            .set_completion(&AssignmentId::new(PRIVATE), CompletionState::Completed)
            .unwrap_err();
        assert_eq!(error, PersistenceError::NotFound);
        assert_safe(error);
        store
            .conn
            .execute_batch("DROP TRIGGER reject_noop_completion")
            .unwrap();
        store
            .set_completion(&expected.id, CompletionState::Incomplete)
            .unwrap();
        expected.completion = CompletionState::Incomplete;
        assert_eq!(store.get_assignment(&expected.id).unwrap(), Some(expected));
    }

    #[test]
    fn import_clears_deadline_but_preserves_unknown_submission_and_missing_url() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut candidate = import_candidate(OriginKind::WebClass, "group", "id");
        store
            .import_assignment_batch(&batch(vec![candidate.clone()]))
            .unwrap();
        let original = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        candidate.deadline = None;
        candidate.submission = SubmissionTarget::Unknown;
        candidate.submission_url = None;
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap()
                .updated,
            1
        );
        let stored = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(stored.deadline, None);
        assert_eq!(stored.submission, SubmissionTarget::WebClass);
        assert_eq!(stored.submission_url, original.submission_url);
        candidate.submission = SubmissionTarget::None;
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap()
                .updated,
            1
        );
        let stored = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(stored.submission, SubmissionTarget::None);
        assert_eq!(stored.submission_url, original.submission_url);
        candidate.submission = SubmissionTarget::Unknown;
        candidate.submission_url = Some("https://submit.example.invalid/new-answer".into());
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap()
                .updated,
            1
        );
        let stored = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(stored.submission, SubmissionTarget::None);
        assert_eq!(stored.submission_url, candidate.submission_url);
        assert_eq!(stored.id, original.id);
    }

    #[test]
    fn webclass_course_resolution_is_per_exact_scope_and_preserves_unresolved_relations() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .create_course_with_source("Same name", vec![], OriginKind::WebClass, " Group-01 ")
            .unwrap();
        let second = store
            .create_course_with_source("Same name", vec![], OriginKind::WebClass, "other-group")
            .unwrap();
        let candidate = import_candidate(OriginKind::WebClass, " Group-01 ", "id");
        let other = import_candidate(OriginKind::WebClass, "other-group", "id");
        let unresolved = import_candidate(OriginKind::WebClass, "Group-01", "id");
        store
            .import_assignment_batch(&batch(vec![
                candidate.clone(),
                other.clone(),
                unresolved.clone(),
            ]))
            .unwrap();
        let bound = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        assert_eq!(bound.course_id, Some(first.id.clone()));
        assert_eq!(
            store
                .find_assignment_by_external_key(&other.external_key)
                .unwrap()
                .unwrap()
                .course_id,
            Some(second.id)
        );
        assert_eq!(
            store
                .find_assignment_by_external_key(&unresolved.external_key)
                .unwrap()
                .unwrap()
                .course_id,
            None
        );
        age_updated_at(&store, &bound.id);
        let metadata = assignment_metadata(&store, &bound.id);
        // Simulate an explicitly absent binding, not source-disappearance deletion.
        store
            .conn
            .execute(
                "DELETE FROM course_sources WHERE course_id = ?1",
                [first.id.as_str()],
            )
            .unwrap();
        assert_eq!(store.find_webclass_course_id(" Group-01 ").unwrap(), None);
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap()
                .unchanged,
            1
        );
        assert_eq!(
            store.get_assignment(&bound.id).unwrap().unwrap().course_id,
            Some(first.id)
        );
        assert_eq!(assignment_metadata(&store, &bound.id), metadata);
        let new_unresolved = import_candidate(OriginKind::WebClass, " Group-01 ", "new-id");
        store
            .import_assignment_batch(&batch(vec![new_unresolved.clone()]))
            .unwrap();
        assert_eq!(
            store
                .find_assignment_by_external_key(&new_unresolved.external_key)
                .unwrap()
                .unwrap()
                .course_id,
            None
        );
        let replacement = store
            .create_course_with_source("Same name", vec![], OriginKind::WebClass, " Group-01 ")
            .unwrap();
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![candidate.clone()]))
                .unwrap()
                .updated,
            1
        );
        assert_eq!(
            store.get_assignment(&bound.id).unwrap().unwrap().course_id,
            Some(replacement.id)
        );
    }

    #[test]
    fn moodle_feed_scopes_never_imply_courses_and_unresolved_import_preserves_an_existing_course() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let course = store.create_course("Course", vec![]).unwrap();
        let existing = import_candidate(OriginKind::Moodle, course.id.as_str(), "existing");
        let mut input = manual_input();
        input.course_id = Some(course.id.clone());
        let stored = store.create_manual_assignment(input).unwrap();
        store
            .bind_assignment_source(&stored.id, &existing.external_key)
            .unwrap();
        let new = import_candidate(OriginKind::Moodle, course.id.as_str(), "new");
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![existing.clone(), new.clone()]))
                .unwrap(),
            AssignmentImportSummary {
                created: 1,
                updated: 1,
                unchanged: 0
            }
        );
        assert_eq!(
            store.get_assignment(&stored.id).unwrap().unwrap().course_id,
            Some(course.id)
        );
        assert_eq!(
            store
                .find_assignment_by_external_key(&new.external_key)
                .unwrap()
                .unwrap()
                .course_id,
            None
        );
        assert_eq!(count(&store.conn, "courses"), 1);
    }

    #[test]
    fn batch_binding_failure_rolls_back_prior_inserts_updates_and_all_bindings() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut existing = import_candidate(OriginKind::Manual, "scope", "existing");
        store
            .import_assignment_batch(&batch(vec![existing.clone()]))
            .unwrap();
        let before = store
            .find_assignment_by_external_key(&existing.external_key)
            .unwrap()
            .unwrap();
        store
            .set_completion(&before.id, CompletionState::Completed)
            .unwrap();
        age_updated_at(&store, &before.id);
        let before = store.get_assignment(&before.id).unwrap().unwrap();
        let metadata = assignment_metadata(&store, &before.id);
        store.conn.execute_batch("CREATE TRIGGER fail_batch_binding BEFORE INSERT ON assignment_sources WHEN NEW.external_id = 'fail' BEGIN SELECT RAISE(ABORT, NEW.scope || NEW.external_id); END;").unwrap();
        let created = import_candidate(OriginKind::Manual, "scope", "created-first");
        existing.title = "Updated then rolled back".into();
        let fail = import_candidate(OriginKind::Manual, PRIVATE, "fail");
        let error = store
            .import_assignment_batch(&batch(vec![
                created.clone(),
                existing.clone(),
                fail.clone(),
            ]))
            .unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        assert!(store.conn.is_autocommit());
        assert_eq!(count(&store.conn, "assignments"), 1);
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
        assert_eq!(
            store.get_assignment(&before.id).unwrap(),
            Some(before.clone())
        );
        assert_eq!(assignment_metadata(&store, &before.id), metadata);
        assert_eq!(
            store
                .find_assignment_by_external_key(&created.external_key)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .find_assignment_by_external_key(&fail.external_key)
                .unwrap(),
            None
        );
    }

    #[test]
    fn corrupt_external_binding_is_rejected_and_rolls_back_the_batch() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .conn
            .pragma_update(None, "foreign_keys", "OFF")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO assignment_sources VALUES ('absent', 'manual', ?1, 'orphan')",
                [PRIVATE],
            )
            .unwrap();
        store
            .conn
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        let created = import_candidate(OriginKind::Manual, "scope", "created");
        let orphan = import_candidate(OriginKind::Manual, PRIVATE, "orphan");
        let error = store
            .import_assignment_batch(&batch(vec![created.clone(), orphan]))
            .unwrap_err();
        assert_eq!(error, PersistenceError::InvalidStoredData);
        assert_safe(error);
        assert_eq!(count(&store.conn, "assignments"), 0);
        assert_eq!(count(&store.conn, "assignment_sources"), 1);
        assert_eq!(
            store
                .find_assignment_by_external_key(&created.external_key)
                .unwrap(),
            None
        );
    }

    #[test]
    fn empty_and_partial_batches_never_delete_or_change_absent_candidates() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first = import_candidate(OriginKind::Manual, "scope", "first");
        let absent = import_candidate(OriginKind::Manual, "scope", "absent");
        store
            .import_assignment_batch(&batch(vec![first.clone(), absent.clone()]))
            .unwrap();
        let stored_first = store
            .find_assignment_by_external_key(&first.external_key)
            .unwrap()
            .unwrap();
        let stored_absent = store
            .find_assignment_by_external_key(&absent.external_key)
            .unwrap()
            .unwrap();
        store
            .set_completion(&stored_absent.id, CompletionState::Completed)
            .unwrap();
        let stored_absent = store.get_assignment(&stored_absent.id).unwrap().unwrap();
        let first_metadata = assignment_metadata(&store, &stored_first.id);
        let absent_metadata = assignment_metadata(&store, &stored_absent.id);
        let changes = store.conn.total_changes();
        assert_eq!(
            store.import_assignment_batch(&batch(vec![])).unwrap(),
            AssignmentImportSummary::default()
        );
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![first.clone()]))
                .unwrap()
                .unchanged,
            1
        );
        assert_eq!(store.conn.total_changes(), changes);
        assert_eq!(
            assignment_metadata(&store, &stored_first.id),
            first_metadata
        );
        let mut changed = first;
        changed.title = "Changed present candidate".into();
        assert_eq!(
            store
                .import_assignment_batch(&batch(vec![changed]))
                .unwrap()
                .updated,
            1
        );
        assert_eq!(
            store.get_assignment(&stored_absent.id).unwrap(),
            Some(stored_absent.clone())
        );
        assert_eq!(
            assignment_metadata(&store, &stored_absent.id),
            absent_metadata
        );
        assert_eq!(count(&store.conn, "assignments"), 2);
        assert_eq!(count(&store.conn, "assignment_sources"), 2);
    }

    #[test]
    fn moodle_fixture_parse_map_and_persist_requires_no_http_client() {
        let events =
            crate::moodle::parse_calendar(include_str!("../fixtures/moodle/calendar_multiple.ics"))
                .unwrap();
        assert_eq!(events.len(), 3);
        let candidates =
            AssignmentCandidateBatch::try_new(crate::moodle::map_events(&events, "cis-feed"))
                .unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(
            store.import_assignment_batch(&candidates).unwrap().created,
            2
        );
        let end = store
            .find_assignment_by_external_key(&external_key(
                OriginKind::Moodle,
                "cis-feed",
                "2002@cms.example.edu",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(end.title, "課題（DFS/BFS）");
        assert_eq!(
            end.deadline,
            Some(Deadline::DateTime(
                decode_timestamp("2026-10-08T01:50:00.000000000Z").unwrap()
            ))
        );
        let deadline = store
            .find_assignment_by_external_key(&external_key(
                OriginKind::Moodle,
                "cis-feed",
                "2003@cms.example.edu",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(deadline.title, "CF-L01課題");
        assert_eq!(
            deadline.deadline,
            Some(Deadline::DateTime(
                decode_timestamp("2026-10-04T14:59:00.000000000Z").unwrap()
            ))
        );
        for assignment in [end, deadline] {
            assert_eq!(assignment.submission, SubmissionTarget::Unknown);
            assert_eq!(assignment.submission_url, None);
            assert_eq!(assignment.completion, CompletionState::Incomplete);
            assert_eq!(assignment.course_id, None);
            assert_eq!(
                store.get_assignment(&assignment.id).unwrap(),
                Some(assignment)
            );
        }
        assert_eq!(
            store
                .find_assignment_by_external_key(&external_key(
                    OriginKind::Moodle,
                    "cis-feed",
                    "2001@cms.example.edu"
                ))
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .import_assignment_batch(&candidates)
                .unwrap()
                .unchanged,
            2
        );
        assert_eq!(count(&store.conn, "assignments"), 2);
    }

    #[test]
    fn webclass_fixture_parse_map_and_persist_keeps_completion_local_and_filtered_records() {
        let raw = include_str!("../fixtures/webclass/contents_states.json");
        let group = "fixture-course-002";
        let contents = crate::webclass::parse_contents(raw).unwrap();
        let candidates = crate::webclass::map_contents_for_group(&contents, group).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let course = store
            .create_course_with_source("Synthetic course", vec![], OriginKind::WebClass, group)
            .unwrap();
        assert_eq!(
            store.import_assignment_batch(&candidates).unwrap().created,
            3
        );
        let mut stored = Vec::new();
        for candidate in candidates.as_slice() {
            let assignment = store
                .find_assignment_by_external_key(&candidate.external_key)
                .unwrap()
                .unwrap();
            assert_eq!(assignment.title, candidate.title);
            assert_eq!(
                assignment.deadline,
                Some(Deadline::DateTime(
                    decode_timestamp("2026-10-31T14:59:59.000000000Z").unwrap()
                ))
            );
            assert_eq!(assignment.course_id, Some(course.id.clone()));
            assert_eq!(assignment.submission, SubmissionTarget::WebClass);
            assert_eq!(assignment.submission_url, None);
            assert_eq!(assignment.completion, CompletionState::Incomplete);
            assert_eq!(
                store.get_assignment(&assignment.id).unwrap(),
                Some(assignment.clone())
            );
            stored.push(assignment);
        }
        // Score/answer data in the fixture never sets local completion.
        assert!(contents[1].scores[0].answer_datetime.is_some());
        assert_eq!(contents[2].scores[0].max_score, Some(0));
        store
            .set_completion(&stored[1].id, CompletionState::Completed)
            .unwrap();
        let completed_metadata = assignment_metadata(&store, &stored[1].id);
        let contents_again = crate::webclass::parse_contents(raw).unwrap();
        let again = crate::webclass::map_contents_for_group(&contents_again, group).unwrap();
        assert_eq!(store.import_assignment_batch(&again).unwrap().unchanged, 3);
        assert_eq!(
            store
                .get_assignment(&stored[1].id)
                .unwrap()
                .unwrap()
                .completion,
            CompletionState::Completed
        );
        assert_eq!(
            assignment_metadata(&store, &stored[1].id),
            completed_metadata
        );
        let mut filtered = contents_again;
        filtered[1].hidden_content = Some(true);
        let filtered_batch = crate::webclass::map_contents_for_group(&filtered, group).unwrap();
        assert_eq!(filtered_batch.as_slice().len(), 2);
        assert_eq!(
            store
                .import_assignment_batch(&filtered_batch)
                .unwrap()
                .unchanged,
            2
        );
        assert_eq!(
            store
                .get_assignment(&stored[1].id)
                .unwrap()
                .unwrap()
                .completion,
            CompletionState::Completed
        );
        assert_eq!(count(&store.conn, "assignments"), 3);
        assert_eq!(count(&store.conn, "assignment_sources"), 3);
    }

    #[test]
    fn imported_assignments_and_local_completion_survive_file_reopen() {
        let temp = TempDb::new();
        let candidate = import_candidate(OriginKind::Manual, "scope", "id");
        let candidates = batch(vec![candidate.clone()]);
        let expected;
        let metadata;
        {
            let mut store = SqliteStore::open(&temp.path).unwrap();
            store.import_assignment_batch(&candidates).unwrap();
            let id = store
                .find_assignment_by_external_key(&candidate.external_key)
                .unwrap()
                .unwrap()
                .id;
            store
                .set_completion(&id, CompletionState::Completed)
                .unwrap();
            expected = store.get_assignment(&id).unwrap().unwrap();
            metadata = assignment_metadata(&store, &id);
        }
        let mut store = SqliteStore::open(&temp.path).unwrap();
        assert_eq!(
            store
                .find_assignment_by_external_key(&candidate.external_key)
                .unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            store
                .import_assignment_batch(&candidates)
                .unwrap()
                .unchanged,
            1
        );
        assert_eq!(
            store.get_assignment(&expected.id).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(assignment_metadata(&store, &expected.id), metadata);
        assert_eq!(pragma_number(&store.conn, "user_version"), 1);
    }

    #[test]
    fn import_and_completion_failures_discard_candidate_values_and_driver_messages() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut candidate = import_candidate(OriginKind::Manual, PRIVATE, PRIVATE);
        candidate.title = PRIVATE.into();
        candidate.submission_url = Some(format!("https://submit.example.invalid/{PRIVATE}"));
        store.conn.execute_batch("CREATE TRIGGER reject_import_insert BEFORE INSERT ON assignments BEGIN SELECT RAISE(ABORT, NEW.title || NEW.submission_url); END;").unwrap();
        let error = store
            .import_assignment_batch(&batch(vec![candidate.clone()]))
            .unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        assert_eq!(count(&store.conn, "assignments"), 0);
        store
            .conn
            .execute_batch("DROP TRIGGER reject_import_insert")
            .unwrap();
        store
            .import_assignment_batch(&batch(vec![candidate.clone()]))
            .unwrap();
        let stored = store
            .find_assignment_by_external_key(&candidate.external_key)
            .unwrap()
            .unwrap();
        let metadata = assignment_metadata(&store, &stored.id);
        store.conn.execute_batch("CREATE TRIGGER reject_import_update BEFORE UPDATE ON assignments BEGIN SELECT RAISE(ABORT, NEW.title || NEW.submission_url); END;").unwrap();
        candidate.title.push_str(" changed");
        let error = store
            .import_assignment_batch(&batch(vec![candidate]))
            .unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        let error = store
            .set_completion(&stored.id, CompletionState::Completed)
            .unwrap_err();
        assert_eq!(error, PersistenceError::Constraint);
        assert_safe(error);
        assert_eq!(
            store.get_assignment(&stored.id).unwrap(),
            Some(stored.clone())
        );
        assert_eq!(assignment_metadata(&store, &stored.id), metadata);
    }
}
