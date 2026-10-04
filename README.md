# Assignment Manager

Rust coreと、Phase 8C2のWindows/macOS向けTauri 2 desktop shellです。

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
このPhaseには課題作成、同期、設定、credentials、DB選択UIはありません。

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
node --test tests/frontend.test.mjs
```

Rust testsはDTO、固定error境界、DB path/directory failure、request→core options、
local time window、statusの境界・completion除外、filter後のCourse候補保持、
worker終了→再起動後のcompletion保持を検証します。test用DBはtemporary directoryのみです。

## Windows smoke test

本番DBに触れずに操作を確認する専用binaryがあります。通常buildには含まれません。
PowerShellで、rootから次を実行します（WebView2が必要）:

```powershell
$smokeDirectory = Join-Path (Get-Location) 'src-tauri/target/list-smoke-data'
cargo run --manifest-path src-tauri/Cargo.toml --bin list-smoke --locked --offline --features custom-protocol,smoke-test -- $smokeDirectory
```

専用directory内にsynthetic assignmentsと同名のMoodle/WebClass Courseを初回だけ作成します。
WebView2 dataもこのdirectory内に分離し、再起動で同じtest DBを使います。
空DBを作成済みのdirectoryには課題を追加しません。大学サーバーへの接続はありません。
WindowsのGUI resource/manifestが必要なため、Cargo exampleとして実行しないでください。

確認項目: filter/sort変更、Course選択、同名Courseの別group、未所属group、
toggle成功後に未完了filterから消えること、filter結果0件、狭いwindowでtoolbarがwrapすること、
再起動後のcompletion保持とdefault条件復帰。

Phase 8C2: Windowsで検証。macOS実機は未検証で、Phase 8D完了後に確認します。

## Remaining

- Phase 8D: Settings、Moodle feed configuration、WebClass credentials、OS credential store、Sync now、startup / manual sync
- After Phase 8D: macOS smoke test、Keychain verification、macOS build / bundle verification
- Later: timetable、sync status、Course management / merge
