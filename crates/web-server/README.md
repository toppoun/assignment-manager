# Web-2 local Browser UI + HTTP PoC

既存shared applicationを利用する、合成データ専用・未認証のHTTP PoCです。
**127.0.0.1:3000だけにlistenします。外部公開、reverse proxy、tunnel接続は行わないでください。**
同一originのvanilla HTML/CSS/JavaScriptで一覧・filter/sort・完了変更を利用できます。
認証、credential保管、大学同期、公開運用、PWAは含みません。

## 起動

repository rootからWindows PowerShellで:

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked
```

初回dependency取得後は `--offline` を追加できます。Ctrl+Cで停止します。
CLI引数は受け付けません。host/bind/DBの選択や環境変数によるpublic bindはありません。
port 3000が使用中なら固定code `unavailable` で終了します。

Browser UI:

[http://127.0.0.1:3000/](http://127.0.0.1:3000/)

画面には「合成データを使ったローカルデモ」と表示し、Settings/Sync操作はありません。
JSONも確認できます:

[http://127.0.0.1:3000/api/v1/assignments](http://127.0.0.1:3000/api/v1/assignments)

PowerShell:

```powershell
$base = 'http://127.0.0.1:3000'
Invoke-RestMethod "$base/health/live"
$snapshot = Invoke-RestMethod "$base/api/v1/assignments?completion=all&sort=title"
$snapshot | Select-Object totalCount, evaluatedAt, timeZone
$snapshot.items | Select-Object title, completion, deadline, status
$snapshot.courses | Format-Table
Invoke-RestMethod "$base/api/v1/assignments?courseMode=unassigned"
```

curl（PowerShellではaliasを避けてcurl.exe）:

```powershell
curl.exe --noproxy '*' -i 'http://127.0.0.1:3000/health/live'
curl.exe --noproxy '*' -i 'http://127.0.0.1:3000/api/v1/assignments?deadline=today'
curl.exe --noproxy '*' -i 'http://127.0.0.1:3000/api/v1/assignments?completion=all&completion=all'
curl.exe --noproxy '*' -i -H 'Host: invalid.example:3000' 'http://127.0.0.1:3000/api/v1/assignments'
curl.exe --noproxy '*' -i -H 'Origin: https://invalid.example' 'http://127.0.0.1:3000/api/v1/assignments'
```

最後の3件は400 invalid_query、400 invalid_host、403 cross_originを返します。
成功・エラーともCache-Control: no-store、X-Content-Type-Options: nosniffを付けます。
Access-Control-Allow-Origin等のCORS許可headerは返しません。

Windowsでlisten先を確認:

```powershell
Get-NetTCPConnection -LocalPort 3000 -State Listen |
    Select-Object LocalAddress, LocalPort, OwningProcess
```

LocalAddressが127.0.0.1だけであることを確認します。0.0.0.0、LAN IP、::にはbindしません。
localhostのHostも同じportで許可しますが、Originがある場合は実際のHostと完全一致した
http://originだけを許可します。127.0.0.1とlocalhostを跨ぐOriginも拒否します。

## Synthetic SQLite

保存先はこのcrateの `target/web-poc/assignment-manager.db` です。
desktop app_data_dir、macOS Application Support、OS credential storeを参照しません。
新規directoryだけをseedし、再起動時はSYNTHETIC-ONLY markerとregular DBを検証します。
foreign directory、marker不整合、symlink/reparse point、壊れたDBは安全なerrorで停止し、
自動上書き・再seed・削除はしません。初期作成中断でmarkerがない場合も同様です。

11課題、同名のMoodle/WebClass相当Course 2件、未所属3件、完了済み1件を含みます。
初期defaultの未完了一覧は10件、totalCountは11です。Course IDsは初回にcoreが生成し、
再起動で保持します。締切は2026-10-21〜29付近の固定合成日付なので、実行日によりbadgeが変わります。
synthetic submission URLとsource markerはDBにだけ存在し、JSONには返しません。

Browserのcheckboxで変更→reload→Ctrl+C→同じcommandで起動→完了済みfilterで、
状態とitems/coursesのID・件数が保持されることを確認できます。DBを選ぶHTTP/CLI機能はありません。

## API contract

GET /api/v1/assignments、GET /health/live、PUT /api/v1/assignments/{id}/completionです。
GETはrequest bodyを受け付けません。completion routeのGET等は固定405です。
static allowlist以外のpathは固定404で、HTTP toggle/settings/sync endpointはありません。

| Query | 許可値 | 省略時 |
| --- | --- | --- |
| completion | all / incomplete / completed | shared default: incomplete |
| deadline | all / overdue / today / within_seven_days | shared default: all |
| sort | deadline / course / title | shared default: deadline |
| courseMode | all / unassigned / course | shared default: all |
| courseId | courseMode=courseの時だけ必須、空白のみ/制御文字不可、256 bytes以内 | なし |

unknown/duplicate parameter、enum不正、矛盾するCourse条件、過大query、不正percent encodingは400です。
now/local_date/utc_offset/timeZone等をclientから指定するAPIはありません。
存在しないopaque Course IDは正常な空結果で、totalCountとCourse候補は保持します。

responseはitems / courses / totalCount / evaluatedAt / timeZone。
Assignmentはid / title / deadline / completion / submission / hasSubmissionUrl / course / status。
deadlineは `{kind:"date_only",value:"YYYY-MM-DD"}`、UTC RFC3339のdate_time、noneを区別します。
必要な小数秒を保持し、DateOnlyをUTC midnightに変換しません。WebClass tagは既存Tauri同様web_classです。
source identity、raw submission URL、credential、DB pathは返しません。
errorは `{"error":{"code":"invalid_query"}}` 等の固定codeだけです。
busy/unavailableは503 + Retry-After、内部storage失敗は500です。

### Completion PUT

```http
PUT /api/v1/assignments/{id}/completion
Origin: http://127.0.0.1:3000
Content-Type: application/json
X-Assignment-Manager: web-2

