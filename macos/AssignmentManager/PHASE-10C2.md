# Phase 10C2: macOS Native Sync Integration

2026-10-06。**Ready for real macOS sync smoke**。
manual/startup Syncを既存shared coordinatorへ接続し、Apple Silicon Debug `.app`をbuildしました。
Codexは大学serverへ接続していません。実server／native windowの確認は下記manual checklistでユーザーが行います。
Phase 11Aには進んでいません。

## Changed files

| Area | Files |
| --- | --- |
| Rust bridge | `crates/macos-bridge/src/lib.rs`, `sync_dto.rs`, `tests.rs`, `sync_tests.rs`, `README.md` |
| Native transport / presentation | `Sources/AssignmentManagerKit/Bridge/RustApplicationClient.swift`, `Models/SyncModels.swift` |
| MainActor state / UI | `Sources/AssignmentManagerKit/ViewModels/AssignmentAppModel.swift`, `Views/AssignmentShell.swift`, `Views/SettingsView.swift` |
| Native tests | `Tests/AssignmentManagerKitTests/SyncTests.swift`, `ConcurrencyTests.swift`, `BridgeIntegrationTests.swift` |
| BridgeSmoke | `Sources/BridgeSmokeSupport/SmokeChecks.swift`, `Sources/BridgeSmoke/main.swift`, `Tests/BridgeSmokeTests/SmokeTests.swift`, `README.md` |
| Documentation | repository `README.md`, native `README.md`, this report |

core/application/Tauri/frontend production source、SQLite schema、Cargo manifests/lockfiles、Keychain adapter、Settings storageは変更していません。
生成Swift/header/module map/staticlibは既存build scriptで再生成し、gitignoreします。生成物の手修正はありません。

## Native sync architecture

```text
MainActor AssignmentAppModel (display / current query selection)
  → RustApplicationClient actor
    → generated async UniFFI MacApplication
      → ApplicationHandle shared coordinator / sync worker
        → existing CoreSyncBackend → core HTTP/auth/sync → SQLite
        → settings worker → CredentialStore → foreign NativeCredentialStore
          → Swift NativeKeychain → Security.framework
```

bridge APIは`syncNow()`, `runStartupSync()`, `getSyncStatus()`のみ追加。
DTOはMoodle/WebClass別NotConfigured/Success/Failed、safe件数、固定failure enum、shared `should_reload`、running/lastReportです。
source選択、独立実行、partial failure、single-flight、session reuse、revision失効、reauth/retry、startup onceはshared application/coreの既存実装を使用します。

## Moodle

shared CredentialStoreから必要時にcalendar URLを読み、既存`MoodleFeed` / `sync_moodle_once`を使用します。
logical identityは`default.course-related` / `CourseRelated`のままです。URL置換でAssignment/Course identityを変更しません。
native Settingsのshared validation、save/replace/remove、secret clearは維持しました。

## WebClass

shared coordinatorがcredentialsを取得し、初回`HoseiAuthenticator.login` → `WebClassClient` → syncを実行します。
以降はmemory sessionを再利用します。AuthExpired時はcoreがfresh credential callback → login → full retry onceを実行します。
再認証失敗／2回目の失効はsafe source failureになり、追加retryをSwiftが行うことはありません。
cookie/sessionをdisk/UserDefaults/Keychainへ保存しません。

## Keychain / credential revision

10C1のSwift native Keychain / UniFFI synchronous foreign trait / shared CredentialStore接続をそのまま使用します。
ViewModelからKeychainを読んでsecretをsyncへ渡すAPIはありません。
WebClass replace/remove成功時のshared revision更新で次sync前に旧sessionを破棄します。
failed saveではrevision/sessionが維持されることをbridgeと既存application testsで確認しました。
entry naming、whole-record replacement、safe OSStatus mapping、secret input lifetimeは変更していません。
non-secret `syncOnStartup`は引き続き`UserDefaults.standard`、default/validationはshared contractです。

## Sync UI

Assignment toolbarにnative「同期」Buttonを追加しました。同期中は「同期中…」とProgressViewを表示し、追加クリックをdisableします。
shared single-flightがAlreadyRunningを返した場合はrunning/lastReportを非blockingで観測し、追加runをqueueしません。
Moodle/WebClassそれぞれ未設定・成功（追加/更新/変更なし件数）・失敗（固定safe message）を表示します。
Settingsの起動時同期の説明も実際の動作へ更新しました。

## Startup sync

window/open → 最初のlocal query結果をpublish → background `runStartupSync`の順です。
network終了までwindow/local listを待たせません。configured判定やON/OFF判定はSwiftにありません。
modelはapplication lifetimeで所有され、Viewの再生成による`start()`の再呼び出しでsyncを再開しません。
別proxyからの再呼び出しもshared lifetime once guardが保護します。OFF／未設定／startup errorもonceを消費する既存policyを維持します。

## Partial failure / offline behavior

`Completed`は全source成功を意味しません。FailedでもWebClassの先行groupがcommit済みの場合があります。
bridgeがshared `should_reload`をそのまま返し、失敗を含む設定済みsourceの終了後に再queryします。
reply喪失などのbridge error後もrollbackとみなさず、保守的にreloadします。
network failureはapp Phaseをfailedに変更しません。既存一覧、filter/sort、completion、Settingsを継続利用できます。
reload自体が失敗しても最後のsnapshotを保持し、安全なnoticeを表示します。

## Completion preservation

