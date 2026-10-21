# Phase Web-1: Rust HTTP API Local PoC

検証日: 2026-10-21（Asia/Tokyo）。作業branch: feature/web-migration。

**Ready for Web-2。** 合成SQLite→shared ApplicationHandle→Axum→JSONの経路をWindows上で確認した。
今回の成果物は127.0.0.1限定・未認証のread-only PoCであり、Web-2実装やインターネット公開は行っていない。

## Changed files

| File | 変更 |
| --- | --- |
| [runtime.rs](../crates/application/src/runtime.rs) | query_assignments_with_clock(AssignmentQuery, QueryClock)を加算。既存query_assignmentsのsignature・desktop local-clock動作は維持。 |
| [local_worker.rs](../crates/application/src/local_worker.rs) | List requestへoptional clockを追加。Noneなら従来のLocal::now、Someならcallerのclockを使い同じlist_from_storeを呼ぶ。 |
| [runtime_tests.rs](../crates/application/src/runtime_tests.rs) | supplied calendar/instantがworkerまで届くこと、既存queryとshutdown contractの保持を確認。 |
| [application README](../crates/application/README.md) | explicit clockとtransport責務を記載。 |
| [.gitignore](../.gitignore) | crates/web-server/targetだけを追加。runtime DB、合成marker、build成果物をcommit対象にしない。 |
| [README](../README.md) | Web-1への入口と制約を追加。 |

新規: crates/web-serverのCargo.toml / Cargo.lock / README.md、src/main.rs / lib.rs / state.rs / routes.rs / requests.rs / dto.rs / error.rs / poc.rs、tests/http_poc.rs、本報告書。
root core、root manifest/lockfile、application manifest/lockfile、Tauri/bridge/frontend source、SQLite schema、macOS native sourceは変更していない。

## New crate architecture

```text
GET /api/v1/assignments
  → Axum Router / loopback Host-Origin boundary
  → Web ListRequest → shared AssignmentQuery
  → PocState（server lifetimeの1 ApplicationHandle）
  → local worker → synthetic SqliteStore v3
  → AssignmentListResponse → Web AssignmentListDto → JSON
```

