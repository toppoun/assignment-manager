//! Dedicated SQLite state for apply operations. Dry runs only open it read-only.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read},
    path::Path,
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{SyncKey, UniversitySource, wire::uuid};

const APPLICATION_ID: i64 = 0x414d4e53; // AMNS
const VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    InvalidPath,
    Busy,
    Io,
    InvalidDatabase,
    Database,
    InvalidTransition,
}

/// Reject reparse points and symlinks in app-owned paths, including ancestors.
pub fn check_path(path: &Path) -> Result<(), StateError> {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(StateError::InvalidPath);
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err(StateError::InvalidPath);
                    }
                }
                if meta.file_type().is_symlink() {
                    return Err(StateError::InvalidPath);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(StateError::Io),
        }
    }
    Ok(())
}

/// The open handle owns an OS lock. Stale lock files are reusable after exit.
pub struct TargetLock {
    _file: File,
}

impl TargetLock {
    #[cfg(target_os = "macos")]
    pub(crate) fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self._file)
    }

    pub fn acquire(directory: &Path) -> Result<Self, StateError> {
        check_path(directory)?;
        fs::create_dir_all(directory).map_err(|_| StateError::Io)?;
        let path = directory.join("run.lock");
        check_path(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| StateError::Io)?;
        if !file.metadata().map_err(|_| StateError::Io)?.is_file() {
            return Err(StateError::InvalidPath);
        }
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(StateError::Busy),
            Err(TryLockError::Error(_)) => Err(StateError::Io),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Partial,
    Failed,
}

