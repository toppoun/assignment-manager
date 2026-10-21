# Assignment Manager

Rust core、Phase 9のshared application layer、Windows向けTauri 2 desktop shell、
Phase 10C2のmacOS native SwiftUI application shell / Settings / Swift Keychain / Syncです。

macOS正式shellのXcode project、build/test手順、lifecycle、保存先、検証結果は
[macos/AssignmentManager/README.md](macos/AssignmentManager/README.md)を参照してください。
`bash macos/AssignmentManager/build.sh`でgenerated bridge、Swift tests、arm64 Debug `.app`をbuildします。

macOS bridgeのarchitecture・公式仕様の確認・再現可能なSwift 6 smoke手順は
[macos/BridgeSmoke/README.md](macos/BridgeSmoke/README.md)を参照してください。
UniFFI 0.32.2を固定し、Apple Silicon上で共有ApplicationHandleのopen/query/toggle/settings/sync/shutdownを検証します。
BridgeSmokeは引き続き独立したFFI regression smokeです。
native manual/startup Syncをshared coordinatorへ接続しました。
最新のarchitecture・検証結果・manual smoke未確認項目は
[Phase 10C2 report](macos/AssignmentManager/PHASE-10C2.md)を参照してください。
macOS Tauri版の起動・SQLite・Keychainは以前の実機確認済みです。
大学serverへの実接続はユーザーのmanual smokeで確認します。Codexは接続していません。

## Shared application architecture

依存方向は`Tauri → assignment-manager-application → assignment-manager`、
macOS正式版は`SwiftUI → Swift adapter → generated UniFFI proxy → macos-bridge → assignment-manager-application → assignment-manager`です。
rootの`src/`はcoreのまま、共有applicationは`crates/application/`に置きます。
coreへTauri、OS credential API、FFI依存を追加していません。SQLite schema v3も維持します。

application lifetimeで一つの`ApplicationHandle`を所有します。cloneは同じruntimeを参照し、
query/completion、sync、settingsの3 workerへrequestを送ります。前者2つは別Connectionを所有し、
network待ちでもquery/completionを利用できます。ConnectionやHTTP clientはhandleから返しません。
source別sync policy、session再利用/revision invalidation、single-flight、startup once、
query optionと安全なread model/errorは共有層にあります。

Tauri側はpath解決、JSON request/DTO、日本語message、JSON settings storage、OS credential backendを担当します。
共通settings contractはJSONを要求せず、`SettingsStore` / `CredentialStore`のplatform portを利用します。
詳細なAPI・lifecycle contractは[application README](crates/application/README.md)を参照してください。

## Development

