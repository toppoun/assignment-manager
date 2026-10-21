# Vanilla frontend integration (Web-2)

## Entrypoints and transport

| Consumer | HTML → entrypoint | Injected adapter | Completion |
| --- | --- | --- | --- |
| Windows Tauri | frontend/index.html → app.js | tauri_api.js | existing toggle_assignment_completion invoke |
| Local Browser | `/` (frontend/web.html) → web_app.js | http_api.js | explicit PUT desired state |

Both call `createListView(api)` in list_view.js. There is no runtime transport
detection or HTTP fallback. Only tauri_api.js references window.__TAURI__.
settings_view.js and desktop sync continue using the same invoke function.
The Web module graph never imports desktop settings/sync/adapters, and those files
are absent from the HTTP asset allowlist. No framework, npm dependency, bundler,
CDN, service worker or browser storage was added.

## Reused presentation and queries

The former app.js DOM view was moved into list_view.js, not copied for each shell.
It still builds textContent nodes, groups by CourseId, distinguishes same-label
courses by origin, retains server order within each group, renders all submission
targets, counts, empty states and fixed errors, and restores focus after updates.
presentation.js still owns control defaults, typed query construction and labels.
Filter/sort and badge membership remain in the Rust application/core.

The HTTP adapter serializes the typed query to completion/deadline/sort/courseMode
and optional courseId via URLSearchParams. It uses fixed relative paths,
same-origin mode, omitted credentials, no-store, redirect rejection and a 10-second
AbortSignal timeout. A timeout ends the browser's wait; it is not a server rollback.
Responses must be JSON and match the safe list/completion shape. Arbitrary errors,
response bodies and internal messages never enter UI error text.

Web DateTime labels use response.timeZone (`Asia/Tokyo`); DateOnly labels format
the calendar string directly. Desktop DateTime retains its OS local display.
Neither entrypoint computes deadline categories in JS.

## Controller and write recovery

list_tasks.js retains createListTaskRunner and adds createAssignmentListModel.
One tab serializes reads/writes. Each reload immediately records the selected
controls and advances a generation. Superseded queued reads are skipped; responses
and errors from old generations never replace the current view. Controls and the
reload button remain usable during I/O; a post-write GET uses the latest selection.

The view captures desired completion from the checkbox, then restores its displayed
committed value. The controller marks the mutation pending synchronously, disables
checkboxes, and accepts no duplicate or competing write until reconciliation ends.
All checkboxes are temporarily blocked (a conservative choice for this small PoC).

| Outcome | Behavior |
| --- | --- |
| PUT acknowledged, GET succeeds | Render fresh server list; enable checkboxes |
| PUT acknowledged, GET fails | State explicitly says saved; keep stale rows disabled; reload recovers |
| Explicit HTTP 4xx rejection | Fixed rejection message; reload required before further writes |
| Network/timeout/body/shape error, or 5xx | State is unknown; query an unfiltered snapshot to find actual completion, then reload the current filter |
| Unknown result cannot be verified | Never claim unsaved or send a reverse toggle; keep checkboxes disabled across failed reloads |

The desktop adapter preserves its existing toggle command and definite command
error behavior. It has no HTTP retry capability. Source sync refreshes still queue
behind active local operations. Settings/Sync exist only in the desktop entrypoint.

Multiple tabs can set the same state idempotently; opposing writes follow DB commit
order. Tabs do not auto-refresh each other and there is no conflict-version API.
Use reload to observe changes made in another tab. No optimistic state is persisted.

## Serving and validation

crates/web-server/src/assets.rs embeds only the Web entry HTML and six required
CSS/JS assets. favicon.ico is an empty 204. There is no filesystem route or directory
traversal resolution. Assets must be rebuilt into the server after editing.
The server applies the localhost boundary to static and API requests alike.

Run from the repository root:

```powershell
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs tests/web_frontend.test.mjs
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
```

The Node standard test runner tests the real adapters/controller and a minimal DOM
seam for literal title rendering/checkbox intent. Browser smoke additionally verifies
the actual DOM, requests and persistence. See [Phase report](PHASE-WEB-2.md).
