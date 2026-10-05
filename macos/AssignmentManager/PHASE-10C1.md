# Phase 10C1: Native Settings / Keychain

2026-10-05。Native SettingsとSwift Security.framework Keychainを接続しました。
network syncはexportも起動もしていません。10C2には進んでいません。

2026-10-06追補: manual smokeで報告された保存buttonのdisabled状態を修正しました。
SettingsViewにあった`status == nil`依存を除き、SettingsModelのinput-presence predicateを使います。
Moodleは空/whitespaceのみ、WebClassはID/passwordのどちらかが空/whitespaceのみ、または設定の変更処理中だけ保存不可です。
URLの正式validationは引き続き共有Rustで保存時に実行し、safe errorを表示します。
status読み込みは変更処理と別stateにし、古い読み込み結果が保存後の状態/errorを上書きしないようにしました。
Keychain/revision/secret-clear contractは変更していません。
通常設定はUserDefaults.standardへ変更し、bundle identifierをsuiteNameへ渡す経路を除去しました。
isolated test/smoke namespaceは維持し、App Groupは使用していません。
追補の検証: Settings関連8 testsを含むnative Swift 25 testsがPASS、Xcode Debug arm64がBUILD SUCCEEDEDです。
Swift 6.4 complete strict concurrency / warnings-as-errorsで実行しました。
log: `/tmp/assignment-settings-save-fix-build.log`。network syncは実行していません。

## Changed files

- `crates/application/src/settings.rs`: shared revision、置換失敗、missing、stable Moodle identity tests。
- `crates/macos-bridge/{Cargo.toml,Cargo.lock}`: private credential encodingのserde/zeroize。
- `crates/macos-bridge/src/{lib.rs,dto.rs,native_ports.rs,tests.rs}`: foreign ports、Settings use cases、boolean status、safe errors/tests。
- `macos/AssignmentManager/App/AssignmentManagerApp.swift`: standard Settings scene、同一client共有、Quit時の入力clear、isolated smoke namespace。
- `macos/AssignmentManager/Views/SettingsView.swift`: Form/Section/TextField/SecureField/Toggle/Button。
- `macos/AssignmentManager/AssignmentManager.xcodeproj/project.pbxproj`: SettingsViewをapplication targetへ追加。
- `macos/AssignmentManager/Sources/AssignmentManagerKit/Bridge/{RustApplicationClient.swift,SafeApplicationError.swift,NativeKeychain.swift,NativeSettingsStorage.swift}`: actor client、native ports、固定error表示。
- `macos/AssignmentManager/Sources/AssignmentManagerKit/ViewModels/SettingsModel.swift`: MainActor Settings state。
- `macos/AssignmentManager/Tests/AssignmentManagerKitTests/{SettingsTests.swift,PresentationTests.swift}`: Settings/Keychain abstraction/integration/security tests。
- `macos/BridgeSmoke/Sources/BridgeSmokeSupport/ForeignStoreChecks.swift`、CLI/test entry points: foreign callback regression。
- repository、bridge、native、BridgeSmoke READMEおよび本検証記録。

generated Swift/header/modulemap/static library、build artifactsは既存方針どおりgit管理外です。
Tauri/frontendのproduction source、core/schemaは変更していません。

## Native Settings architecture

```text
SwiftUI Settings scene / SettingsView
  → MainActor SettingsModel
  → RustApplicationClient actor（Assignment画面と同じinstance）
  → UniFFI MacApplication async methods
  → shared ApplicationHandle
  → shared settings worker / SettingsService
      → NativeCredentials → NativeCredentialStore foreign trait → Swift NativeKeychain
      → NativeSettings    → NativeSettingsStore foreign trait   → Swift NativeSettingsStorage
```

SettingsStatusDtoはMoodle configured / WebClass configured / syncOnStartupの3 booleanだけです。
保存済みURL・ID・passwordをSettings UIへ返すAPIはありません。
追加したSettings use casesはget/save/remove/setの必要範囲のみです。
既存Assignment open/query/filter/sort/toggle/shutdownを維持しています。

## Keychain architecture / UniFFI finding

