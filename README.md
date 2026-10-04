# Assignment Manager

Rust coreと、Phase 8D1のWindows/macOS向けTauri 2 desktop shellです。

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
coreはroot crateのまま、desktopは独立manifest/lockfileで依存します。

初回DBは空です。「課題はまだありません」は正常な状態です。
このPhaseには課題作成、同期、DB選択UIはありません。

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
Courseの順序はcoreのCourse sort（表示ラベル→CourseId）、未所属は最後です。
groupのidentityはCourseIdで、同名・同sourceでも別IDをmergeせず、group内は選択したcore sort順を保ちます。
Moodleは「表示ラベル」と明示します。DateOnlyは日付のみ、DateTimeはlocal timezone、Noneは「期限なし」です。

filter/sort変更と完了切り替え成功後はRustへ再queryします。
保存成功後に再queryだけ失敗した場合は保存済みと通知し、再取得が成功するまでcheckboxを無効にします。
DB自体が空の状態とfilter結果だけ空の状態を区別し、loading中も前の一覧を保持します。
query失敗時は前回成功した条件へ戻して前の一覧を表示します。errorは固定messageのみです。

## Settings and secrets

「設定」画面でMoodle calendar URL、WebClass大学ID/passwordを保存・置換・個別削除できます。
保存してもMoodle fetch、WebClass login、startup syncは行いません。大学サーバーへの接続はありません。
保存後は「設定済み」だけを表示し、保存済みURL/ID/passwordをfrontendへ返しません。
secret入力は送信時、command終了時、画面切り替え時にclearし、失敗時も再入力が必要です。
localStorage/sessionStorage/IndexedDBやJS stateへのsecret保存はありません。
WebViewはincognitoで起動し、Windowsのgeneral autofillも無効です。

非secret設定は`app.path().app_config_dir()`の`settings.json`へ保存します。
内容はversion、`moodleScope: default.course-related`、`moodleExportKind: course_related`のみです。
single university/profile/feedの固定contractで、token URLの置換・削除によってscopeは変わりません。
`MoodleFeed::new`でHTTPS・host・userinfoを検証し、coreのscope/export-kind suffix contractを守ります。
同じdirectoryのtemporary fileをsyncしてatomicに新規作成します。破損・未知version・変更されたidentity・
未知fieldを含む既存設定は固定errorにし、上書きやsilent resetはしません。

secretはdesktop内部の`CredentialStore`境界に集約しています。
maintainedな[keyring-core](https://docs.rs/keyring-core/1.0.0/keyring_core/)とOS providerを使用します。
Windowsは[Windows Credential Manager](https://docs.rs/windows-native-keyring-store/1.1.0/windows_native_keyring_store/)
（Local persistence）、macOSは[login Keychain](https://docs.rs/apple-native-keyring-store/1.0.2/apple_native_keyring_store/keychain/)です。
production serviceは`io.assignment-manager.desktop`、accountは固定の`moodle-feed-v1`と
`webclass-credentials-v1`です。大学IDをentry名やOS metadataに使いません。
WebClassのID/passwordは単一secret recordにまとめ、置換は旧entryを先に削除せず直接writeします。
missing entryは正常な未設定で、他appのentryを列挙する処理はありません。

blockingな設定/credential操作は専用settings workerで順に実行します。既存SQLite workerとは独立し、
core/schema v3にdesktop APIや設定/credential tableは追加していません。
Rustのsecret wrapperはDebugをredactし、owned bufferをdrop時にzeroizeします。
libraryのraw error/causeやsecretのdeserialize errorは公開せず、固定のapplication errorへ変換します。

追加commands: `get_settings_status`、`save_moodle_feed`、`remove_moodle_feed`、
`save_webclass_credentials`、`remove_webclass_credentials`。
status DTOは`moodleConfigured`/`webclassConfigured`のbooleanのみです。
mutationはwrite/delete成功だけを返し、その後のstatus read失敗と保存失敗が混ざらないようにしています。

## Checks

core（Tauri依存の取得なしでも実行可能）:

```sh
cargo fmt --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline
cargo test --doc --locked --offline
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
node --test tests/frontend.test.mjs tests/settings.test.mjs
```

Rust testsはDTO、固定error境界、DB path/directory failure、request→core options、
local time window、statusの境界・completion除外、filter後のCourse候補保持、
worker終了→再起動後のcompletion保持を検証します。test用DBはtemporary directoryのみです。
settings testsはfake credential backendだけを使用し、save/load/delete、missing、置換失敗時の保持、
partial/corrupt record、error分類、設定roundtripと破損保持を検証します。
literal secretが設定file、SQLite、status DTO、error、Debugへ出ないことも検証します。
frontend testsはconfigured表示、save/remove request、入力clear、secretをstateへ残さないこと、固定errorを検証します。
unit testが実OS credential storeへ書き込むことはありません。

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
WindowsのGUI resource/manifestが必要なため、Cargo exampleとして実行しないでください。

確認項目: filter/sort変更、Course選択、同名Courseの別group、未所属group、
toggle成功後に未完了filterから消えること、filter結果0件、狭いwindowでtoolbarがwrapすること、
再起動後のcompletion保持とdefault条件復帰。
Settingsの確認項目: synthetic HTTPS URL/ID/passwordの保存、入力clear、app再起動で両方が設定済み、
置換、各sourceの削除、「状態を確認」で両方が未設定になること。終了前にsynthetic secretsを個別削除してください。

Phase 8D1: Windowsで上記の実credential backend操作を検証。macOS実機/Keychainは未検証です。

## Remaining

- Phase 8D2: Manual Sync now、Moodle sync、WebClass sync + automatic reauth、configured sourceごとのsync、progress/result/safe error UI、startup sync policy
- After Phase 8D2: macOS smoke test、Keychain verification、macOS build / bundle verification
- Later: timetable、sync status、Course management / merge