impl RunOutcome {
    pub fn code(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    fn parse(code: &str) -> Result<Self, StateError> {
        match code {
            "success" => Ok(Self::Success),
            "partial" => Ok(Self::Partial),
            "failed" => Ok(Self::Failed),
            _ => Err(StateError::InvalidDatabase),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunIssue {
    Acquisition,
    Notion,
    Reconciliation,
    Interrupted,
}

impl RunIssue {
    pub fn code(self) -> &'static str {
        match self {
            Self::Acquisition => "acquisition",
            Self::Notion => "notion",
            Self::Reconciliation => "reconciliation",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(code: &str) -> Result<Self, StateError> {
        match code {
            "acquisition" => Ok(Self::Acquisition),
            "notion" => Ok(Self::Notion),
            "reconciliation" => Ok(Self::Reconciliation),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(StateError::InvalidDatabase),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingStatus {
    CreateIntent,
    Unknown,
    Reconcile,
    Linked,
}

impl BindingStatus {
    fn parse(code: &str) -> Result<Self, StateError> {
        match code {
            "intent" => Ok(Self::CreateIntent),
            "unknown" => Ok(Self::Unknown),
            "reconcile" => Ok(Self::Reconcile),
            "linked" => Ok(Self::Linked),
            _ => Err(StateError::InvalidDatabase),
        }
    }
}

#[derive(Default)]
pub struct StateSummary {
    pub(super) bindings: HashMap<SyncKey, Binding>,
    pub pending: usize,
    pub last_fetch: Option<DateTime<Utc>>,
    pub last_apply: Option<DateTime<Utc>>,
    pub last_run: Option<StateRun>,
}

impl StateSummary {
    pub(super) fn page_owners(&self) -> HashMap<&str, &SyncKey> {
        self.bindings
            .iter()
            .filter_map(|(key, binding)| Some((binding.page_id.as_deref()?, key)))
            .collect()
    }
}

pub struct StateRun {
    pub finished_at: DateTime<Utc>,
    pub outcome: RunOutcome,
    pub issue: Option<RunIssue>,
    pub deferred: usize,
}

pub struct Binding {
    pub status: BindingStatus,
    pub page_id: Option<String>,
}

pub struct StateStore(Connection);

impl StateStore {
    /// Explicit bootstrap, publishing a complete database without replacing a
    /// preexisting file. Reinitializing a verified state file is a no-op.
    pub fn initialize(path: &Path, target: &str) -> Result<(), StateError> {
        check_path(path)?;
        let target = uuid(target).ok_or(StateError::InvalidDatabase)?;
        if exists(path)? {
            return Self::open(path, &target, OpenFlags::SQLITE_OPEN_READ_ONLY)?
                .summary()
                .map(|_| ());
        }
        let parent = path.parent().ok_or(StateError::InvalidPath)?;
        let tmp = tempfile::NamedTempFile::new_in(parent).map_err(|_| StateError::Io)?;
        let mut conn = Connection::open(tmp.path()).map_err(|_| StateError::Database)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| StateError::Database)?;
        let tx = conn.transaction().map_err(|_| StateError::Database)?;
        tx.execute_batch(
            "CREATE TABLE target (singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL) STRICT;
             CREATE TABLE bindings (
                sync_key TEXT PRIMARY KEY NOT NULL,
                page_id TEXT UNIQUE,
                status TEXT NOT NULL CHECK(status IN ('intent','unknown','reconcile','linked')),
                CHECK((status='linked' AND page_id IS NOT NULL) OR status='reconcile'
                    OR (status IN ('intent','unknown') AND page_id IS NULL))
             ) STRICT;
             CREATE TABLE progress (
                source TEXT PRIMARY KEY NOT NULL CHECK(source IN ('moodle','webclass')),
                fetched_at INTEGER, applied_at INTEGER
             ) STRICT;
             CREATE TABLE last_run (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                finished_at INTEGER NOT NULL,
                outcome TEXT NOT NULL CHECK(outcome IN ('success','partial','failed')),
                issue TEXT CHECK(issue IN ('acquisition','notion','reconciliation','interrupted')),
                deferred INTEGER NOT NULL CHECK(deferred>=0)
             ) STRICT;",
        ).map_err(|_| StateError::Database)?;
        tx.execute("INSERT INTO target VALUES (1,?1)", [target])
            .map_err(|_| StateError::Database)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(|_| StateError::Database)?;
        tx.pragma_update(None, "user_version", VERSION)
            .map_err(|_| StateError::Database)?;
        tx.commit().map_err(|_| StateError::Database)?;
        drop(conn);
        tmp.as_file().sync_all().map_err(|_| StateError::Io)?;
        tmp.persist_noclobber(path).map_err(|_| StateError::Io)?;
        Ok(())
    }

    pub fn read(path: &Path, target: &str) -> Result<Option<StateSummary>, StateError> {
        Self::read_only(path, target)?
            .map(|state| state.summary())
            .transpose()
    }

    pub(super) fn read_only(path: &Path, target: &str) -> Result<Option<Self>, StateError> {
        check_path(path)?;
        if !exists(path)? {
            return Ok(None);
        }
        Self::open(path, target, OpenFlags::SQLITE_OPEN_READ_ONLY).map(Some)
    }

    /// Open an already initialized state file for explicit apply or recovery.
    /// The caller owns the target lock for the duration of its run.
    pub fn for_apply(path: &Path, target: &str) -> Result<Self, StateError> {
        let state = Self::open(path, target, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let integrity: String = state
            .0
            .pragma_query_value(None, "quick_check", |row| row.get(0))
            .map_err(|_| StateError::InvalidDatabase)?;
        if integrity != "ok" {
            return Err(StateError::InvalidDatabase);
        }
        state.summary()?;
        state
            .0
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| StateError::Database)?;
        Ok(state)
    }

    fn open(path: &Path, target: &str, flags: OpenFlags) -> Result<Self, StateError> {
        check_path(path)?;
        let target = uuid(target).ok_or(StateError::InvalidDatabase)?;
        check_header(path)?;
        let conn =
            Connection::open_with_flags(path, flags).map_err(|_| StateError::InvalidDatabase)?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|_| StateError::Database)?;
        let id: i64 = conn
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(|_| StateError::InvalidDatabase)?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|_| StateError::InvalidDatabase)?;
        if id != APPLICATION_ID || version != VERSION {
            return Err(StateError::InvalidDatabase);
        }
        let stored: String = conn
            .query_row("SELECT id FROM target WHERE singleton=1", [], |r| r.get(0))
            .map_err(|_| StateError::InvalidDatabase)?;
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .map_err(|_| StateError::InvalidDatabase)?;
        if stored != target || mode != "delete" {
            return Err(StateError::InvalidDatabase);
        }
        Ok(Self(conn))
    }

    /// Record an intent before any POST. Every existing row blocks a new POST.
    pub fn reserve_create(&self, key: &SyncKey) -> Result<bool, StateError> {
        self.0
            .execute(
                "INSERT INTO bindings(sync_key,status) VALUES (?1,'intent') ON CONFLICT DO NOTHING",
                [key.as_str()],
            )
            .map(|changed| changed == 1)
            .map_err(|_| StateError::Database)
    }

    pub(super) fn recurring_approval(&self) -> Result<Option<String>, StateError> {
        if !self.has_table("recurring_approval")? {
            return Ok(None);
        }
        if !self.has_table("recurring_origins")? {
            return Err(StateError::InvalidDatabase);
        }
        self.0
            .query_row(
                "SELECT fingerprint FROM recurring_approval WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| StateError::InvalidDatabase)
    }

    pub(super) fn approve_recurring(&self, fingerprint: &str) -> Result<(), StateError> {
        // These tables are additive so existing first-sync state files stay usable.
        let tx = self
            .0
            .unchecked_transaction()
            .map_err(|_| StateError::Database)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS recurring_approval (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                fingerprint TEXT NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS recurring_origins (
                sync_key TEXT PRIMARY KEY NOT NULL,
                source TEXT NOT NULL CHECK(source IN ('moodle','webclass'))
            ) STRICT;",
        )
        .map_err(|_| StateError::Database)?;
        tx.execute(
            "INSERT INTO recurring_approval VALUES (1,?1)
             ON CONFLICT(singleton) DO UPDATE SET fingerprint=excluded.fingerprint",
            [fingerprint],
        )
        .map_err(|_| StateError::Database)?;
        tx.commit().map_err(|_| StateError::Database)
    }

    pub(super) fn revoke_recurring_approval(&self) -> Result<(), StateError> {
        if self.recurring_approval()?.is_some() {
            self.0
                .execute("DELETE FROM recurring_approval WHERE singleton=1", [])
                .map_err(|_| StateError::Database)?;
        }
        Ok(())
    }

    pub(super) fn reserve_recurring_create(
        &self,
        key: &SyncKey,
        source: UniversitySource,
    ) -> Result<bool, StateError> {
        let tx = self
            .0
            .unchecked_transaction()
            .map_err(|_| StateError::Database)?;
        if !self.reserve_create(key)? {
            return Ok(false);
        }
        let source = match source {
            UniversitySource::Moodle => "moodle",
            UniversitySource::WebClass => "webclass",
        };
        tx.execute(
            "INSERT INTO recurring_origins VALUES (?1,?2)
             ON CONFLICT(sync_key) DO UPDATE SET source=excluded.source",
            params![key.as_str(), source],
        )
        .map_err(|_| StateError::Database)?;
        tx.commit().map_err(|_| StateError::Database)?;
        Ok(true)
    }

    pub(super) fn recurring_origins(
        &self,
    ) -> Result<HashMap<SyncKey, UniversitySource>, StateError> {
        if !self.has_table("recurring_origins")? {
            return Ok(HashMap::new());
        }
        let mut statement = self
            .0
            .prepare(
                "SELECT b.sync_key,r.source FROM bindings b
             JOIN recurring_origins r USING (sync_key) WHERE b.status<>'linked'",
            )
            .map_err(|_| StateError::InvalidDatabase)?;
        let mut rows = statement
            .query([])
            .map_err(|_| StateError::InvalidDatabase)?;
        let mut origins = HashMap::new();
        while let Some(row) = rows.next().map_err(|_| StateError::InvalidDatabase)? {
            let key: String = row.get(0).map_err(|_| StateError::InvalidDatabase)?;
            let source: String = row.get(1).map_err(|_| StateError::InvalidDatabase)?;
            let source = match source.as_str() {
                "moodle" => UniversitySource::Moodle,
                "webclass" => UniversitySource::WebClass,
                _ => return Err(StateError::InvalidDatabase),
            };
            origins.insert(
                key.parse().map_err(|_| StateError::InvalidDatabase)?,
                source,
            );
        }
        Ok(origins)
    }

    fn has_table(&self, name: &str) -> Result<bool, StateError> {
        self.0
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1 AND type='table')",
                [name],
                |row| row.get(0),
            )
            .map_err(|_| StateError::InvalidDatabase)
    }

    pub fn binding(&self, key: &SyncKey) -> Result<Option<Binding>, StateError> {
        let row: Option<(String, Option<String>)> = self
            .0
            .query_row(
                "SELECT status,page_id FROM bindings WHERE sync_key=?1",
                [key.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| StateError::Database)?;
        row.map(|(status, page_id)| decode_binding(&status, page_id))
            .transpose()
    }

    pub fn mark_unknown(&self, key: &SyncKey) -> Result<(), StateError> {
        self.change("UPDATE bindings SET status='unknown' WHERE sync_key=?1 AND status IN ('intent','unknown')", key)
    }

    pub(super) fn is_owned_by_another_key(
        &self,
        page: &str,
        key: &SyncKey,
    ) -> Result<bool, StateError> {
        self.0
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM bindings WHERE page_id=?1 AND sync_key<>?2)",
                params![page, key.as_str()],
                |row| row.get(0),
            )
            .map_err(|_| StateError::Database)
    }

    /// Release only an intent whose request got an explicit rejection.
    pub(super) fn release_rejected_create(&self, key: &SyncKey) -> Result<(), StateError> {
        self.change(
            "DELETE FROM bindings WHERE sync_key=?1 AND status='intent'",
            key,
        )
    }

    pub(super) fn record_manual_candidates(
        &self,
        matches: &[(SyncKey, String)],
    ) -> Result<(), StateError> {
        if matches.is_empty() {
            return Ok(());
        }
        // Additive to Phase 3C state. Candidate history is separate from bindings.
        let tx = self
            .0
            .unchecked_transaction()
            .map_err(|_| StateError::Database)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS manual_candidates (
                sync_key TEXT NOT NULL, page_id TEXT NOT NULL,
                PRIMARY KEY(sync_key,page_id)
            ) STRICT;",
        )
        .map_err(|_| StateError::Database)?;
        for (key, page) in matches {
            tx.execute(
                "INSERT INTO manual_candidates VALUES (?1,?2) ON CONFLICT(sync_key,page_id) DO NOTHING",
                params![key.as_str(), page],
            ).map_err(|_| StateError::Database)?;
        }
        tx.commit().map_err(|_| StateError::Database)
    }

    pub fn mark_reconcile(&self, key: &SyncKey) -> Result<(), StateError> {
        self.change(
            "UPDATE bindings SET status='reconcile' WHERE sync_key=?1",
            key,
        )
    }

    /// Persist an independently verified match. Moving an established binding to
    /// a different page requires a separate recovery procedure.
    pub fn confirm(&self, key: &SyncKey, page: &str) -> Result<(), StateError> {
        let page = uuid(page).ok_or(StateError::InvalidTransition)?;
        changed_one(self.0.execute(
            "INSERT INTO bindings(sync_key,page_id,status) VALUES (?1,?2,'linked')
             ON CONFLICT(sync_key) DO UPDATE SET page_id=excluded.page_id,status='linked'
             WHERE bindings.page_id IS NULL OR bindings.page_id=excluded.page_id",
            params![key.as_str(), page],
        ))
    }

    fn change(&self, sql: &str, key: &SyncKey) -> Result<(), StateError> {
        changed_one(self.0.execute(sql, [key.as_str()]))
    }

    pub fn record_progress(
        &self,
        source: UniversitySource,
        fetched: Option<DateTime<Utc>>,
        applied: Option<DateTime<Utc>>,
    ) -> Result<(), StateError> {
        let source = match source {
            UniversitySource::Moodle => "moodle",
            UniversitySource::WebClass => "webclass",
        };
        self.0
            .execute(
                "INSERT INTO progress VALUES (?1,?2,?3) ON CONFLICT(source) DO UPDATE SET
               fetched_at=CASE WHEN fetched_at IS NULL THEN excluded.fetched_at
                  ELSE max(fetched_at,coalesce(excluded.fetched_at,fetched_at)) END,
               applied_at=CASE WHEN applied_at IS NULL THEN excluded.applied_at
                  ELSE max(applied_at,coalesce(excluded.applied_at,applied_at)) END",
                params![
                    source,
                    fetched.map(|v| v.timestamp()),
                    applied.map(|v| v.timestamp())
                ],
            )
            .map(|_| ())
            .map_err(|_| StateError::Database)
    }

    pub fn record_run(
        &self,
        at: DateTime<Utc>,
        outcome: RunOutcome,
        issue: Option<RunIssue>,
        deferred: usize,
    ) -> Result<(), StateError> {
        let deferred = i64::try_from(deferred).map_err(|_| StateError::InvalidTransition)?;
        self.0.execute(
            "INSERT INTO last_run VALUES (1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET
                finished_at=excluded.finished_at,outcome=excluded.outcome,issue=excluded.issue,deferred=excluded.deferred",
            params![at.timestamp(), outcome.code(), issue.map(RunIssue::code), deferred],
        ).map(|_| ()).map_err(|_| StateError::Database)
    }

    pub(super) fn summary(&self) -> Result<StateSummary, StateError> {
        let mut stmt = self
            .0
            .prepare("SELECT sync_key, page_id, status FROM bindings")
            .map_err(|_| StateError::InvalidDatabase)?;
        let mut rows = stmt.query([]).map_err(|_| StateError::InvalidDatabase)?;
        let mut bindings = HashMap::new();
        let mut pending = 0;
        while let Some(row) = rows.next().map_err(|_| StateError::InvalidDatabase)? {
            let key: String = row.get(0).map_err(|_| StateError::InvalidDatabase)?;
            let page: Option<String> = row.get(1).map_err(|_| StateError::InvalidDatabase)?;
            let status: String = row.get(2).map_err(|_| StateError::InvalidDatabase)?;
            let binding = decode_binding(&status, page)?;
            pending += usize::from(binding.status != BindingStatus::Linked);
            bindings.insert(
                key.parse().map_err(|_| StateError::InvalidDatabase)?,
                binding,
            );
        }
        let (fetch, apply): (Option<i64>, Option<i64>) = self
            .0
            .query_row(
                "SELECT max(fetched_at),max(applied_at) FROM progress",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| StateError::InvalidDatabase)?;
        let time = |stamp: Option<i64>| {
            stamp
                .map(|stamp| DateTime::from_timestamp(stamp, 0).ok_or(StateError::InvalidDatabase))
                .transpose()
        };
        let last_run = self
            .0
            .query_row(
                "SELECT finished_at,outcome,issue,deferred FROM last_run WHERE singleton=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| StateError::InvalidDatabase)?
            .map(|(finished_at, outcome, issue, deferred)| {
                Ok(StateRun {
                    finished_at: DateTime::from_timestamp(finished_at, 0)
                        .ok_or(StateError::InvalidDatabase)?,
                    outcome: RunOutcome::parse(&outcome)?,
                    issue: issue.as_deref().map(RunIssue::parse).transpose()?,
                    deferred: usize::try_from(deferred).map_err(|_| StateError::InvalidDatabase)?,
                })
            })
            .transpose()?;
        Ok(StateSummary {
            bindings,
            pending,
            last_fetch: time(fetch)?,
            last_apply: time(apply)?,
            last_run,
        })
    }
}

fn decode_binding(status: &str, page_id: Option<String>) -> Result<Binding, StateError> {
    let status = BindingStatus::parse(status)?;
    if page_id
        .as_deref()
        .is_some_and(|page| uuid(page).as_deref() != Some(page))
        || (status == BindingStatus::Linked && page_id.is_none())
        || (matches!(status, BindingStatus::CreateIntent | BindingStatus::Unknown)
            && page_id.is_some())
    {
        return Err(StateError::InvalidDatabase);
    }
    Ok(Binding { status, page_id })
}

fn changed_one(result: rusqlite::Result<usize>) -> Result<(), StateError> {
    match result {
        Ok(1) => Ok(()),
        Ok(_) => Err(StateError::InvalidTransition),
        Err(_) => Err(StateError::Database),
    }
}

fn exists(path: &Path) -> Result<bool, StateError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(StateError::InvalidPath),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StateError::Io),
    }
}

