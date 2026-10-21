# Vanilla frontend integration (Web-3B)

## Entrypoints and transport

| Consumer | HTML → entrypoint | Injected adapter | Completion |
| --- | --- | --- | --- |
| Windows Tauri | frontend/index.html → app.js | tauri_api.js | existing toggle_assignment_completion invoke |
| Local Browser | `/` (frontend/web.html) → web_app.js → web_auth.js | http_api.js | authenticated PUT desired state + CSRF |

Both call `createListView(api)` in list_view.js. There is no runtime transport
detection or HTTP fallback. Only tauri_api.js references window.__TAURI__.
settings_view.js and desktop sync continue using the same invoke function.
The Web module graph imports the shared settings.js presentation/clearing helpers,
but never desktop settings_view.js, sync.js or tauri_api.js. Those desktop modules
remain absent from the HTTP asset allowlist. No framework, npm dependency, bundler,
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
same-origin mode, same-origin credentials, no-store, redirect rejection and a 10-second
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
behind active local operations. Sync exists only in the desktop entrypoint. Web Settings has its own HTTP lifecycle.

Multiple tabs can set the same state idempotently; opposing writes follow DB commit
order. Tabs do not auto-refresh each other and there is no conflict-version API.
Use reload to observe changes made in another tab. No optimistic state is persisted.

## Serving and validation

crates/web-server/src/assets.rs embeds only the Web entry HTML and nine required
CSS/JS assets. favicon.ico is an empty 204. There is no filesystem route or directory
traversal resolution. Assets must be rebuilt into the server after editing.
The server applies the localhost boundary to static and API requests alike.

Run from the repository root:

```powershell
node --test tests/*.test.mjs
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
```

The Node standard test runner tests the real adapters/controller and a minimal DOM
seam for literal title rendering/checkbox intent. Browser smoke additionally verifies
the actual DOM, requests and persistence. See [Phase report](PHASE-WEB-3B.md).

## Web authentication lifecycle

web_auth.js checks GET /api/v1/session before constructing the list view. A 401
shows the login form; successful POST /api/v1/auth/login sets the HttpOnly Cookie
and returns safe session information. The adapter retains only the CSRF token in
memory. The password goes into the transient JSON request, and the input clears
before awaiting its response; no browser storage is used. Password buffers inside
the browser/network engine cannot be guaranteed to be erased by JavaScript.

PUT and logout send X-CSRF-Token plus X-Assignment-Manager: web-3a and JSON.
Login uses the same non-simple header/JSON/Origin policy without a session token.
The browser sets Origin and Fetch Metadata itself. No UserId/tenant/DB path is
accepted as a client ownership selector. The server resolves the Cookie owner.

Logout immediately disposes the view, clears rows, Course options/counts/errors,
and invalidates adapter and model generations. Old GET/body/PUT responses cannot
restore data or initiate post-write reconciliation after logout. View disposal
also aborts its control event listeners; Tauri never calls this auth lifecycle.
401 and general 403 responses similarly return to login. Reauth-specific 403 keeps the session and requests fresh proof. A failed logout keeps the list empty,
blocks another login and offers a server-confirmed logout retry. Back/forward cache
restores check the session again before showing assignments.

Idle/absolute expiration is enforced by the server. On the next API request the
UI clears expired data; it has no background keepalive. Other tabs do not yet
receive a logout broadcast and revalidate on their next request/reload. This PoC
supports one account and is not approved for public multi-user use.

## Web Settings lifecycle (Web-3B)

Navigation disposes the previous view/listeners and constructs web_settings.js only
for an authenticated session. Desktop settings.js/settings_view.js/tauri_api.js keep
their invoke contract without Web reauthentication. Shared configuredPresentation
and clearSecretInputs are reused. No frontend encryption is added.

Web settings state contains booleans, busy/outcome classifications and fixed text.
The adapter sends fixed same-origin URLs plus the existing JSON/custom header/CSRF
proof. GET status accepts exactly two boolean fields. PUT/DELETE/reauth expect 204;
status is a separate GET, and its failure never changes an acknowledged save into
a failed save. Unknown network/5xx/response outcomes trigger only a safe status read.
Boolean configured status cannot prove whether an existing secret was replaced.
The UI preserves this uncertainty and never automatically resends a write.

Reauth uses the Web account password and is enforced by the server for five minutes
on that session. Input clears at send start, after an active operation, navigation,
visibility change, pagehide and logout. It is never repopulated. Autocomplete is off
for token/university/reauth inputs, with password fields for token URL/passwords.
JavaScript cannot guarantee destruction of engine/network/password-manager copies.

Generation and active-view checks suppress stale status/write/reauth results after
disposal, including automatic follow-up GETs and old finally blocks clearing a new
view's inputs. Pending operations reject duplicate submission. The server remains
the authority for proof expiry even when a view still shows the previous proof label.
