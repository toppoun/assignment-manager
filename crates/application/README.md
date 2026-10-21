# Shared application layer

`assignment-manager-application` depends on the existing root Rust core. The core
never depends on this crate. Tauri, native credential implementations, settings
encoding, UI state, localized messages and Swift/FFI dependencies stay outside.

## Handle and lifetime

Own one `ApplicationHandle` per application. Clones share the same workers,
startup gate, sync status and session. Do not open a handle per command or view.

`open(database_path, initialize_settings)` starts three workers. The shell supplies
an absolute DB path and a `Send` initializer that constructs its `SettingsStore`
and `CredentialStore` implementations on the settings worker. Ports are used on
that worker only; a future native Keychain adapter must not block on the UI thread.
Initialization failures are retained. A settings/credential failure does not
disable offline assignment queries or completion writes.

- Local worker: one `SqliteStore`, query/completion requests, bounded queue of 32.
- Sync worker: a separate `SqliteStore`, source coordinator and in-memory session.
- Settings worker: non-secret settings, credential ports and WebClass revision;
  bounded queue of 8. Sync loads fresh secrets through this worker when needed.

`begin_shutdown()` rejects new requests immediately across all clones. Accepted
requests drain. The sync worker retains its settings sender until it finishes, so
reauthentication can still access credentials during shutdown. `shutdown().await`
observes worker-owned resource destruction through a completion signal; it never
joins a thread on the caller. Dropping the final handle also closes admission and
lets workers exit. Tauri prevents exit while asynchronous cleanup finishes.

There is no cancellation API. Dropping a response future does not cancel an
admitted operation, reset single-flight early, or undo a DB commit.

## Use cases

- `query_assignments(AssignmentQuery)` -> `AssignmentListResponse`
- `query_assignments_with_clock(AssignmentQuery, QueryClock)` -> same response,
  using one trusted caller-supplied clock (Web/server adapters and deterministic tests).
  Transport clients must not supply raw clock/calendar fields. The original query
  API retains its local-worker `Local::now()` behavior for desktop consumers.
- `toggle_completion(internal_id)` -> committed `CompletionState`
- `set_completion(internal_id, desired: CompletionState)` -> committed `CompletionState`.
  Explicit set on the local worker calls `SqliteStore::set_completion` directly;
  identical state preserves `updated_at`. It never emulates set through read/toggle.
  Missing IDs return `NotFound`; invalid IDs return `InvalidAssignmentId`.
- `get_settings_status()` -> configured flags and startup setting only
- save/remove Moodle configuration and WebClass credentials
- `set_sync_on_startup(bool)`
- `sync_now()` -> source-specific `SyncReport`, or `AlreadyRunning`
- `get_sync_status()` -> running flag and last report, kept in memory only
- `run_startup_sync()` -> `Option<SyncReport>`
- `begin_shutdown()`, `is_shutdown_complete()`, `shutdown().await`

Call startup after the initial local view is ready. The application checks policy
once, even if disabled, unconfigured, or settings access fails. Later settings
changes do not schedule another startup run. Concurrent manual/startup requests
use the same single-flight admission; extra runs are not queued.

`open_with_backend` / `SyncBackend` provide the existing synthetic transport test
seam, including the separate Tauri smoke executable. They are not frontend APIs
and should not be exported by a future Swift bridge.

## Data and errors

A query loads the validated core read model once. Existing core filter/sort rules
are applied in memory to this snapshot, with one sampled clock for all projections.
Items, course options, total count and deadline badges cannot mix separate commits.
Application read models contain no raw submission/token URL or source identity.
Transport adapters define wire tags, date serialization and localized error messages.

`toggle_completion` acknowledges the committed state only. If a later list refresh
fails, the toggle remains successful; callers must not retry it as a failed write.
`set_completion` likewise acknowledges commit independently of a later read. Losing
its response does not undo the operation. HTTP consumers can repeat the same desired
state safely and reconcile with a fresh query. Desktop toggle and source sync's
user-owned completion contract remain unchanged; no schema migration is involved.

Sources run independently. `Completed` means the run ended, not universal success.
`Failed` does **not** mean unchanged DB: earlier WebClass groups may be committed.
`SyncReport::should_reload()` includes source failures. Reload current local data
after those results, including when all attempted sources failed.

Credentials have no Debug, Clone or serialization implementation. Owned buffers
zeroize on drop, but this is not a guarantee about copies inside HTTP, OS, Swift or
future FFI runtimes. Settings/read models/errors/sync status never contain secrets.
Settings validation and revision changes happen in the shared service; successful
WebClass save/delete invalidates the cached session before the next sync. Revision
is process-local. Session cookies are never persisted.

## Future changes

Academic-term selection can resolve to date ranges in application query options,
intersecting existing filters. Source submission state can extend the source-owned
domain/import/read model separately from user completion. Course schedule remains
unchanged. WidgetKit should consume a separate safe snapshot, not this live DB.
No Swift, FFI, academic-term, timetable or widget implementation is included here.
