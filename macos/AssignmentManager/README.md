# Phase 10B: macOS native SwiftUI shell

Apple Silicon用の正式なmacOS application targetです。ローカルAssignment一覧、
completion/deadline/Course filter、sort、completion toggleを提供します。
Windows/Tauri版とcore/applicationのproduction sourceは維持しています。
Settings、Keychain、manual/startup network sync、時間割、WidgetKit、学期、
source submission stateは今回追加していません。

## Project / build

```text
macos/
├── AssignmentManager/
│   ├── AssignmentManager.xcodeproj/   native macOS application target / shared scheme
│   ├── App/                          @main App / AppKit termination delegate
│   ├── Views/                        SwiftUI shell / filter strip / assignment row
│   ├── Sources/AssignmentManagerKit/
│   │   ├── Bridge/                   Rust actor adapter / native path / safe messages
│   │   ├── Models/                   DTO projection / presentation / grouping
│   │   └── ViewModels/               MainActor application model
│   ├── Resources/Info.plist
│   ├── Tests/AssignmentManagerKitTests/
│   ├── Package.swift                 local library + Swift Testing
│   └── build.sh
└── BridgeSmoke/                       retained FFI regression smoke / generated package
```

Full Xcode、macOS SDK、Swift 6、Rustと`aarch64-apple-darwin` targetが必要です。
Command Line Toolsのみの場合、build scriptは停止します。deployment targetはmacOS 14.0です。
初回のRust dependency fetchはBridgeSmoke READMEと同じです。

repository rootから:

```sh
bash macos/AssignmentManager/build.sh
```

scriptはlocked/offline Rust buildと同versionのUniFFI生成、native smoke helperのbuild、
Swift 6 tests（complete strict concurrency / warnings-as-errors）、Xcode Debug buildを実行します。
Xcode標準cacheへの書き込みが必要です。生成Swift/header/module mapにpatchを適用しません。

出力: `macos/AssignmentManager/build/Build/Products/Debug/Assignment Manager.app`

CLIのnative build部分:

```sh
xcodebuild -project macos/AssignmentManager/AssignmentManager.xcodeproj \
  -scheme AssignmentManager -configuration Debug \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath macos/AssignmentManager/build build
```

Xcode GUIでは先に`build.sh`を1回実行して生成物を準備し、`.xcodeproj`の
`AssignmentManager` schemeを開いてbuild/runします。projectはlocal Swift packageを参照します。
Rust APIやtoolchain変更後はscriptで再生成してください。machine固有のpathはtracked projectにありません。
生成物、Rust/Swift cache、DerivedData、runtime DBはgitignoreします。

この段階はlocal Debug/ad-hoc signingです。App Sandbox、hardened runtime、Developer ID、
notarization、universal binaryや配布用XCFrameworkの検証は含みません。

## Ownership / startup / shutdown

```text
@main AssignmentManagerApp
  → ApplicationDelegate（application lifetimeに1つ）
    → MainActor AssignmentAppModel
      → RustApplicationClient actor
        → generated MacApplication proxy
          → ApplicationHandle
            → shared application workers / core
```

`Window` sceneを用い、View/Windowでclientを生成しません。delegateがmodel/clientを所有します。
Viewの`.task`はidempotentな`model.start()`を呼びます。windowを準備してloadingを表示し、
client actorが保存先解決と`MacApplication.open`を行い、その後local queryを発行します。
bridge constructor内の初期化確認queryは10Aの既存contractです。
初回responseが来るまでsnapshotはnilで、空DB表示へ早期遷移しません。

clientはin-flight open taskを共有するため、同時openでもproxy/runtimeを二重生成しません。
open failureはpayloadのないsafe errorへ変換し、View再生成ではretryしません。
失敗画面の明示retryのみ再度openできます。