{"completion":"completed"}
```

`incomplete`も指定できます。成功は200 + `{"id":"…","completion":"completed"}`。
coreの明示setを呼び、SQLite commit後に応答します。同状態の再PUTは反転せずupdated_atも不変です。
unknown field/重複field/不正JSON/不正値/空白・制御文字等の不正IDは400、存在しないIDは404、
Content-Type不正は415、1024 bytes超のbodyは413です。Content-Lengthのないbodyにも上限を適用します。
JSONはapplication/json、任意の`; charset=utf-8`だけを許可します。

PowerShellでの合成データ操作例:

```powershell
$base = 'http://127.0.0.1:3000'
$snapshot = Invoke-RestMethod "$base/api/v1/assignments?completion=all"
$id = [uri]::EscapeDataString($snapshot.items[0].id)
$headers = @{ Origin = $base; 'X-Assignment-Manager' = 'web-2' }
Invoke-RestMethod "$base/api/v1/assignments/$id/completion" -Method Put `
    -ContentType 'application/json' -Headers $headers -Body '{"completion":"completed"}'
```

PUTには実Hostと完全一致するOriginと専用headerが必須です。
Fetch Metadataがある場合、Sec-Fetch-Siteはsame-originだけ、Sec-Fetch-Modeはsame-origin/corsだけを許可します。
CLIはFetch Metadataを省略できますが、Origin・専用header・JSONは必須です。
これらは未認証loopback PoC向けのCSRF対策で、本番の認証・authorizationの代替にはなりません。

### Static frontend

`/`→frontend/web.html、styles.css、web_app.js、http_api.js、list_view.js、list_tasks.js、presentation.js
だけをinclude_bytes!で埋め込みます。favicon.icoは204です。runtime filesystem readやdirectory listingはありません。
Tauri entrypoint/settings/sync、DB、source/config/lockfileは配信しません。asset変更後はserverを再buildしてください。
HTML/CSS/JSにはUTF-8の正しいContent-Typeを付け、全応答にno-store、nosniff、CSP、no-referrer、
frame拒否、CORP same-originを設定します。外部CDNへの依存はありません。

## Architecture / lifecycle

```text
Browser → same-origin GET/PUT → Axum transport DTO
  → one PocState / ApplicationHandle → local worker → SQLite v3
  → safe application read model → Web JSON DTO
```

shared applicationのquery_assignments_with_clockと新しいset_completionを利用します。既存query_assignmentsは
Local::now()を使う従来のままで、Tauri/SwiftUIの時計を変更しません。
WebではserverがUTC instantを一度だけsampleし、+09:00のQueryClockを生成します。
evaluatedAtも同じsampleで、server OS timezoneに依存しません。filter/sort/badgeは全てshared/core側です。
Web表示もresponse.timeZone=Asia/TokyoをIntlへ指定します。DateOnlyは文字列のcalendar dateのまま表示します。
共通のlist view/controllerへadapterを注入し、desktopは既存invoke/toggle、Webはfetch/PUTを選びます。
自動transport fallbackはありません。[frontend統合説明](../../docs/FRONTEND-INTEGRATION.md)を参照してください。

checkbox操作は希望状態を決定→二重操作防止→PUT→現在の条件でGETの順です。
保存成功と再取得失敗を分け、PUT応答喪失時は未保存と断定せず、全件GETで実際の状態を確認します。
確認できるまでcheckboxを停止し、再読み込みで復旧します。filter変更は通信中も可能で、古いGETは破棄します。

startupはsynthetic DB検証→open_with_backend→実queryによるreadiness→loopback bind。
initializerはmemory settingsと未設定credential portだけで、DisabledNetworkBackendは全操作を拒否します。
startup syncや大学HTTP clientを起動する経路はありません。
Ctrl+CはHTTP受付停止→既存HTTP request drain→ApplicationHandle.shutdown().await→終了です。
async threadでworkerをblocking joinしません。

## Checks

```powershell
cargo fmt --manifest-path crates/web-server/Cargo.toml --check
cargo clippy --manifest-path crates/web-server/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs tests/web_frontend.test.mjs
```

testsはisolated temporary DBとloopback socketだけを使い、実credentialや大学serverへ接続しません。
clockはserver-side test seamで固定できますが、HTTPからのclock入力はありません。
[Web-2実装・検証報告](../../docs/PHASE-WEB-2.md)に実測結果と制限を記載します。
Web-1の履歴は[Web-1報告](../../docs/WEB-1-IMPLEMENTATION.md)にあります。
AxumのState/Query/serveは[公式documentation](https://docs.rs/axum/0.8.9/axum/)を参照しています。
