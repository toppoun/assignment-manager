# Web-3A: local authentication + Browser HTTP PoC

合成データ専用・Web専用password認証のPoCです。
**127.0.0.1:3000だけにbindします。外部公開、LAN、reverse proxy、tunnel運用は禁止です。**
本番HTTPS、reverse proxy trust、deployment secret管理は未実装です。
Cookie policyはHTTP loopback専用で、production/public flagやbind/DB pathの選択機能はありません。
Ready for Web-3Bは次Phaseへの着手判定であり、公開運用の許可ではありません。

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
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline
```

[Browser UI](http://127.0.0.1:3000/)からログインします。Ctrl+Cで停止します。
CLIは引数なしまたはbootstrap-adminだけを許可。control DBのexclusive OS file lockで
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
| web-control.db | 独立Web control schema v1: accounts / sessions / login_attempts |
| web-control.lock | server/bootstrapのexclusive OS lock。fileの存在だけではlock判定しない |

初回だけ11課題をseedします。marker不整合、symlink/reparse point、foreign/unknown schema、
破損DBはfail closedです。既存課題DBを変更してWeb tableを追加しません。
認証後のUserIdがimmutable TenantRuntime.ownerと一致してから、1つのApplicationHandleへ
query/setを送ります。request単位で3 workerを作らず、DB pathをrequestから選びません。
NoCredentials / MemorySettings / DisabledNetworkBackendを維持し、大学HTTP client、
real Keychain、大学設定、startup sync、sync/settings endpointはありません。

## Authentication / session API

| Method / path | Behavior |
| --- | --- |
| POST /api/v1/auth/login | JSON login/passwordを検証、新規Cookie + safe session JSON |
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
削除し再ログインを要求します。account、課題、completionは保持します。
loginは全ID合計5回/60秒 (成功・失敗を含む)、同時hash検証1件、超過は429 + Retry-After: 60。
limit timestampも再起動で消しません。password reset/changeは今回未実装です。

## CSRF / request boundaries

login・logout・PUTには実Hostと完全一致するOrigin、X-Assignment-Manager: web-3a、
application/json (任意のcharset=utf-8だけを許可)が必要です。
Fetch MetadataがあればSite=same-origin、Mode=same-origin/corsだけです。
logout・PUTにはさらにsessionに束縛したX-CSRF-Token (constant-time比較)が必要です。
loginはOrigin/JSON/custom header/Metadataでlogin CSRFを防御します。
CLIはMetadata省略可能ですが、Origin/custom header/JSON/認証・CSRFは必須です。
GET APIはsame-origin、または直接navigationのnoneを許可し、same-site/cross-site/不明値を拒否。
mutationのnoneは拒否します。旧web-2 headerだけでは許可しません。

全bodyは1024 bytes上限、queryは2048 bytes。不明・重複field、不正enum/JSON/IDを拒否します。
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
CookieはJSへ返さず、CSRFだけmemory保持。logout/401/403でrows/Course options/countsを消去し、
古いGET/PUTからの復活を防止。失敗したlogoutは一覧を隠したまま再試行できます。
passwordのbrowser storageはありません。titleはtextContentで描画します。

compile-time allowlistは/、styles.css、web_app.js、web_auth.js、http_api.js、list_view.js、
list_tasks.js、presentation.jsだけ。faviconは204。filesystem serving/desktop module配信はありません。
asset変更後はrebuildが必要。startupはowner設定検証→実query/readiness→bind。
shutdownはadmission停止→HTTP drain→application shutdown→control DB/lock close。

## Verification

```powershell
cargo fmt --manifest-path crates/web-server/Cargo.toml --check
cargo clippy --manifest-path crates/web-server/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs tests/web_frontend.test.mjs
```

[認証設計](../../docs/WEB-AUTHENTICATION.md)、[Web-3A検証報告](../../docs/PHASE-WEB-3A.md)、
[frontend統合](../../docs/FRONTEND-INTEGRATION.md)を参照してください。
WindowsでRust/Tauri buildを確認し、macOS native/XcodeはWindowsでは未検証です。