QuitはAppKit delegateの`applicationShouldTerminate`から`.terminateLater`を返し、
async `model.shutdown()` → `client.shutdown()` → `MacApplication.shutdown()`をawaitした後、
`reply(toApplicationShouldTerminate: true)`を呼びます。
startupとQuitが重なる場合もin-flight openを待ってcloseします。
Rustは新規requestを拒否し、accepted workをdrainします。
強制終了時は明示shutdownを保証できず、最後のproxy dropの既存cleanup contractに従います。
AppKit termination方式は[Apple公式contract](https://developer.apple.com/documentation/appkit/nsapplicationdelegate/applicationshouldterminate(_:))に従います。

## UI / query contract

NavigationSplitViewのsidebarは「課題」のみです。List/Section/checkbox Toggle、
menu Picker、Toolbarのローカル再読み込みと⌘Rを使います。
空のSettings/Timetable画面は作りません。

| Native selection | Existing bridge / shared application |
| --- | --- |
| 未完了（初期値） / 完了済み / すべて | Incomplete / Completed / All |
| すべて / 期限切れ / 7日以内 | All / Overdue / WithinSevenDays |
| すべて / 未所属 / Course label | All / Unassigned / Course { opaque id } |
| 締切 / Course / タイトル | Deadline / Course / Title |

既存repositoryの`QueryOptions`と9件のRust bridge testsが既に全mappingを持っていました。
10BではそのAPIを利用し、production bridge APIを重複追加していません。
queryのvalidation、時計、期限判定、completion/filter/sortはRust application/coreの責務です。
SwiftのDTO projectionは返されたitem arrayを並べ替えません。
グループは最初に現れたCourseIdの順、各グループ内はRust responseのsubsequenceです。
Course filterのoption順はRust `courses` responseの順です。
同名Courseをmergeせず、未所属はoptional identityでどのCourseIdとも区別します。
グループ化のため、全Courseを横断するflat sort表示にはしません。

rowはcompletion、title、締切、Rust deadline status、提出先を表示し、Section headerに
Course labelとMoodle/WebClass/手動/連携なしを表示します。内部IDはUIに表示しません。
DateOnlyはcalendar componentsを直接`yyyy/MM/dd`へ整形し、Dateへ変換しません。
DateTimeはUTC RFC3339をparseして`TimeZone.current`で表示します。
deadline statusはRustのOverdue/Today/Upcoming/Noneを表示するだけで、Swiftで再判定しません。
Completedは控えめに表示し、期限警告を出しません。
SubmissionTargetは「提出先」として表示します。提出済みstatusは追加していません。

nil snapshot/loading、totalCount=0の空DB、totalCount>0のfilter結果なしを区別します。
再queryが失敗しても直前snapshotを保持し、固定safe messageを表示します。
保存先、SQLite cause、URL、secretをUI/logに渡しません。catch-allも固定Unavailableへ落とします。

filter stripは`ViewThatFits`で1行/2行へ切り替わり、学期Pickerを後から追加できるViewに分離しています。
rowも狭い幅では締切と提出先を縦に表示します。初期920×660、最小640×420です。
system primary/secondary/quaternaryを利用し、期限切れのみsystem redと警告icon/textを使います。
Light/Dark/実resizeの目視確認は今回未実施です。

## Concurrency / completion

表示stateはすべてMainActor `ObservableObject`へ限定します。
非MainActor actorのclientがgenerated async proxyをawaitし、同期DB処理はRust workerが実行します。
blocking wait、semaphore、手書きunchecked Sendableやpreconcurrencyはありません。

filter変更/reloadごとにrequest generationを増やし、前のread Taskをcancelします。
response/errorの採用時にgeneration、Task cancellation、lifecycleを確認し、
古いresponseが最新selection/snapshot/errorを上書きできないようにします。
cancelはUI response採用の中止です。Rustのadmitted DB operationのrollbackを意味しません。
[UniFFI async contract](https://mozilla.github.io/uniffi-rs/latest/futures.html)も参照してください。

toggle Taskはfilter変更/View再生成でcancelしません。保存中はcheckboxをdisableします。
成功responseのcommitted stateを直前snapshotへ反映した後、最新selectionで別queryを行います。
refresh failureはqueryErrorのまま保持し、「完了状態は保存しました」と表示します。
toggleErrorへ変更したり、writeを自動retryしたりしません。
toggle failureも自動再toggleせず、readのみ再queryします。

## Persistence / isolation

Foundation `FileManager.url(for: .applicationSupportDirectory, in: .userDomainMask, ...)`で
取得したdirectoryに`org.assignmentmanager.native/assignment-manager.db`を配置します。
通常のunsandboxed macOSでは`~/Library/Application Support/org.assignmentmanager.native/assignment-manager.db`
に相当しますが、この文字列をsourceへhardcodeしていません。
directory作成はclient actor、SQLite open/schemaの処理はRust workerで行います。

Tauriの`io.assignment-manager.desktop`とは保存先が独立しています。
自動copy/migrationや同一live DBの同時共有は行いません。
既存completion引継ぎは後で一度限りのmigrationとして設計します。
通常native Application Support DBの実起動/再起動は未確認です。
一時DBで実bridgeのtoggle → shutdown → reopenの保持をtestしています。

debug限定`--smoke-data-directory`は解決後の一時directoryのみ許可します。
引数不正ならsafe startup failureとなり、本番DBへfallbackしません。Releaseにはこのargument処理がありません。
`prepare-native-smoke`は入力pathを受け取らず、自分の一時directoryへ8件を生成します。
Keychain、credential file、secret環境変数、大学serverへアクセスしません。
bridge private settings/credential adaptersは10Aのread-only/default/missing contractのままです。

## Tests / verification (2026-10-05)

Xcode 27.0 (27A266a)、macOS 27.0 SDK、Apple Swift 6.4、Apple Silicon。
`xcode-select -p`は`/Applications/Xcode.app/Contents/Developer`です。
arm64 Debug native application targetの`xcodebuild`が成功し、実行ファイルのMach-O arm64を確認しました。

Swift testsはDTO projection、DateOnly timezone不変、DateTime local timezone/rollover、
CourseId grouping/order、全query mapping、safe error全category、loading/empty、
startup once/retry、stale success/failure、snapshot保持、toggle commit+refresh failure、
filter変更中のwrite、write failure、native path、安全なdebug override、実bridge全filter/sort、
toggle永続化/reopen、open/shutdown raceを検証します。
一時fixture以外のDBはtestsで開きません。

| Check | Result |
| --- | --- |
| core fmt / clippy all-targets all-features | PASS |
| core tests / doc tests | 312 + 11 PASS（HTTP fixtureはsandbox外で実行） |
| application fmt / clippy / tests / doc tests | 29 + 2 PASS |
| bridge fmt / clippy / tests / doc tests | 9 PASS / doc tests 0 |
| Tauri fmt / clippy / tests / custom-protocol build | 18 PASS / build PASS |
| frontend Node tests | 30 PASS |
| BridgeSmoke Swift 6 strict build / tests / CLI smoke | 4 PASS / 全CLI項目PASS |
| Native Swift 6 strict tests | 17 PASS |
| Xcode arm64 Debug `.app` build | PASS（Mach-O arm64 / local ad-hoc signing） |
| 実window manual smoke / Light / Dark / resize | 未確認（Computer Use権限未許可、ユーザー指定で今回は省略） |

BridgeSmokeはSwift 6.4のlocal weak-var warningに対応し、stored weak referenceへ変更しました。
warningを無効化せず、元のretain/drop assertionを維持しています。
Windows cross-build/実機UI smokeとmacOS Tauri GUI smokeは今回実施していません。

## Changed files

新規:

- `AssignmentManager.xcodeproj/project.pbxproj`、shared `AssignmentManager.xcscheme`
- `App/AssignmentManagerApp.swift`
- `Views/AssignmentShell.swift`、`AssignmentFilters.swift`、`AssignmentRow.swift`
- `Sources/AssignmentManagerKit/Bridge/RustApplicationClient.swift`、`SafeApplicationError.swift`
- `Sources/AssignmentManagerKit/Models/AssignmentModels.swift`
- `Sources/AssignmentManagerKit/ViewModels/AssignmentAppModel.swift`
- `Resources/Info.plist`、`Package.swift`、`build.sh`、本README
- `Tests/AssignmentManagerKitTests/PresentationTests.swift`、`ConcurrencyTests.swift`、`BridgeIntegrationTests.swift`
- repositoryの`crates/macos-bridge/examples/prepare-native-smoke.rs`

更新:

- root `.gitignore`、`README.md`
- `crates/macos-bridge/README.md`
- `macos/BridgeSmoke/README.md`、`Sources/BridgeSmokeSupport/SmokeChecks.swift`

application/core/Tauri/frontendのproduction sourceとCargo manifest/lockfileには変更がありません。

## Later manual smoke

以下は今回未実施です。temporary seeded DBで再現できます:

```sh
smoke_dir="$(macos/BridgeSmoke/Native/prepare-native-smoke)"
open -n 'macos/AssignmentManager/build/Build/Products/Debug/Assignment Manager.app' \
  --args --smoke-data-directory "$smoke_dir"
```

1. startup/loading → 7 incomplete / total 8、同名のMoodle/WebClassが別Section。
2. 完了filterの未完了/完了済み/すべて、期限filter、未所属と各Course、3種sort。
3. checkbox toggle → requery、⌘Qで終了し同じ一時pathで再起動 → completion保持。
4. 空のtemporary directoryで起動 → DB空表示、filter no match → 条件なし表示。
5. 不正SQLite内容の一時DBで起動 → safe startup message、path/causeなし。
6. Light/Dark、920×660と640×420、長いtitleのwrap、window resize、sidebar toggle。
7. 最後に通常launchでnative Application Support DBが独立して作成されることを確認。

## Remaining

次はPhase 10C: macOS native Settings / Keychain / manual・startup Sync。
その後にshared Rust Academic term filter、Source submission state、Timetable、WidgetKit。
今回10Cには進みません。