// Check ownership and rollback mode before SQLite can perform recovery or
// create a WAL sidecar, even if a foreign database was placed at this path.
fn check_header(path: &Path) -> Result<(), StateError> {
    let mut header = [0; 100];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|_| StateError::InvalidDatabase)?;
    if &header[..16] != b"SQLite format 3\0"
        || header[18..20] != [1, 1]
        || header[60..64] != (VERSION as u32).to_be_bytes()
        || header[68..72] != (APPLICATION_ID as u32).to_be_bytes()
    {
        return Err(StateError::InvalidDatabase);
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn test_state_dir() -> tempfile::TempDir {
    let root = std::env::temp_dir();
    // The default macOS temp directory is reached through the /var symlink.
    #[cfg(target_os = "macos")]
    let root = root.canonicalize().unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::SqliteStore;

    const TARGET: &str = "00000000-0000-4000-8000-000000000001";
    const PAGE: &str = "00000000-0000-4000-8000-000000000002";
    const OTHER: &str = "00000000-0000-4000-8000-000000000003";

    fn key(char: char) -> SyncKey {
        format!("v1:{}", char.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    #[test]
    fn fresh_state_is_atomic_and_foreign_databases_remain_unchanged() {
        let root = test_state_dir();
        let path = root.path().join("state.sqlite3");
        assert!(StateStore::read(&path, TARGET).unwrap().is_none());
        assert!(!path.exists());
        StateStore::initialize(&path, TARGET).unwrap();
        let original = fs::read(&path).unwrap();
        StateStore::initialize(&path, TARGET).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(matches!(
            StateStore::read(&path, OTHER),
            Err(StateError::InvalidDatabase)
        ));

        let legacy = path.with_file_name("assignment-manager.db");
        drop(SqliteStore::open(&legacy).unwrap());
        let old_db = fs::read(&legacy).unwrap();
        assert_eq!(
            StateStore::initialize(&legacy, TARGET),
            Err(StateError::InvalidDatabase)
        );
        assert!(matches!(
            StateStore::for_apply(&legacy, TARGET),
            Err(StateError::InvalidDatabase)
        ));
        assert_eq!(fs::read(&legacy).unwrap(), old_db);
    }

    #[test]
    fn intents_unknown_and_linked_pages_remain_reserved_after_restart() {
        let root = test_state_dir();
        let path = root.path().join("state.sqlite3");
        StateStore::initialize(&path, TARGET).unwrap();
        let (a, b) = (key('a'), key('b'));
        let store = StateStore::for_apply(&path, TARGET).unwrap();
        assert!(store.reserve_create(&a).unwrap());
        assert!(store.reserve_create(&b).unwrap());
        store.mark_unknown(&a).unwrap();
        drop(store);

        let store = StateStore::for_apply(&path, TARGET).unwrap();
        assert!(!store.reserve_create(&a).unwrap());
        assert!(!store.reserve_create(&b).unwrap());
        assert_eq!(
            store.binding(&a).unwrap().unwrap().status,
            BindingStatus::Unknown
        );
        assert_eq!(
            store.binding(&b).unwrap().unwrap().status,
            BindingStatus::CreateIntent
        );
        store.mark_reconcile(&a).unwrap();
        store.confirm(&a, PAGE).unwrap();
        assert_eq!(store.confirm(&a, OTHER), Err(StateError::InvalidTransition));
        assert_eq!(store.confirm(&b, PAGE), Err(StateError::Database));
        assert_eq!(
            store.binding(&a).unwrap().unwrap().page_id.as_deref(),
            Some(PAGE)
        );
        assert_eq!(store.mark_unknown(&a), Err(StateError::InvalidTransition));
        let summary = StateStore::read(&path, TARGET).unwrap().unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.bindings.len(), 2);
        assert_eq!(summary.bindings[&a].status, BindingStatus::Linked);
        assert_eq!(summary.bindings[&b].status, BindingStatus::CreateIntent);
    }

    #[test]
    fn progress_and_fixed_run_issue_roundtrip_without_regressing_timestamps() {
        let root = test_state_dir();
        let path = root.path().join("state.sqlite3");
        StateStore::initialize(&path, TARGET).unwrap();
        let state = StateStore::for_apply(&path, TARGET).unwrap();
        let now = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let before = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        state
            .record_progress(UniversitySource::Moodle, Some(now), None)
            .unwrap();
        state
            .record_progress(UniversitySource::Moodle, Some(before), Some(before))
            .unwrap();
        state
            .record_run(now, RunOutcome::Partial, Some(RunIssue::Reconciliation), 2)
            .unwrap();
        drop(state);
        let data = StateStore::read(&path, TARGET).unwrap().unwrap();
        assert_eq!(data.last_fetch, Some(now));
        assert_eq!(data.last_apply, Some(before));
        let run = data.last_run.unwrap();
        assert_eq!(run.finished_at, now);
        assert_eq!(run.outcome, RunOutcome::Partial);
        assert_eq!(run.issue, Some(RunIssue::Reconciliation));
        assert_eq!(run.deferred, 2);
    }
}
