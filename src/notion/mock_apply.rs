//! Loopback-only prototype for crash-safe creation. Production has no mutation dispatch.

use reqwest::Method;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    NotionClient, NotionError, PreparedWrite, SyncKey,
    wire::{Schema, checked_page_id},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CreateOutcome {
    Created,
    Confirmed,
    Conflict,
    Deferred,
}

#[derive(Debug)]
pub(super) enum ApplyError {
    Journal,
    Read(NotionError),
}

pub(super) fn initialize(db: &Connection) -> Result<(), ApplyError> {
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| ApplyError::Journal)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS notion_creation (
            data_source TEXT NOT NULL,
            sync_key TEXT NOT NULL,
            page_id TEXT,
            PRIMARY KEY (data_source, sync_key)
        ) STRICT;",
    )
    .map_err(|_| ApplyError::Journal)
}

pub(super) fn create_once(
    client: &NotionClient,
    schema: &Schema,
    db: &Connection,
    request: &PreparedWrite,
) -> Result<CreateOutcome, ApplyError> {
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.path(), "/v1/pages");
    assert_eq!(
        request.body()["parent"]["data_source_id"],
        schema.assignments_id
    );
    // Autocommit makes the intent durable before the outbound request.
    if !db.is_autocommit() {
        return Err(ApplyError::Journal);
    }
    let key = request.key();
    let stored: Option<Option<String>> = db
        .query_row(
            "SELECT page_id FROM notion_creation WHERE data_source=?1 AND sync_key=?2",
            params![schema.assignments_id, key.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ApplyError::Journal)?;

    if let Some(Some(known)) = &stored {
        match client.retrieve_page(known, &[&schema.properties.sync_key]) {
            Ok(page) if verified_id(schema, &page, key).as_deref() == Some(known) => {}
            Ok(_) | Err(NotionError::NotFound) => return Ok(CreateOutcome::Deferred),
            Err(error) => return Err(ApplyError::Read(error)),
        }
    }

    // Even a new intent requires a complete, exact-key query before creation.
    let matched = client
        .query_assignments(schema, Some(key))
        .map_err(ApplyError::Read)?;
    match matched.as_slice() {
        [row] => {
            if let Some(Some(known)) = &stored
                && *known != row.page_id
            {
                return Ok(CreateOutcome::Conflict);
            }
            bind(db, schema, key, &row.page_id)?;
            return Ok(CreateOutcome::Confirmed);
        }
        [] if stored.is_some() => return Ok(CreateOutcome::Deferred),
        [] => {}
        _ => return Ok(CreateOutcome::Conflict),
    }

    let inserted = db
        .execute(
            "INSERT INTO notion_creation (data_source,sync_key) VALUES (?1,?2)
         ON CONFLICT DO NOTHING",
            params![schema.assignments_id, key.as_str()],
        )
        .map_err(|_| ApplyError::Journal)?;
    if inserted != 1 {
        return Ok(CreateOutcome::Deferred);
    }

    // All ambiguous results retain the intent. This mock never repeats a write
    // on a transport error, malformed success, HTTP error or missing search result.
    let Ok(page) = client.send_mock_write(request) else {
        return Ok(CreateOutcome::Deferred);
    };
    let Some(id) = verified_id(schema, &page, key) else {
        return Ok(CreateOutcome::Deferred);
    };
    bind(db, schema, key, &id)?;
    Ok(CreateOutcome::Created)
}

fn verified_id(schema: &Schema, page: &serde_json::Value, key: &SyncKey) -> Option<String> {
    let id = checked_page_id(page, &schema.assignments_id, &schema.assignments_database).ok()?;
    (schema.page_key(page).ok().flatten().as_ref() == Some(key)).then_some(id)
}

fn bind(db: &Connection, schema: &Schema, key: &SyncKey, id: &str) -> Result<(), ApplyError> {
    let count = db
        .execute(
            "INSERT INTO notion_creation (data_source, sync_key, page_id) VALUES (?1,?2,?3)
         ON CONFLICT (data_source,sync_key) DO UPDATE SET page_id=excluded.page_id
         WHERE page_id IS NULL OR page_id=excluded.page_id",
            params![schema.assignments_id, key.as_str(), id],
        )
        .map_err(|_| ApplyError::Journal)?;
    (count == 1).then_some(()).ok_or(ApplyError::Journal)
}