独立manifest/lockfileのcrateとし、root workspace化やcore移動を行っていない。
production dependencyはexisting core/application、Axum 0.8.9、Tokio、serde、chrono、tempfile。
tempfileは初回seedをtemporary SQLiteで完成後publishするために使用する。
test-only dependencyはserde_json、tower util、Tokioのtest用IO/time features。
Tauri/UniFFI/native credential backendに依存しない。
AxumのQuery/State/serve/graceful shutdownは[公式API](https://docs.rs/axum/0.8.9/axum/)を確認して使用した。

## ApplicationHandle integration

[PocState::open](../crates/web-server/src/state.rs)でopen_with_backendを一度呼び、ready stateのcloneを全requestで共有する。
handlerはConnectionやworkerを取得せず、requestごとのopenもしない。

起動順序はPocDatabase::prepare→ApplicationHandle::open_with_backend→実queryをawaitするreadiness→127.0.0.1 bind。
readinessが失敗した場合はapplication.shutdown().await後に固定errorで終了し、listenしない。
bind失敗時にもopen済みruntimeをshutdownする。

settingsはmemoryのdefault OFF、credential portは両sourceとも未設定しか返さずwrite/deleteを拒否する。
DisabledNetworkBackendは全operationを固定failureで拒否し、HTTP clientの構築やcredential callbackを行わない。
startup syncやsync routeを呼ぶコードはない。

Ctrl+CはHTTP admissionを閉じ、Axumのin-flight requestsが完了してからapplication.shutdown().awaitを呼ぶ。
accepted requestをbarrierで保留するtestで、新規requestの503、accepted requestの200、worker shutdownの順序を確認した。
workerのblocking joinは追加していない。

## Clock handling

Web queryごとにserver側Clock::nowを一度sampleし、UTC instantを+09:00へ投影してQueryClock::from_localを作る。
同じinstantからevaluatedAtも生成する。clientがnow/local_date/utc_offset/timeZoneを指定するqueryは400。

Web→query_assignments_with_clock→local worker→既存queryの経路なのでfilter/sort/badgeの独自実装はない。
DateOnlyはcalendar date、DateTimeはUTC instantのまま。TodayはJST当日、WithinSevenDaysは
DateTimeで168時間・DateOnlyで今日から7日後までinclusiveという既存仕様を維持する。
既存query_assignmentsはclock=None経路を使い、Tauri/SwiftUIの挙動を変更しない。

固定UTC clockでJST午夜の前後、締切ちょうど、今日末尾、翌日開始、168時間端点、その1ns後、DateOnlyの7日後、
Completedのbadge除外を検証した。advanceするclockを使い、readinessと各GETでsampleが1回ずつであること、
evaluatedAtと実際のfilter/badgeが同じclockに基づくことを確認した。OS timezoneを変更する操作は行っていない。

## Synthetic DB

binaryの保存先はcompile時のcrate directory配下、crates/web-server/target/web-poc/assignment-manager.db。
HTTP/CLI/envから任意のDBや保存directoryを選ぶ機能はない。testsだけがisolated temporary directoryを渡す。
symlink/reparse pointを拒否し、既存directoryには正しいSYNTHETIC-ONLY markerとregular DBを要求する。
markerなし/不一致/壊れたDBは上書き・削除・silent resetせず停止する。

初回はtemporary DBをcore APIでseedし、全操作の成功後にpersist_noclobberでpublishしmarkerを書き込む。
初期化中断でmarkerがないdirectoryもfail closedとする。再起動ではseedしない。

fixtureは課題11件（Incomplete 10、Completed 1）、同名で異なるID/originのCourse 2件（Moodle/WebClass相当）、
未所属課題3件、日本語title、DateOnly/DateTime/期限なしを含む。固定合成日付なので実行日によりbadgeは変化する。
synthetic submission URL / source markerはDBにだけ存在し、DTOで公開しない。
HTTPにcompletion writeはなく、保持testだけが停止済みの合成DBへcore set_completionを使う。

## API contract

| Method / path | Response |
| --- | --- |
| GET /api/v1/assignments | items / courses / totalCount / evaluatedAt / timeZone |
| GET /health/live | {"status":"alive"} |

completion=all/incomplete/completed、deadline=all/overdue/today/within_seven_days、sort=deadline/course/title、
courseMode=all/unassigned/course、courseId（course時だけ必須）を扱う。
省略値はAssignmentQuery::defaultから継承し、unknown/duplicate/invalid enum/無効Course条件は400 invalid_query。
courseIdは空白のみ・制御文字・256 bytes超を拒否し、それ以外はopaque IDをそのままapplicationへ渡す。
存在しないIDのfilterは空itemsになるがtotalCountとCourse候補を維持する。query全体上限は2048 bytes。

他pathは404、GET以外は405。body APIはなく、予期しないGET bodyも400 invalid_request。
malformed percent encodingやserde Query rejectionは固定codeへ変換し入力値を返さない。
busy/unavailableは503（Retry-After: 1）、内部storage失敗は500 internal_error。

## JSON DTO

[dto.rs](../crates/web-server/src/dto.rs)はsafe application modelからのみ変換する。
core Assignmentの直接SerializeやTauri DTOへの依存はない。
Assignment fieldはid/title/deadline/completion/submission/hasSubmissionUrl/course/statusに限定する。
Courseはid/displayLabel/origin、WebClass tagはweb_classで既存Tauri表現と揃える。

deadlineはdate_only / date_time / noneのtagged enum。
DateTimeはUTC RFC3339 Z、SecondsFormat::AutoSiで必要な小数秒を保持し、nanosecondsをtestする。
source identity、raw submission URL、大学secret、DB pathを含めない。
errorも {"error":{"code":"..."}} の固定表現のみでcause/stack trace/raw inputを保持しない。

## Security boundary

- listener factoryはIpv4Addr::LOCALHOST固定。binaryは3000固定で引数なし。public bind optionなし。
- Hostは127.0.0.1またはlocalhostと実portの完全一致。複数Hostや他host/portは400。
- Originがあれば実Hostのhttp://originと完全一致した1値だけ許可し、その他を403。cross-site Fetch Metadataも拒否。
- CORSを許可せず、全responseへno-store / nosniffを追加。GET mutationなし。
- 実university URL、実ID/password、OS Keychain/Credential Manager、desktop SQLiteへアクセスしない。
- initial settings/credential adapterはmemoryの未設定のみ。DisabledNetworkBackendにはHTTP処理が存在しない。

この境界は認証の代替ではなく、合成データ専用local PoCの制限である。
同じPCのlocal processからのアクセスを認証する機能はない。外部proxy/tunnel/公開運用は対象外。

## Automated tests

| 対象 | 結果 |
| --- | --- |
| Web unit | 3 PASS：固定error mapping、inert ports/backendのnetwork不実行、deterministic graceful drain |
| Web HTTP integration | 10 PASS：defaults/DTO/filters/sort/Course identity、JST/precision/single clock、query validation、Host/Origin/body/method制限、readiness failure、永続化/reseed防止、実loopback listener/shutdown |
| Web doctest | 0件 |
| Web fmt / clippy all-targets all-features -D warnings | PASS |

network backend testはloopback tripwireにも接続せず、credential providerも呼ばないことを確認する。
大学通信はbackend構造とroute不在でも禁止しており、testsは実大学をprobeしない。
queue saturationをsleepのタイミングで再現するtestは追加せず、Busy/Unavailable→503を固定mappingで検証した。

## Existing regressions

| 対象 | Windowsでの結果 |
| --- | --- |
| Rust core unit | 312 PASS |
| Rust core doctests（明示--doc実行も含む） | 11 PASS |
| Shared application unit / doctests | 31 / 2 PASS（explicit-clock testを1件追加） |
| Tauri unit | 18 PASS |
| macOS bridge Rust unit / doctests | 18 PASS / 0件 |
| frontend Node tests | 30 PASS |
| root / application / web / Tauri / bridge fmt --check | 全てPASS |
| 上記5対象のclippy --all-targets --all-features -- -D warnings | 全てPASS |

既存lockfileは維持し、全Cargo test/clippyを--locked --offlineで確認した。
Axum依存と既存固定UniFFI cache不足の取得時だけcargo fetchを実行し、その後はofflineを使用した。
初回の制限環境ではcore HTTP clientのmock test、一部のTauri/Poc temporary-file操作が失敗したため、
通常Windows権限で対象testsを再実行してPASSを確認した。制限を回避するproductionコード変更はしていない。
incremental cacheのhard-link警告は検証process内のCARGO_INCREMENTAL=0で回避した。

## Windows build results

Rust/Cargo 1.97.1、Windows PowerShell環境でWeb binaryのcompile/run、
Tauri custom-protocol build、bridgeのRust側tests/clippyが成功した。
Webの実listenはGet-NetTCPConnectionで {LocalAddress:"127.0.0.1",LocalPort:3000} と確認した。

macOS Xcode / SwiftUI app / Swift BridgeSmoke / 実Keychain smokeはWindowsのため未実施。
Rust bridgeのPASSをnative macOS buildのPASSとは扱わない。Windows Tauri UI実機操作も今回の対象外。

## Manual smoke procedure

起動・Browser/PowerShell/curl・listen確認の再現手順は[crate README](../crates/web-server/README.md)に記載した。
今回、以下を実施した。

1. cargo run --manifest-path crates/web-server/Cargo.toml --locked --offlineで起動。
2. in-app BrowserでGET assignmentsのJSON表示を確認。PowerShellでもall/title取得、totalCount=11、timeZone=Asia/Tokyoを確認。
3. curlでhealth 200、Host偽装400、cross-origin 403、duplicate query 400、no-store/nosniffを確認。
4. Get-NetTCPConnectionで127.0.0.1:3000だけのlistenを確認。
5. Ctrl+C停止後にlisten件数0を確認し、同じcommandで再起動。
6. all/title responseの再比較でTotalBefore=11 / TotalAfter=11、AssignmentIdChanges=0、CourseIdChanges=0、Completed=1を確認。
7. 再度Ctrl+Cで停止。確認用serverは常駐させない。

## Remaining limitations

認証、per-user registry、暗号化credential保管、SSRF対策を伴う実大学sync、frontend/PWA、remote phoneアクセスは未実装。
PoCはsingle runtime / single process / fixed port / compile時crate配下の専用DBであり、production packagingではない。
合成日付は固定であり、実行日のToday表示が常に同じ件数になる仕様ではない。
不完全な初回DB作成を自動repairする機能はない。unknown directoryを上書きする操作もない。

**Ready for Web-2** — 指定された2つのGET、shared clock seam、synthetic persistence、security boundary、readiness/graceful shutdown、
Windows build/HTTP smoke、既存consumer regressionが確認できた。次に進めるのは引き続きlocal synthetic dataを使うBrowser frontend接続であり、
公開運用ではない。今回はWeb-2以降を実装していない。