Tauriの[OS別prerequisites](https://v2.tauri.app/start/prerequisites/)が必要です。
WindowsではMSVC build toolsとWebView2、macOSではXcode Command Line Toolsを使用します。
frontendは素のHTML/CSS/JavaScriptで、npm、frontend framework、pluginは不要です。

最小の起動方法（リポジトリrootから）:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --locked --features custom-protocol
```

任意でTauri CLIを使用するとfrontendの変更も自動reloadできます:

```sh
cargo install tauri-cli --version '^2' --locked
cargo tauri dev
```

通常起動はTauriの`app.path().app_data_dir()`で取得したapp専用directory
（identifier: `io.assignment-manager.desktop`）内の`assignment-manager.db`を毎回使います。
identifierとfilenameを変更すると保存先が変わるため、継続して同じ値を使用してください。
directory作成と`SqliteStore::open`（schema v3 migration）は起動時に専用workerで一度実行し、
一覧取得・完了切り替えもそのworkerが同じstoreで順に処理します。
coreはroot crateのまま、application/desktop/macos-bridgeはそれぞれ独立manifest/lockfileで依存します。

初回DBは空です。「課題はまだありません」は正常な状態です。
設定済みサービスから「同期」で課題を取得します。課題作成・DB選択UIはありません。

## Assignment list

初期表示は未完了・締切が近い順です。完了状態（すべて／未完了／完了済み）、
締切（すべて／期限切れ／今日／7日以内）、Course（すべて／未所属／各Course）、
並び順（締切／Course／タイトル）を変更できます。coreは昇順のみのcontractなので方向指定はありません。
filter/sortの選択は再起動でdefaultへ戻ります。

frontendは型付きrequestを`list_assignments`へ渡し、RustがOS local timeから
`QueryTimeContext`と`AssignmentListOptions`を構築します。
「7日以内」はDateTimeで現在のinstantから168時間先まで、DateOnlyで今日から7日後までの
inclusive windowです。「今日」はOS local timezoneの今日の開始から翌日の開始直前までです。
「期限切れ」「今日」「7日以内」のbadgeもcoreのOverdue／DueWithin queryへの所属から決めます。
完了済みにはdeadline status badgeを付けません。frontendで期限の比較やAssignmentの再sortはしません。

responseはfiltered itemsに加えて、filter前のtotal countと参照されている全Courseの選択肢を返します。
DBを一度だけ読み、同じsnapshotとclockからitems・Course候補・件数・badgeを構成します。
Courseの順序はcoreのCourse sort（表示ラベル→CourseId）、未所属は最後です。
groupのidentityはCourseIdで、同名・同sourceでも別IDをmergeせず、group内は選択したcore sort順を保ちます。
Moodleは「表示ラベル」と明示します。DateOnlyは日付のみ、DateTimeはlocal timezone、Noneは「期限なし」です。

filter/sort変更と完了切り替え成功後はRustへ再queryします。
保存成功後に再queryだけ失敗した場合は保存済みと通知し、再取得が成功するまでcheckboxを無効にします。
DB自体が空の状態とfilter結果だけ空の状態を区別し、loading中も前の一覧を保持します。
query失敗時は前回成功した条件へ戻して前の一覧を表示します。errorは固定messageのみです。

## Settings and secrets

「設定」画面でMoodle calendar URL、WebClass大学ID/passwordを保存・置換・個別削除できます。
保存した直後は同期しません。「同期」button、または次回の起動時同期で利用します。
保存後は「設定済み」だけを表示し、保存済みURL/ID/passwordをfrontendへ返しません。
secret入力は送信時、command終了時、画面切り替え時にclearし、失敗時も再入力が必要です。
localStorage/sessionStorage/IndexedDBやJS stateへのsecret保存はありません。
WebViewはincognitoで起動し、Windowsのgeneral autofillも無効です。

非secret設定は`app.path().app_config_dir()`の`settings.json`へ保存します。
内容はversion、`moodleScope: default.course-related`、`moodleExportKind: course_related`、`syncOnStartup`です。
single university/profile/feedの固定contractで、token URLの置換・削除によってscopeは変わりません。
`MoodleFeed::new`でHTTPS・host・userinfoを検証し、coreのscope/export-kind suffix contractを守ります。
同じdirectoryのtemporary fileをsyncしてatomicに作成・置換します。破損・未知version・変更されたidentity・
未知fieldを含む既存設定は固定errorにし、上書きやsilent resetはしません。

secret操作のcontractはshared applicationの`CredentialStore`へ集約し、OS実装はdesktop adapterに置きます。
maintainedな[keyring-core](https://docs.rs/keyring-core/1.0.0/keyring_core/)とOS providerを使用します。
Windowsは[Windows Credential Manager](https://docs.rs/windows-native-keyring-store/1.1.0/windows_native_keyring_store/)
（Local persistence）、macOSは[login Keychain](https://docs.rs/apple-native-keyring-store/1.0.2/apple_native_keyring_store/keychain/)です。
production serviceは`io.assignment-manager.desktop`、accountは固定の`moodle-feed-v1`と
`webclass-credentials-v1`です。大学IDをentry名やOS metadataに使いません。
WebClassのID/passwordは単一secret recordにまとめ、置換は旧entryを先に削除せず直接writeします。
missing entryは正常な未設定で、他appのentryを列挙する処理はありません。

blockingな設定/credential操作は専用settings workerで順に実行します。既存SQLite workerとは独立し、
core/schema v3にdesktop APIや設定/credential tableは追加していません。
Rustのsecret wrapperはDebug/Clone/Serializeを実装せず、owned bufferをdrop時にzeroizeします。
libraryのraw error/causeやsecretのdeserialize errorは公開せず、固定のapplication errorへ変換します。

追加commands: `get_settings_status`、`save_moodle_feed`、`remove_moodle_feed`、
`save_webclass_credentials`、`remove_webclass_credentials`。
status DTOは`moodleConfigured`/`webclassConfigured`/`syncOnStartup`のbooleanのみです。
mutationはwrite/delete成功だけを返し、その後のstatus read失敗と保存失敗が混ざらないようにしています。

## Desktop sync

一覧画面の「同期」は設定済みsourceだけを実行します。未設定なら通信はありません。
Moodle/WebClassは独立した結果を返し、一方の失敗でももう一方を実行します。
結果は未設定・成功（新規/更新/変更なしの件数）・固定error分類だけです。
scope、external identity、URL、HTTP body、credential、raw causeをIPC/logへ出しません。

[async Tauri command](https://v2.tauri.app/develop/calling-rust/)は専用sync workerへ要求を送り、
workerが同じ`app_data_dir / assignment-manager.db`を一度openします。
一覧/toggle workerのConnectionとは別で、network待ちでもローカル操作を利用できます。
coreの5秒busy timeoutと短い`BEGIN IMMEDIATE` transactionを再利用し、schema v3は変更しません。
atomicなsingle-flight admissionによりstartup/manual/連打は重複実行せず、追加runをqueueへ積みません。
IPC待ちがcancelされても実行中のsyncが終わるまでsingle-flightを維持します。

secretは各sourceの処理直前にSettings worker経由でOS storeから読みます。
Moodleは固定scope/`CourseRelated`を使い、`sync_moodle_once`へ接続します。
URL rotationはsource identityを変えません。URLをrun間でcacheしません。
WebClassは初回に`HoseiAuthenticator::login`し、以降はメモリだけにsessionを保持します。
`sync_webclass_all_with_reauth`がAuthExpired時のcredential取得・再login・一度のretryを担当します。
Settingsでcredentialを保存/削除した成功時だけrevisionを進め、次syncで旧sessionを破棄します。
cookie jar/sessionをdiskへ保存しません。再起動後は次syncでloginします。

設定の更新は[`NamedTempFile::persist`](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist)
による同directory内のatomic置換を使用します。
「起動時に同期する」は既定OFFで、旧settings fileのfield省略もOFFとして読めます。
起動時は最初のローカル一覧取得→設定確認→background sync→一覧再queryの順です。
frontendの`run_startup_sync`要求に対し、application runtimeが設定とonce guardを判定します。
View再生成・複数window・同じhandleのcloneでも、起動時policyの実行はapplication lifetimeで一度だけです。
同期失敗は起動を妨げません。完了操作もofflineで利用できます。
実行したsourceがあれば失敗/partial failureでも現在のfilter/sortで一覧を再queryします。
query/toggle中の同期完了によるreloadは処理終了後に実行します。
同期はlocal user-owned CompletionStateを変更しません。

`Failed`でも既にcommit済みのgroupが存在し得ます。`Completed`はrunの終了であり全source成功ではありません。
completion toggleの戻り値はcommit済み状態です。その後のrefresh失敗をtoggle失敗として再試行しません。
終了時は新規requestを拒否し、accepted jobが終わってConnection/session/adapterがdropされるまで非blockingで待ちます。

## Checks

core（Tauri依存の取得なしでも実行可能）:

```sh
cargo fmt --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline
cargo test --doc --locked --offline
```

shared application:

```sh
cargo fmt --manifest-path crates/application/Cargo.toml --check
cargo clippy --manifest-path crates/application/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/application/Cargo.toml --locked --offline
cargo test --doc --manifest-path crates/application/Cargo.toml --locked --offline
```

desktop（初回は依存の取得が必要、以降は`--offline`を付けられます）:

```sh
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked --offline
cargo build --manifest-path src-tauri/Cargo.toml --locked --offline --features custom-protocol
```

Node.jsがある場合、外部test frameworkなしで表示helperも検証できます:

```sh
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs
```

Rust testsはDTO、固定error境界、DB path/directory failure、request→core options、
local time window、statusの境界・completion除外、filter後のCourse候補保持、
worker終了→再起動後のcompletion保持を検証します。test用DBはtemporary directoryのみです。
settings testsはfake credential backendだけを使用し、save/load/delete、missing、置換失敗時の保持、
partial/corrupt record、error分類、設定roundtripと破損保持を検証します。
literal secretが設定file、SQLite、status DTO、error、Debugへ出ないことも検証します。
frontend testsはconfigured表示、save/remove request、入力clear、secretをstateへ残さないこと、固定errorを検証します。
unit testが実OS credential storeへ書き込むことはありません。
shared application testsはfake transport/providerのみを使用し、source選択、partial failure、
初回login/session再利用、core reauth callbackへの接続、credential変更/削除、single-flight、
network待ち中のlocal query/toggle、sync後・DB reopen後のcompletion保持を検証します。
startup once、shutdown/最後のhandle drop、settings thread ownership、別Connectionのcommitを挟んだ
snapshot consistencyもshared applicationで検証します。desktopにはJSON/設定file/credential backendのadapter testsを残します。
core HTTP testsがreauth protocol/retryをlocalhost mockで検証します。
frontend testsは進行状態/連打、source別成功・失敗、reloadの順序、startup policyへの委譲を検証します。
通常test suiteは大学サーバーへ接続しません。

Phase 8D1の依存追加時だけcrates.ioへ接続してdesktop lockfileを更新しました。
以降のcore/desktop checksは`--locked --offline`で実行しています。

## Windows smoke test

本番DBに触れずに操作を確認する専用binaryがあります。通常buildには含まれません。
PowerShellで、rootから次を実行します（WebView2が必要）:

```powershell
$smokeDirectory = Join-Path (Get-Location) 'src-tauri/target/list-smoke-data'
cargo run --manifest-path src-tauri/Cargo.toml --bin list-smoke --locked --offline --features custom-protocol,smoke-test -- $smokeDirectory
```

専用directory内にsynthetic assignmentsと同名のMoodle/WebClass Courseを初回だけ作成します。
WebView2 dataもこのdirectory内に分離し、再起動で同じtest DBを使います。
Settingsも同じRust実装・実OS credential backendを使用しますが、serviceは
`io.assignment-manager.settings-smoke`に分離します。本番credential entryは変更しません。
空DBを作成済みのdirectoryには課題を追加しません。大学サーバーへの接続はありません。
同期transportもfakeに置換し、設定済みsourceからsynthetic課題だけをimportします。
同期処理は各sourceで2秒待ち、進行表示とlocal操作を確認できます。
上記commandのdirectoryの後に`partial`を付けるとWebClass認証失敗、`failure`なら両source失敗になります。
WindowsのGUI resource/manifestが必要なため、Cargo exampleとして実行しないでください。

確認項目: filter/sort変更、Course選択、同名Courseの別group、未所属group、
toggle成功後に未完了filterから消えること、filter結果0件、狭いwindowでtoolbarがwrapすること、
再起動後のcompletion保持とdefault条件復帰。
Settingsの確認項目: synthetic HTTPS URL/ID/passwordの保存、入力clear、app再起動で両方が設定済み、
置換、各sourceの削除、「状態を確認」で両方が未設定になること。終了前にsynthetic secretsを個別削除してください。

Phase 8D1: Windowsで上記の実credential backend操作を検証。macOS実機/Keychainは未検証です。

本番大学サーバーのmanual smokeは、通常のdesktop binaryでOS storeに保存した設定を使い、
「同期」から個別/両source取得、再同期でduplicateなし、toggle→再同期でCompleted保持、
再起動でローカル一覧即表示を確認します。URL/ID/passwordをcode、fixture、env fileへ書かないでください。
実接続を通常testsやoffline smoke binaryへ組み込まないでください。

Phase 8D2（Windows、2026-10-04）: 実server manual smokeでMoodle 15件、WebClass 146件を取得。
再同期は全161件が変更なしで、toggle→sync→app restartでもCompletedを保持しました。
検証用toggleは元に戻しました。初回Moodle通信失敗時もWebClass成功分を表示できました。
起動時同期のON保存・再起動・自動実行は隔離したoffline smokeで確認済みです。
本番startup policyはOFFのまま、synthetic OS credentialは削除済みです。macOSは未検証です。

## Remaining

- MVP stabilization: UI整理、source submission state / mismatch warning設計、sync status、timetable、Course management
- macOS native: SwiftUI shell、Rust bridge、Keychain、Assignment UI、timetable、WidgetKit
- macOS実機/Keychainは未検証。今回macOS固有UIやSwiftUI実装は追加していません。
