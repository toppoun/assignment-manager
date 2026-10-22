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

## Web credential adapter (Web-3B)

The Web consumer uses the existing ApplicationHandle settings methods and
SettingsWorker with an owner-bound encrypted CredentialStore. Validation,
`default.course-related`/CourseRelated identity and WebClass credential revision
updates remain in SettingsService; Web HTTP does not implement a second service.
Successful credential save/delete changes revision; a failed persistence operation
does not. Existing SyncService drops old cached sessions before the next run with
a new revision. Web-3B uses DisabledNetworkBackend and never establishes a session
or runs startup/manual sync. Session-bound reauthentication is Web transport policy
and is not imposed on desktop/UniFFI. See [Web vault](../../docs/WEB-CREDENTIAL-VAULT.md).


## Web sync adapter (Web-4A)

WebSyncCoordinator keeps a server-owned task awaiting the existing `sync_now()`;
HTTP handlers never await the long sync directly. The shared SyncWorker, source
results, revision/session invalidation and `should_reload()` are unchanged.
The Web owner-bound admission boundary also excludes credential mutations. A
mutation's guard stays in a detached task until the SettingsWorker reply, even
when its HTTP request disappears. A status precheck alone would be insufficient.
Do not call `begin_shutdown()` on the application before accepted Web jobs and
mutations drain: those jobs still need the settings sender for fresh credentials.
Completion uses its separate worker and core IMMEDIATE transactions; imports
preserve the user-owned completion value in either commit order.

The ordinary Web runtime always injects DisabledNetworkBackend. Only the isolated
synthetic test/example runtime injects MockSyncBackend. It simulates transport and
retry outcomes and calls the existing core import/update path; it does not validate
real SSO, cookies or university protocol compatibility. No shared public API,
worker cancellation API, core network path or desktop policy changed.


## University transport boundary (Web-4B1)

Core now provides explicit `MoodleClient::for_web(NetworkPolicy)` and
`HoseiAuthenticator::for_web(NetworkPolicy)` constructors. They enforce Web egress
and bounded responses while retaining the existing parsers, authentication flow,
and sync functions. Desktop constructors remain available. Shared body reads are
bounded in both modes; the application orchestration and its APIs are unchanged.

`ApplicationHandle::open` keeps the desktop factory. Explicit trusted Rust callers
can inject `SecuredCoreSyncBackend::new(NetworkPolicy)` using `open_with_backend`.
The common CoreSyncBackend implementation uses a private factory, so Moodle,
initial WebClass login and reauth all select the same secured policy. No policy
argument is optional and a missing Moodle exact host/path fails closed.

SyncService calls the backend's default-no-op `begin_run` once before both sources.
The secured factory resets shared Web-only resource accounting while retaining the
cached session and cookie jar. Credential revision still controls session invalidation;
policy changes require draining and replacing the runtime. Sessions from another
factory/runtime are rejected. Existing sync/import/completion implementations are reused.

Ordinary Web remains DisabledNetworkBackend; MockSyncBackend stays in isolated
examples. The core's synthetic CA/DNS seam stays cfg(test)-only; transport E2E and
application/Web boundary tests are intentionally split. See
[Web-4B2](../../docs/PHASE-WEB-4B2.md) for limits, cancellation gaps and evidence.
Web-4A Windows validation remains incomplete and its final status remains BLOCKED.
