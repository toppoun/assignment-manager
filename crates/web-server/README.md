# Web-1 local HTTP PoC

既存shared applicationを利用する、合成データ専用・未認証のHTTP PoCです。
**127.0.0.1:3000だけにlistenします。外部公開、reverse proxy、tunnel接続は行わないでください。**
Web-2のHTML接続、完了変更API、認証、大学同期、PWAは含みません。

## 起動

repository rootからWindows PowerShellで:

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked
```

初回dependency取得後は `--offline` を追加できます。Ctrl+Cで停止します。
CLI引数は受け付けません。host/bind/DBの選択や環境変数によるpublic bindはありません。
port 3000が使用中なら固定code `unavailable` で終了します。

Browserで次を開くとJSONを確認できます:

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

再起動smokeはcompletion=allのJSONを取得→Ctrl+C→同じcommandで起動→再取得し、
items/coursesのIDと件数が変わらないことを確認します。DBを選ぶHTTP/CLI機能はありません。

## API contract

GET /api/v1/assignmentsとGET /health/liveだけです。他pathは404、GET以外は405です。
request bodyは受け付けず、JSON bodyの不正入力も固定400 invalid_requestになります。

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

## Architecture / lifecycle

```text
HTTP GET → Axum Query DTO → AssignmentQuery
  → one PocState / ApplicationHandle → local worker → SQLite v3
  → safe application read model → Web JSON DTO
```

shared applicationへquery_assignments_with_clockを追加しました。既存query_assignmentsは
Local::now()を使う従来のままで、Tauri/SwiftUIの時計を変更しません。
WebではserverがUTC instantを一度だけsampleし、+09:00のQueryClockを生成します。
evaluatedAtも同じsampleで、server OS timezoneに依存しません。filter/sort/badgeは全てshared/core側です。

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
```

testsはisolated temporary DBとloopback socketだけを使い、実credentialや大学serverへ接続しません。
clockはserver-side test seamで固定できますが、HTTPからのclock入力はありません。
[実装・検証報告](../../docs/WEB-1-IMPLEMENTATION.md)に実測結果と制限を記載します。
AxumのState/Query/serveは[公式documentation](https://docs.rs/axum/0.8.9/axum/)を参照しています。
