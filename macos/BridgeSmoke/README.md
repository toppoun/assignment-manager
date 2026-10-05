# Phase 10A: macOS Swift bridge PoC

Swift processから共有Rust applicationをopen/query/toggle/shutdownする検証用SwiftPM packageです。
正式なSwiftUI shell、Settings、native Keychain、Sync、academic term filterは含みません。

## UniFFIと公式仕様の確認

2026-10-05時点のstable **UniFFI 0.32.2**を`=0.32.2`で固定しています。
runtimeとbindgenは同じCargo dependency/lockfileからbuildします。

導入前に確認した一次資料:

- [0.32.2 package/version](https://docs.rs/crate/uniffi/0.32.2)
- [Swift bindings / Swift 6 support](https://mozilla.github.io/uniffi-rs/latest/swift/overview.html):
  object proxy、record、associated enum、typed Error/throws。Swift 6対応はpartialと明記。
- [Objects / ownership / concurrency](https://mozilla.github.io/uniffi-rs/latest/types/interfaces.html):
  Rust objectはArcで共有され、Send + Syncが要求される。
- [Async](https://mozilla.github.io/uniffi-rs/latest/futures.html):
  Rust FutureをSwift async/awaitでpollできる。独立したRust async runtimeは必須ではない。
- [Foreign traits](https://mozilla.github.io/uniffi-rs/latest/foreign_traits.html)および
  [0.32.2 changelog](https://github.com/mozilla/uniffi-rs/blob/v0.32.2/CHANGELOG.md):
  foreign implementationをexport可能。0.32では`foreign` / `rust, foreign`指定を利用できる。
  今回はforeign traitをexportせず、将来Keychain接続時に別途検証する。
- [Xcode integration](https://mozilla.github.io/uniffi-rs/latest/swift/xcode.html):
  Rust static library、生成Swift、header/module mapをnative targetへ組み込める。
- [Timestamp](https://mozilla.github.io/uniffi-rs/latest/types/builtin_types.html):
  built-in timestampはSystemTimeで、Swift変換でprecisionを失う場合がある。
- [Apple Silicon Rust target](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html):
  `aarch64-apple-darwin`、macOS SDK/Clangによるbuild。

公式のSwift 6 caveatは全APIの可否を断定する材料ではないため、今回の生成物を
Swift 6 language mode / complete strict concurrency / warnings-as-errorsで実測します。
生成Swiftやmodule mapにpost-generation patchを適用しません。

## Build / run

Apple Silicon Mac、Rust、Swift 6以上、macOS Command Line ToolsまたはXcodeが必要です。
macOS deployment targetはRust/Swiftとも14.0です。同梱Swift Testing frameworkの最低OS versionに揃えています。
Intel/universal/iOS sliceは作りません。

repository rootから初回のみ:

```sh
cargo fetch --manifest-path crates/macos-bridge/Cargo.toml
```

以後のbuildはlocked/offlineです:

```sh
bash macos/BridgeSmoke/build.sh
```

scriptは次を実行します:

1. `aarch64-apple-darwin`のRust staticlib/cdylibをbuild。
2. 同じversionのbindgenとtest-only fixture helperをhost向けにbuild。
3. cdylibのmetadataからSwift/header/module mapを生成。
4. SwiftPM用directoryへ生成物とstatic library/helperを配置。
5. Swift 6 strict build、Swift Testingの4 tests、command-line smokeを実行。

生成だけ行う場合:

```sh
bash macos/BridgeSmoke/build.sh --prepare-only
swift build --package-path macos/BridgeSmoke -Xswiftc -strict-concurrency=complete -Xswiftc -warnings-as-errors
swift test --package-path macos/BridgeSmoke -Xswiftc -strict-concurrency=complete -Xswiftc -warnings-as-errors
swift run --package-path macos/BridgeSmoke --skip-build BridgeSmoke
```

一部Command Line Tools環境では`swift test`が同梱Testing frameworkの検索pathを発見できません。
scriptは`xcode-select --print-path`からそのframework/plugin pathを解決し、Swift testsだけに指定します。
その環境でのtestsは`build.sh`を利用してください。Xcodeの標準framework配置では通常のSwiftPM検索を使います。
XCTestを必要としないSwift Testing runnerを指定しています。

build scriptはcallerのcwdに依存しません。UniFFI 0.32のglobal config
`crates/macos-bridge/bindgen.toml`はrelative crate rootを指定し、そのcrateの`uniffi.toml`を読みます。
root Cargo.tomlからのmetadataだけでは独立bridge crateを発見できないため、この指定が必要です。

## Xcode integration

`--prepare-only`実行後、Xcodeでこのdirectoryの`Package.swift`を開き、
`BridgeSmoke` schemeのmacOS targetをbuild/run、package testsを実行できます。
別native projectに取り込む場合はこのlocal packageの`AssignmentManagerBridge` productを利用します。
SwiftPMのsystem-library targetが生成header/module mapを供給し、Swift targetがRust staticlibをlinkします。
Swiftのconsumerにbridging-header設定や独自FFI実装は必要ありません。

library search pathはPackage.swiftの位置から計算します。machine固有のabsolute pathを
tracked projectへ埋め込みません。Static linkingなので実行時DYLD_LIBRARY_PATHも不要です。
PoC packageはlocal development用で`-L`のunsafe linker flagを利用します。
配布用Swift package/XCFramework、Xcode build phaseへの自動生成、署名・sandbox・app bundleは
次のnative shell段階で決めます。PoCにはXCFrameworkや.xcodeprojを追加していません。

## Ownership / API

```text
Swift SmokeSession actor
  owns generated MacApplication proxy
    owns UniFFI Arc<MacApplication>
      owns ApplicationHandle
        owns shared local/settings/sync worker lifetime
```

依存方向はbridge → application → coreです。coreへのbridge dependencyは
domain enum conversionだけに使用します。application/core/Tauriのproduction implementationは変更しません。
実際のPhase 9 read modelは`crates/application/src/models.rs`です。

今回のSwift-facing API:

```swift
MacApplication.open(databasePath: String) async throws -> MacApplication
application.queryAssignments(options: QueryOptions? = nil) async throws -> AssignmentListDto
application.toggleCompletion(id: String) async throws -> CompletionDto
application.shutdown() async throws
```

`ApplicationHandle.open`はworker initializationの結果を後続requestへ保持する設計です。
bridgeのnamed async constructorは初回default queryをawaitし、失敗時はshutdown完了後に
typed errorを返します。成功したopenはlocal DBがquery可能であることを意味します。
shared construction contractは変更しません。

`QueryOptions`はcompletion、course、deadline、sort selectionを持ちます。nilではshared default
（Incomplete、全Course、全deadline、deadline sort）を使います。
全selectionをshared `AssignmentQuery`へ変換し、Swift側のfilter/sortはありません。
Phase 10B shellもこの既存APIを利用します。CourseはAll/Unassigned/opaque ID、
deadlineはAll/Overdue/WithinSevenDays、sortはDeadline/Course/Titleです。
UniFFI ABI/checksumは変更時に再生成が必要で、record追加がbinary互換であるという意味ではありません。

## DTO mapping

| Shared value | Swift bridge |
| --- | --- |
| Deadline::DateOnly | `.dateOnly(year: Int32, month: UInt32, day: UInt32)` |
| Deadline::DateTime(Utc) | `.dateTime(utcRfc3339: String)`、UTC `Z`、必要な小数秒を保持 |
| CompletionState | `.incomplete` / `.completed` |
| SubmissionTarget | `.unknown` / `.none` / `.moodle` / `.webClass` / `.external` |
| CourseOption | internal id、displayLabel、optional origin。nameでmergeしない |
| DeadlineStatus | `.overdue` / `.today` / `.upcoming` / `.none`、Rust判定の投影 |
| AssignmentListResponse | items、courses、totalCount。snapshotの順序を維持 |
| ApplicationError | payloadのないBridgeErrorへ全categoryを1対1変換 |

Assignmentはid/title/deadline/completion/submission/course/deadlineStatusだけです。
raw submission URL、URL presence flag、external source identityはexportしません。
`BridgeError`はSwift Error/throws、固定categoryのみです。error stringからの分類はしません。

Built-in timestampはSwift Dateへの自然なmappingに利点がありますが、DateOnlyの表現には
使えず、subsecond precisionを失う可能性があります。今回のRFC3339はDateTimeのUTC instantが
明示的で、nanosecondとpre-epochのconversionを直接testできます。表示時のlocal timezone変換は
Swift UIの将来の責務で、deadline判定は現在もRustです。DateOnlyをUTC midnightのDateへ変換しません。

## Threads / lifetime

SmokeSession actorはgenerated proxyを保持し、open/query/toggle/shutdownはasync/awaitです。
smokeは各operationの開始時にmain threadでないことをassertします。
fixture helperのProcess待ちやfilesystem操作もSmokeChecks actor上で実行します。
MainActorでのblocking wait、DispatchSemaphore、Rust block_on wrapperを導入していません。

Rust側は既存workerの同期SQLite/HTTP実装を維持しています。
SwiftがpollするFutureはworkerへのenqueueとoneshot/watch応答待ちだけで、Tokio runtime注入は不要です。
未使用のsettings/sync workerも既存ApplicationHandle contractどおり生成されます。
private PocSettingsはdefault settings、NoCredentialsはmissing credentialsを返し、writeは拒否します。
OS Keychain・設定file・secret環境変数へアクセスしません。Phase 10Cでplatform portへ置き換えます。

UniFFIはgenerated objectに`@unchecked Sendable`を自動付与します。
これは手書きの回避annotationではなく、Rust objectにSend + Syncを要求する生成contractです。
MacApplicationのRust Send + Syncをtestし、Swift DTOは通常のSendable conformanceで扱います。
手書きSwiftに`@unchecked Sendable`や`@preconcurrency`はありません。
生成classのsubclassやunsafeFromHandle等の低水準helperをsmokeから使いません。

shutdownは全proxyの新規requestを拒否し、accepted workをdrainしてworker cleanupをawaitします。
複数回呼べます。最後のproxy dropでもshared handleのnonblocking cleanupが始まります。
toggleのreturnはcommit済みstateであり、後続queryのfailureでtoggleを失敗扱いしません。
smokeはshutdownでrefresh failureを起こし、その後reopenしてcommit保持を確認します。

独自unsafe FFIはありません。Panic handlingはUniFFIの通常contractに従います。
公開operationは全てthrowsに対応し、予期しないpanicは通常のapplication categoryとは別の
UniFFI internal errorとなります。panic=abort/process failureからの回復は保証されません。
CLIのcatch-allはunknown error/panic/platform causeを出力しません。

## Test isolation / generated files

空DBはSwiftがtemporary directoryを作ります。seeded/invalid DBはtest-only Rust exampleが
自身のtemporary directoryだけに作り、既存path/DBをargumentとして受け取りません。
Swiftはそのpathをbridgeに渡し、shutdown後にdirectoryを削除します。
nativeApplicationDataDirectory()はmacOS標準APIを使い、smokeは絶対pathの解決だけを確認します。
そのdirectoryは作成せず、本番DBをopen/toggleしません。

Commitするもの: Rust/Swiftの手書きsource、Cargo manifest/lockfile、UniFFI config、
SwiftPM manifest、build script、README。

生成しgitignoreするもの: bridge target/、Native/（.a、helper、生成物）、
Sources/AssignmentManagerBridge/（生成Swift）、Sources/AssignmentManagerBridgeFFI/（header/module map）、
.build/、.swiftpm/。machine-local library/cache/SQLite DBはcommitしません。

## Rust checks

```sh
cargo fmt --manifest-path crates/macos-bridge/Cargo.toml --check
cargo clippy --manifest-path crates/macos-bridge/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/macos-bridge/Cargo.toml --locked --offline
cargo test --doc --manifest-path crates/macos-bridge/Cargo.toml --locked --offline
```

Bridge testsはread model/course/completion/submission/status/error変換、
DateOnly/UTC nanoseconds/pre-epoch変換、open failure、empty DB、default query、
commit/requery、Arc clone/shutdown/reject/drop/reopenを検証します。

## 2026-10-05 verification / verdict

実機: Apple Silicon (`arm64`)、Apple Swift 6.2.1、Rust 1.97.1、Command Line Tools。
bridge cdylibとSwift executableは`file`でMach-O arm64と確認しました。
Rust staticlibをSwift executableへlinkし、そのSwift processが実際にRust workerへrequestしました。

`bash macos/BridgeSmoke/build.sh`を通してRust build → binding generation → Swift build →
Swift tests → CLI smokeを実行し、最終設定でwarning/errorはありません。
generated Swift/header/module mapは無修正です。Xcode本体はinstallされておらず、
`xcodebuild -version`はCommand Line Tools選択を理由に失敗します。
したがってXcode GUI / xcodebuildでのbuild/testは未確認です。SwiftPM integrationは実機検証済みです。

| Check | Result |
| --- | --- |
| core fmt / locked offline clippy all-targets all-features | PASS |
| core locked offline tests / doc tests | 312 unit tests + 11 doc tests PASS |
| application fmt / locked offline clippy all-targets all-features | PASS |
| application locked offline tests / doc tests | 29 unit tests + 2 doc tests PASS |
| bridge fmt / locked offline clippy all-targets all-features | PASS |
| bridge locked offline tests / doc tests | 8 unit tests PASS、doc tests 0件 |
| desktop Tauri fmt / locked offline clippy all-targets all-features | PASS |
| desktop Tauri locked offline tests / custom-protocol build | 18 tests PASS、build PASS |
| frontend Node tests | 30 tests PASS |
| Swift 6 / complete strict concurrency / warnings-as-errors build | PASS |
| Swift Testing | 4 tests PASS |
| CLI smoke | 全項目PASS |

core/application/desktopはroot README記載の各check commandを実行しました。
Windows targetのcross-build・Windows実機UI smokeはこのMacでは実行していません。
production DB / OS credential store / 大学serverは今回の検証で使用していません。

Swift smokeで確認した内容:

- open、空DB query、Swiftによるnative Application Support path解決。
- synthetic 4 assignments、3 Course options、同名Courseの異なるinternal IDs/origins。
- DateOnly、UTC RFC3339 DateTime（nanoseconds）、deadline/courseなし、Completed/Incomplete。
- default/All/Completed selectionはRustで処理され、Rust deadline順のままSwiftへ到達。
- toggle → committed Completed → All queryで一致 → defaultから除外 → toggleで復元 → requery一致。
- typed DataDirectory/DatabaseStartup/InvalidAssignmentId/NotFound/Unavailable。
- proxy alias、shutdown再実行、shutdown後query/toggle reject、最後のSwift reference release。
- commit後のrefresh rejectionをwrite failureへ変更せず、reopenでもcommitted stateを確認。
- actor上のoperationがmain threadで実行されていないこと。
- private submission URL/token marker、external Course identities、DB pathがread model/errorに出ないこと。

**Final verdict: UniFFI suitable with caveats.**

今回のopaque object / Rust async worker / typed DTO/error / lifetimeは実機で成立しました。
C ABIへの切替を必要とするBLOCKERは確認していません。

今後の注意点:

1. 公式のSwift 6対応はpartial。今回はRust async functionsだけで成功しており、
   async foreign traits/native Keychain callbackのSwift 6対応を保証しません。
2. generated proxyのSendable contractはRust Send + Syncが根拠です。
   native shellでもactor ownershipを維持し、generated classのsubclassやunchecked annotation追加は避けます。
   Swift compiler/default isolation settings変更時にはstrict checksを再実行します。
3. UniFFI upgradeやAPI変更時にはruntime/library/bindgen/Swiftを同時に再生成します。
   generated ABI/checksumはstable binary distribution contractとして扱いません。
4. UniFFI native cancellationは保証されず、admitted writeはcancel/drop後もcommitし得ます。
   toggleをresponse cancellation/refresh failureで自動retryしません。
5. full Xcode integration、app lifecycle/署名/sandbox、XCFramework配布、Intel対応は未確認。
   このlocal static-library SwiftPM方式を配布packageへそのまま流用する前にpackagingを検討します。
6. toolchain問題はbridge問題と区別しました。最初のsandbox cache書き込み拒否は
   許可された実行で解消し、CLTのTesting検索path不足はscriptで動的解決しました。
   Testing.frameworkのmacOS 14最低versionへ揃え、初期link warningも解消しました。

## Remaining

Phase 10Bの正式なSwiftUI shellは[AssignmentManager](../AssignmentManager/README.md)へ追加しました。
本packageは今後もFFI regression smokeとして保持します。

Phase 10C: macOS Settings/native Keychain/Sync — Moodle config、WebClass credentials、manual/startup sync。

その後: academic term filter、source submission state、timetable、WidgetKit。
このPoC自体にはUIを追加しません。
