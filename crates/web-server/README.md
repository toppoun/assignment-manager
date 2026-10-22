# Web-4A: asynchronous Sync API + offline mock integration

合成データ専用・Web専用password認証のPoCです。
**127.0.0.1:3000だけにbindします。外部公開、LAN、reverse proxy、tunnel運用は禁止です。**
本番HTTPS、reverse proxy trust、deployment secret管理は未実装です。
Cookie policyはHTTP loopback専用で、production/public flagやbind/DB pathの選択機能はありません。
本Phaseはmock統合までです。公開運用・実大学接続を許可する判定ではありません。

## Bootstrap / start

serverを停止した状態でrepository rootから実行します。

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked -- bootstrap-admin
```

Web専用login ID (ASCII英数字/._-、1–64 bytes)を入力し、大学とは別のpassword
(15–256 bytes、制御文字なし)を非表示入力で2回入力します。passwordをCLI引数、
env file、sourceへ保存しないでください。公開signup・default account・auth bypassはありません。
既存accountはaccount_existsで拒否し、transaction失敗で不完全なaccountを残しません。
account未作成のserver起動はbootstrap_requiredで停止します。

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline -- init-vault-key
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline
```

[Browser UI](http://127.0.0.1:3000/)からログインします。Ctrl+Cで停止します。
CLIは引数なし、bootstrap-admin、init-vault-keyだけを許可。control DBのexclusive OS file lockで
複数serverや稼働中bootstrapを拒否します。localhostと127.0.0.1は別Cookie hostです。
Host/portとOriginの完全一致を要求し、CORS許可はありません。

```powershell
Get-NetTCPConnection -LocalPort 3000 -State Listen |
    Select-Object LocalAddress, LocalPort, OwningProcess
```

LocalAddressが127.0.0.1だけであることを確認してください。

## Storage / isolation

固定directoryはcrates/web-server/target/web-pocです。

| File | Responsibility |
| --- | --- |
| assignment-manager.db | 既存synthetic課題DB、core schema v3のまま |
| SYNTHETIC-ONLY | 既存PoC所有marker、再seed禁止 |
| web-control.db | 独立Web control schema v2: account/session/rate limit + vault binding/暗号化record/nonce履歴 |
| web-control.lock | server/bootstrapのexclusive OS lock。fileの存在だけではlock判定しない |

初回だけ11課題をseedします。marker不整合、symlink/reparse point、foreign/unknown schema、
破損DBはfail closedです。既存課題DBを変更してWeb tableを追加しません。
認証後のUserIdがimmutable TenantRuntime.ownerと一致してから、1つのApplicationHandleへ
query/setを送ります。request単位で3 workerを作らず、DB pathをrequestから選びません。
WebVaultがshared CredentialStoreを実装し、MemorySettings（startup sync OFF）とDisabledNetworkBackendを維持します。
大学HTTP clientは構築せず、大学ログイン/fetchとstartup syncは行いません。手動Sync APIはDisabledNetworkBackendを使います。

## Authentication / session API

| Method / path | Behavior |
| --- | --- |
| POST /api/v1/auth/login | JSON login/passwordを検証、新規Cookie + safe session JSON |
| POST /api/v1/auth/reauth | JSON password（Web専用）+ CSRF、Argon2id verify、成功204 |
| GET /api/v1/settings/status | 認証必須、moodleConfigured/webclassConfigured booleanだけ |
| PUT /api/v1/settings/moodle | JSON calendarUrl、recent reauth + CSRF、commit後204 |
| DELETE /api/v1/settings/moodle | JSON {}、recent reauth + CSRF、成功204（未設定も同じ） |
| PUT /api/v1/settings/webclass | JSON universityId/passwordを一組で保存、recent reauth + CSRF、204 |
| DELETE /api/v1/settings/webclass | JSON {}、recent reauth + CSRF、成功204（未設定も同じ） |
| GET /api/v1/session | authenticated/userId/csrfToken/expiresAt/absoluteExpiresAt |
| POST /api/v1/auth/logout | JSON {} + CSRF、server record削除、Cookie Max-Age=0 |
| GET /api/v1/assignments | 認証されたownerの一覧 |
| PUT /api/v1/assignments/{id}/completion | 認証されたownerの明示completion設定 + CSRF |
| GET /health/live | 認証不要、secretなしのalive status |

未認証/無効/期限切れは401 authentication_failed。unknown ID・wrong password・disabled accountの
login失敗は同じ401です。有効sessionのaccount無効化・tenant owner不一致は403。
JSONはpassword verifier、raw session token、server secretを返しません。
session時刻はUnix seconds。課題APIのevaluatedAtは従来どおりUTC RFC3339です。

Cookieはam_loopback_session、256-bit OS乱数、HttpOnly、SameSite=Strict、Path=/、
Max-Age=43200、Domainなし。**HTTP loopback専用のためSecureなし**です。
将来の本番HTTPSは別policyで__Host- prefix + Secure + Path=/ + Domainなしを必須とします。
現状は本番配信経路自体がなく、Secureを外すproduction switchもありません。

idle 30分、absolute 12時間をserverで検証。API活動でidleを更新しますがabsoluteは固定。
loginでCookie/CSRFを新規発行し、提示された旧sessionを同一transactionで失効させます。
logout後のreplayは401。reloadはCookieを利用可能ですが、server再起動では全sessionを
削除し再ログインを要求します。account、課題、completion、暗号化credentialは保持します。
loginとreauthは共通で全ID合計5回/60秒 (成功・失敗を含む)、同時hash検証1件、超過は429 + Retry-After: 60。
limit timestampも再起動で消しません。password reset/changeは今回未実装です。

## CSRF / request boundaries

login・logout・reauth・Sync POST・Settings PUT/DELETE・completion PUTには実Hostと完全一致するOrigin、X-Assignment-Manager: web-3a、
application/json (任意のcharset=utf-8だけを許可)が必要です。
Fetch MetadataがあればSite=same-origin、Mode=same-origin/corsだけです。
logout・reauth・Sync POST・Settings PUT/DELETE・completion PUTにはさらにsessionに束縛したX-CSRF-Token (constant-time比較)が必要です。
loginはOrigin/JSON/custom header/Metadataでlogin CSRFを防御します。
CLIはMetadata省略可能ですが、Origin/custom header/JSON/認証・CSRFは必須です。
GET APIはsame-origin、または直接navigationのnoneを許可し、same-site/cross-site/不明値を拒否。
mutationのnoneは拒否します。旧web-2 headerだけでは許可しません。

Settings bodyは4096 bytes、その他のmutationは1024 bytes上限、queryは2048 bytes。
Moodle URLは2048 bytes、大学IDは256 bytes、大学passwordは1024 bytesまで。不明・重複field、不正enum/JSON/IDを拒否します。
raw error/body/path/SQL/secretをlogやresponseへ出しません。全responseにno-store、nosniff、
CSP、no-referrer、X-Frame-Options: DENY、CORP: same-originを設定します。

## Assignment API (Web-2 contract retained)

| Query | Values / default |
| --- | --- |
| completion | all / incomplete (default) / completed |
| deadline | all (default) / overdue / today / within_seven_days |
| sort | deadline (default) / course / title |
| courseMode | all (default) / unassigned / course |
| courseId | courseMode=courseの場合だけ、空でないopaque CourseId |

responseはitems/courses/totalCount/evaluatedAt/timeZone。DateOnlyはcalendar string、
DateTimeはUTC RFC3339小数秒を維持。Asia/Tokyoの期限分類/sortはserver/applicationが計算します。
raw submission URL/source identityは返しません。

認証済みPUT bodyは {"completion":"completed"} または {"completion":"incomplete"}。
成功200は{id,completion}。SQLite commitを待って返します。同じPUTの再送は反転せず、
同状態のupdated_atも不変。unknown IDは404。PUT応答喪失時はUIが未保存と断定せず、
全件GETで照合してから操作を再開します。

## Browser / lifecycle

web_auth.jsがsessionを確認し、login成功後だけ共通list viewを作成します。
CookieはJSへ返さず、CSRFだけmemory保持。logout/401/一般403でrows/Course options/countsとsecret入力を消去し、
古いGET/PUTからの復活を防止。失敗したlogoutは一覧を隠したまま再試行できます。
passwordのbrowser storageはありません。titleはtextContentで描画します。

compile-time allowlistは/、styles.css、web_app.js、web_auth.js、http_api.js、list_view.js、
list_tasks.js、presentation.js、web_settings.js、settings.js、web_sync.js、sync.jsだけ。faviconは204。filesystem serving/desktop transport・viewの配信はありません。sync.jsの共通presentation関数だけを再利用します。
asset変更後はrebuildが必要。startupはowner設定検証→実query/readiness→bind。
shutdownはHTTP/job admission停止→HTTP drain→job/Settings mutation drain→application shutdown→control DB/lock close。

## Verification

```powershell
cargo fmt --manifest-path crates/web-server/Cargo.toml --check
cargo clippy --manifest-path crates/web-server/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
node --test tests/*.test.mjs
```

[認証設計](../../docs/WEB-AUTHENTICATION.md)、[Web-3B検証報告](../../docs/PHASE-WEB-3B.md)、[Vault設計](../../docs/WEB-CREDENTIAL-VAULT.md)、
[frontend統合](../../docs/FRONTEND-INTEGRATION.md)を参照してください。
WindowsでRust/Tauri buildを確認し、macOS native/XcodeはWindowsでは未検証です。

## Key management / recovery boundary

`init-vault-key`は明示操作だけで鍵を作成します。Windows保護ストアのserviceは
`org.assignmentmanager.web.vault.keys.v1`、accountはinstallation pathのSHA-256です。
Local persistenceを使い、大学ID/token/passwordをOS metadataに入れません。
control DBの暗号化key-check recordで鍵ID/実鍵を照合します。欠落・不一致・破損はfail closedです。
鍵が消えた既存Vaultを新規鍵で初期化する機能や暗号化を迂回するfallbackはありません。

通常testはmemory鍵だけです。実Windows providerの隔離テストは明示的に実行します。

```powershell
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline windows_key_persists -- --ignored
```

このtestは一時directory専用のOS entryだけを作成・検証・削除します。
Browser smokeには通常環境と別の固定`target/web-3b-browser-smoke`を使うexampleがあります。

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline --example vault_browser_smoke -- bootstrap
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline --example vault_browser_smoke -- init-vault-key
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline --example vault_browser_smoke -- serve
```

login IDは`synthetic-smoke`、Web passwordはbootstrap時の非表示入力です。既存accountは上書きしません。
normal serverと同時起動せず、同じ127.0.0.1:3000で合成値だけを入力してください。

Linux/VPS provider、鍵rotation/re-encryption CLI、鍵のbackup/restore、ACLの運用設計は未実装です。
鍵とciphertextを同じbackupへ入れないでください。path移動ではOS entryのinstallation identityが変わるため、
現状のままdirectoryを移動するrestore手順は提供していません。実大学通信の前にはegress/SSRF制御が必要です。


## Asynchronous manual sync (Web-4A)

| Method / path | Contract |
| --- | --- |
| POST /api/v1/sync | JSON `{}`、Idempotency-Key必須、202 `{runId,state,statusUrl}` |
| GET /api/v1/sync/status | `{running,activeRunId,lastRun}`、同期を開始しない |
| GET /api/v1/sync/runs/{runId} | running / finished + report / failed + safe error、未知runは404 |

すべてsessionとTenantRuntime ownerを検証します。POSTにはCSRFと既存Origin/Host/Metadata/JSON/custom headerを要求。
通常同期にrecent reauthは不要です。source resultはnot_configured / success(counts) / failed(error code)。
`finished`は全source成功の意味ではなく、`shouldReload`は部分失敗・全失敗でもtrueになり得ます。
worker/report喪失はstate=failed、job_failed、shouldReload=trueです。防御的なalready_runningはshared should_reload()を維持してfalseです。

WebSyncCoordinatorはowner単位の短いmemory admission lockで、同期jobとcredential save/deleteを排他にします。
同期中の設定変更は409 sync_running、設定mutation中の新規同期は503 busy。Settingsの受付済み操作もserver taskがreplyまで保持します。
同userのactive jobは1件。別keyの重複要求も同じactive runへ対応し、queueを作りません。
新規runは5件/60秒。Idempotency-KeyはASCII英数字と `._-` の1–128文字、ownerごとのSHA-256だけmemoryに保存。
最大128key・128履歴、keyは受付から1時間（実行中は保留）、結果は完了から1時間まで。live keyを容量のために追い出さず429で拒否。
同key再送は元のrunを返し、完了済みの再送では202のstateもfinished/failedになります。各POST bodyは常に同じ空objectです。
単一processの保証です。restart・期限後のdedup保証はなく、過去runは404、lastRun=null。partial SQLite commitは保持します。

通常binaryはfeatureの有無に関係なくDisabledNetworkBackendです。実大学への切替flag/envはありません。
`synthetic-sync` featureの専用test/exampleだけがMockSyncBackendを明示注入します。
mockは公開synthetic定数以外を拒否し、coreのvalidated batch/import/updateを使います。HTTP client/authenticatorは構築しません。

```powershell
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline --all-features
cargo run --manifest-path crates/web-server/Cargo.toml --features synthetic-sync --example sync_browser_smoke -- bootstrap
cargo run --manifest-path crates/web-server/Cargo.toml --features synthetic-sync --example sync_browser_smoke -- init-vault-key
cargo run --manifest-path crates/web-server/Cargo.toml --features synthetic-sync --example sync_browser_smoke -- serve both-success
```

Windows専用の固定 `target/web-4a-browser-smoke` とその保護鍵だけを使います。通常DB/Vaultを削除・初期化しません。
scenarioはboth-success / moodle-failure / webclass-partial / both-failure / auth-expired / paced。
pacedは各sourceを10秒待機させるmanual専用modeで、実行中表示・Settings409・複数tabを再現します。自動race testsはchannelを使います。変更には停止後に別scenarioで再起動します。
Web loginはsynthetic-smoke、passwordはbootstrapで入力した値。
設定にはMoodle `https://example.invalid/synthetic-calendar`、大学ID `synthetic-student`、password `synthetic-university-password`だけを使ってください。
これらは公開されたmock専用値で、大学への認証能力を持ちません。

portableなUI確認だけなら、専用exampleの `ephemeral webclass-partial` を使用できます。
新しい一時DBとmemory鍵を毎回生成し、Web loginはsynthetic-smoke / synthetic-web-only-password-4aです。
終了時に一時環境を破棄し、再起動でcredentialは引き継ぎません。通常serverへのfallbackや永続Vaultの検証には使いません。

Browserは1.5秒間隔・最大120回だけrunning runをpollします。10秒HTTP timeoutとserver完了を区別し、結果不明なら状態再確認を案内。
通信断で自動的に別runを始めず、lost POSTは利用者が同期ボタンを押したときだけ同keyで再確認します。
Settings遷移でも監視を維持し、logout/新session/disposeで古い結果とtimerを破棄します。
完了後は現在のfilter/sortを維持して一覧を再取得し、completion pendingと同じlist queueで調停します。

今回の検証結果、Windows未実施項目と公開前の未実装境界は[Phase Web-4A](../../docs/PHASE-WEB-4A.md)を参照してください。


## Secure university transport preparation (Web-4B1)

Mac上でcore HTTP層へ明示Web constructor、exact origin/path policy、DNS-to-connect制御、
proxy/redirect/retry制限、bounded body読取を追加しました。通常binaryは引き続き
DisabledNetworkBackendで、all-featuresでも実大学通信へ切り替わりません。
既存synthetic-syncのMockSyncBackend / WebSyncCoordinatorは維持します。

Moodle実hostname/pathは未確定のため未設定で拒否します。WebClassの3 originsは既存codeで
確認した値で、現在の大学稼働確認ではありません。policyのenv/CLI設定やhidden switchはありません。
SettingsがURLの保存を受理しても、それは通信先の承認ではありません。

[Phase Web-4B1](../../docs/PHASE-WEB-4B1.md)、
[大学HTTP security設計](../../docs/WEB-NETWORK-SECURITY.md)を参照してください。
Web-4B1はMac/offline transport gate。Web-4B2でbackend内部の統合とoffline検証を追加しました。
Web-4AのWindows build / Browser smokeは引き続き未完了で、Web-4A最終判定はBLOCKEDです。


## Secured backend integration (Web-4B2)

共有applicationに`SecuredCoreSyncBackend::new(NetworkPolicy)`を追加しました。
通常binaryは引き続きDisabledNetworkBackend固定で、CLI/envから有効化できません。
MockSyncBackendのexampleも維持します。新しいsecured成功経路はcore内部の合成HTTPS/CA/DNSと
既存SSO→parser→SQLiteで検証し、Web側は明示注入した実backendのpolicy拒否を認証API/Vault/
coordinator/worker経由で検証します。任意CA/DNSを公開するproduction設定はありません。

全runの時間・request/bytes・course/group/batch・Cookie上限、協調的deadlineの限界、
Web-4B3の未実行手順は[Phase Web-4B2](../../docs/PHASE-WEB-4B2.md)を参照してください。
判定はReady for Web-4B3（Mac/offline統合のみ）。大学実通信・公開運用の許可ではありません。
Web-4AのWindows未検証・最終BLOCKEDは維持します。