reloadは現在のcompletion/deadline/Course filterとsortを維持します。
query generationで古いread responseの上書きを防ぎます。syncはlocal CompletionStateを変更しません。
synthetic sync → toggle → URL replace/re-sync → shutdown/reopenでID/completion保持を確認しました。

## Swift concurrency

表示stateはMainActor、transportは既存client actor、HTTP/auth/SQLiteはRust workerです。
async UniFFIをawaitし、Keychain callbackはsettings worker上で実行します。
unchecked Sendable / preconcurrency / semaphore / blocking waitは追加していません。
sync Taskはfilter変更／View lifetimeでcancelしません。QuitはRustがaccepted workをdrainし、sync/toggle Taskをawaitします。
Swift Task cancellationやreply喪失をRust rollbackとして扱いません。

## Security

Swiftへ渡すsync結果にはsecret/URL/session/identity/raw causeのfieldがありません。
messageは固定enumから作り、unknown bridge errorも既存SafeApplicationErrorへ変換します。
Moodle token URL、WebClass ID/password、cookie、SAMLResponse、RelayState、redirect、raw HTTP body、DB path、SQLite cause、external identityをUI/logへ出しません。
secretをUserDefaults/plist/SQLiteへコピーする処理は追加していません。
testsはsynthetic `example.invalid`の入力、memory credential backend、UUIDのisolated preferences、一時DB、localhost HTTP mockだけを使用します。
production Keychainや大学serverはautomated testsで使用しません。

## Automated tests / existing regressions

| Check | Result |
| --- | --- |
| core fmt / clippy all-targets all-features `-D warnings` | PASS |
| core unit tests / doctests | 312 / 11 PASS |
| shared application fmt / clippy / unit tests / doctests | PASS / PASS / 30 / 2 PASS |
| macOS bridge fmt / clippy / unit tests / doctests | PASS / PASS / 18 PASS / 0 cases |
| Tauri fmt / clippy all-targets all-features / tests / custom-protocol build | PASS / PASS / 18 PASS / PASS |
| frontend Node tests | 30 PASS |
| BridgeSmoke strict Swift build / tests / CLI | PASS / 6 PASS / all checks PASS |
| native strict Swift tests (including Settings/Keychain) | 32 PASS |

existing shared/core fake/localhost tests cover no source/each/both、Moodle成功/失敗、initial login/session reuse、AuthExpired→reauth成功、reauth失敗/retry once、source独立失敗/partial commit、single-flight、replace/remove revision、startup OFF/ON/once、completion保持。
new bridge tests cover foreign-port source選択、safe DTO/status、partial commit/failed-source reload、duplicateなし、URL rotation/completion/reopen、failed replace後session維持、replace/remove時session破棄、single-flight/local query、startup once。
new Swift tests coverrunning/button gate、source presentation/safe error全category、partial failure/両source失敗、current filter/sortでreload、local list先行startup/再start、startup skip、AlreadyRunning観測（再submitなし）、offline snapshot保持、Quit時drain。
既存Settings保存可否・shared validation・secret clear・native preferences persistence・Keychain abstraction regressionも通過しています。

## Xcode build

Xcode 27.0 (27A266a)、Apple Swift 6.4、macOS 27.0 SDK、deployment target macOS 14.0。
`bash macos/AssignmentManager/build.sh`が成功し、Debug `.app`のMach-O arm64を確認しました。
Swift 6 language mode / complete strict concurrency / warnings-as-errorsです。

- Compiler warning: **0** / compiler error: **0**。
- AppIntents metadata warning: **1** — `Metadata extraction skipped, no AppIntents.framework dependency found`（既知のbuild-tool warning、compiler warningと分離）。
- 出力: `macos/AssignmentManager/build/Build/Products/Debug/Assignment Manager.app`。

## Manual smoke checklist (user performs real server access)

Codexは大学serverへ接続せず、以下は未実施です。本番Keychainのsecretをlog表示して確認しないでください。

1. 起動してlocal list／filter／sort／completion／⌘, Settingsが使えることを確認。
2. Moodle configured → 「同期」 → Moodle成功・Assignment取得を確認（WebClass未設定は「未設定」）。
3. WebClass configured → 「同期」 → login・WebClass成功・Assignment取得を確認（Moodle未設定も個別に確認）。
4. 両方configured → 「同期」 → source別結果・一覧反映を確認。
5. 再Sync → duplicateなしを確認。連打して追加runがqueueされず「同期中…」になることを確認。
6. completion toggle → Sync → completion保持を確認。completion/deadline/Course filter・sortを変更して同期後も維持されることを確認。
7. WebClass credential replace → 次Syncが旧sessionを使用しないこと、remove → 未設定になることを確認。
8. offline／片source失敗 → 既存一覧とcompletion/Settingsを利用でき、成功sourceや先行commitが一覧へ反映されることを確認。
9. syncOnStartup OFF → restart → local listが即表示され、自動network syncしないことを確認。
10. syncOnStartup ON → restart → local list先行表示・background sync・完了後reloadを確認。View再表示で再実行しないことを確認。
11. Light / Darkでtoolbar、progress、source別結果、notice、Settingsの可読性を確認。

## Final verdict / remaining

**Ready for real macOS sync smoke**。automated checks/buildのBLOCKERはありません。
実大学serverのSSO・session expiry・実Keychain権限・実windowの結果はユーザーのmanual smokeで確認します。
Windows実機UI／cross-build、macOS Tauri GUI、配布署名は今回の検証に含みません。

次: Phase 11A Academic term filter → Phase 11B Source submission state → Phase 12 Timetable → Phase 13 WidgetKit。
今回これらの機能を追加していません。