UniFFIは引き続き**0.32.2固定**です。[公式0.32 foreign trait contract](https://mozilla.github.io/uniffi-rs/0.32/foreign_traits.html)と
[0.32.2 crate](https://docs.rs/uniffi/0.32.2/uniffi/)を確認しました。
`#[uniffi::export(foreign)]`、owned引数、`Arc<dyn Trait>` constructor、全callbackのResult、
`From<UnexpectedUniFFICallbackError>`による固定error変換を採用しています。

**foreign traitは成立しました。** Swift 6.4で生成されたprotocolはAnyObject / Sendableです。
Swift NativeKeychain/NativeSettingsStorageはimmutable final classとしてchecked Sendableに適合します。
settings workerの同期portに同期callbackを合わせたため、async callbackやMainActorへの逆hopは不要です。
Rust→Swift callbackのworker thread、Swift objectのretain、shutdown時のrelease、未知errorのsafe変換を
BridgeSmokeで実際の生成bindingを使って検証しました。callback objectはRust proxyを保持せずcycleを作りません。

secret transportはprivate adapter用Data/Vec<u8>です。公開secret DTOやgenerated secret recordは追加していません。
Rust NativeCredentialsがopaque recordをencoding/decodingし、application CredentialStoreへ変換します。
Swiftはrecordの内容を解釈せず、OS読み書きだけを担当します。validation/revision/session policyは共有Rustに残しています。

production service: `org.assignmentmanager.native.credentials.v1`。
account: `moodle-feed-v1` / `webclass-credentials-v1`。
大学ID、token URL、passwordをKeychain metadataに使用しません。
generic passwordをmacOSのlocal file-based Keychainへ作成します。
現在のad-hoc署名に合わせ、Data Protection Keychainへ切り替えるentitlementsは追加していません。
`kSecUseDataProtectionKeychain=false`を指定し、synchronizable属性を省略します（defaultではiCloudへ同期しません）。
このbackendで保証されないiOS/Data Protection専用accessible属性は設定しません。
OS Keychainの暗号化・lock・access controlを使用し、ThisDeviceOnly protectionを保証するという主張はしません。
Appleの[SecItem注意事項](https://developer.apple.com/forums/thread/724013)とXcode SDKのSecItem.h contractを確認しています。
replaceはSecItemUpdate、missing時のみSecItemAdd、add raceのduplicate時はupdate再試行です。
delete-firstはありません。missing read/deleteは正常な未設定です。

Tauri serviceは`io.assignment-manager.desktop`です。同じaccount名でもserviceが異なるため共有せず、
既存entryの探索、自動利用、silent migrationはしません。

## Moodle / WebClass / syncOnStartup

- Moodle: shared validationでHTTPS / host / userinfoなしを検証。save/replace/remove/missingを検証済み。
  `default.course-related` / `CourseRelated`はshared application/coreの固定identityをそのまま使い、URLからidentityを作りません。
- WebClass: ID/passwordを一つのJSON valueData recordとして保存。partial recordはsafe read errorです。
  shared SettingsServiceによるrevisionを維持。保存→置換→削除の成功で1→2→3、失敗では2のままというtestを追加しました。
  revisionはapplication lifetime内のsession世代であり、OS設定へ独自に複製・永続化しません。
  既存のsession invalidation testsも通過しています。
- syncOnStartup: 通常起動はUserDefaults.standardの`syncOnStartup` keyへbooleanのみ保存。
  test/smokeは明示的な独立suiteへ保存します。bundle identifierをsuiteNameに渡しません。
  missingはforeign portがNoneを返し、default=falseはRust Settings::defaultから取得します。
  保存結果をshared SettingsServiceへ返し、成功後のみapplication stateを更新します。
  設定をtrueにしても10C1では同期を呼びません。UIにも現在は自動実行されないことを表示します。

## Security / Swift concurrency

secret入力は保存Taskが値を取得した直後、await前にUIからclearします。成功・失敗後にもclearし、
Settings windowが閉じたときとQuit時にもclearします。保存済み値をplaceholderへ埋めません。
passwordはSecureFieldです。SettingsModel / NativeKeychainのdebugDescriptionは固定文字列です。
Swift/UniFFIのString/Data/serialization copies全体についてheapの完全zeroizationは保証しません。
共有SecretTextとRustの読み込みbufferはzeroizeし、native側でsecretを持つcacheを作りません。

UserDefaults/plist/SQLite/read model/logへのsecret保存経路はありません。
Security frameworkのOSStatusはNativeStoreError（Unavailable / Failed）へ直ちに分類し、
Rustが操作に応じたapplication/BridgeErrorへ変換します。UIは固定日本語messageのみを表示します。
未知foreign callback errorもcauseを破棄し、UIへraw textを返しません。

SettingsModelはMainActor、RustApplicationClientはactorです。callbackは共有settings workerから呼ばれます。
NativeKeychainはimmutable backendを持ち、Security API dictionary/resultはcall-localです。
Security呼び出し自体もprocess-wide serial queueで直列化し、複数handleからの同時呼び出しを防ぎます。
呼び出し元はRust settings workerです。このqueueでMainActorを待機させる経路はありません。
native settings adapterはimmutableなoptional suite名だけを保持し、独自のmutable cacheを作りません。
通常起動はthread-safeなUserDefaults.standard、明示的にsuiteを指定したtest/smokeは独立domainを使います。
test backendはOSAllocatedUnfairLockを使用します。手書き`@unchecked Sendable` / preconcurrency / semaphoreは追加していません。
UniFFI自身のgenerated proxyが持つSendable実装はgenerator出力のままです。

## Tests / existing regressions

unit testsはmemory Keychain doubleだけを使用します。UserDefaultsはUUID入りtest domain、DBはtemporary directoryです。
production Keychainへunit testsからアクセスしません。

| Check | Result |
|---|---|
| Rust fmt / clippy -D warnings（core/application/bridge/Tauri） | PASS |
| core tests / doctests | 312 / 11 PASS |
| application tests / doctests | 30 / 2 PASS |
| bridge tests / doctests | 12 PASS / 0 |
| Tauri tests / custom-protocol build | 18 PASS / PASS |
| frontend tests | 30 PASS |
| Native Swift 6.4 tests, complete strict concurrency, warnings-as-errors | 25 PASS（2026-10-06追補）、compiler warning/error 0 |
| BridgeSmoke Swift tests / CLI | 5 PASS / 全項目PASS |
| Xcode 27 Debug arm64 | BUILD SUCCEEDED、Swift compiler warning/error 0 |

Xcode build全体には、AppIntentsを使用しないtargetに対するmetadata extractorの
`Metadata extraction skipped, no AppIntents.framework dependency found` warningが1件あります。
このwarningを隠すbuild設定は追加していません。Xcode全体warning 0の目標は未達です。
core HTTP testsはsandboxのlocalhost制限で初回失敗し、sandbox外のlocalhost mockで再実行して通過しました。
大学serverへの接続は行っていません。

主要追加checks: Moodle/WebClass save/replace/remove/missing、configured status、application再open時の設定保持、
syncOnStartup preference persistence、failed settings write時のstate保持、WebClass revision成功/失敗、
atomic whole-record replacement、safe error mapping、corrupt/partial record、UI入力clear、
UserDefaults/SQLite/read model/error/Debugのsecret marker非混入、foreign lifetime/thread/error。

build/check logsは`/tmp/assignment-10c1-*.log`です。logへsecret値を出力していません。

## Manual / production Keychain smoke

**未実施です。** Computer UseのAssignment Managerへのアクセスが許可されませんでした。
実.appのSettings、⌘,、Light/Dark、実Keychainへの保存・再起動・置換・削除を確認済みとは扱いません。

build済み.app: `macos/AssignmentManager/build/Build/Products/Debug/Assignment Manager.app`。
実OS Keychainを安全に確認するため、DEBUG版の`--smoke-data-directory`はfixture名に対応する
`org.assignmentmanager.native.smoke.<directory-name>` service / preferences domainを使います。
再起動時は同じdirectoryを使ってください。production namespaceへのmigrationはありません。

手動手順（synthetic dataのみ、secretをlogへ出さない）:

1. `macos/BridgeSmoke/Native/prepare-native-smoke`で一時fixture directoryを作成する。
2. build済み.appを`open -n 'macos/AssignmentManager/build/Build/Products/Debug/Assignment Manager.app' --args --smoke-data-directory '<fixture-directory>'`で起動する。
3. Assignment一覧/filter/sort/completionを確認する。
4. app menuのSettingsまたは⌘,でstandard Settings windowを開く。
5. Moodleへexample.invalidのHTTPS synthetic URLを保存。入力がclearされ設定済みだけ表示されることを確認する。
6. WebClassへsynthetic ID/passwordを保存。同じく入力clear/設定済みを確認する。
7. 起動時に同期をtrueに設定、⌘Qで終了、同じdirectoryで再起動する。
8. 両方configured=true / syncOnStartup=true、network操作なしを確認する。
9. Moodle/WebClassを個別に置換、終了・再起動、configuredを確認する。
10. 個別に削除、終了・再起動、両方未設定を確認する。smoke Keychain entryを残さない。
11. Light/DarkのForm/Section/SecureField表示を確認する。

production serviceでの同じ手順は、実credentialを開示/記録せず、所有者による確認が必要です。

## Final verdict / Remaining

**実装と自動検証はnative sync接続の前提を満たしますが、Ready for native syncの最終判定は保留です。**
実Keychain / 実.app manual smokeが未完了で、Xcode metadata warningが1件残っています。

次はPhase 10C2: syncNow / startup sync / Moodle / WebClass login・reauth / partial failure / assignment reload。
今回はいずれも接続していません。その後はPhase 11A Academic term filter、11B Source submission state、
12 Timetable、13 WidgetKitです。
