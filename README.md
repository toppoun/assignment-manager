# Assignment Manager

Rust coreと、Phase 8C1のWindows/macOS向けTauri 2 desktop shellです。

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
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets --all-features -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked
cargo build --manifest-path src-tauri/Cargo.toml --locked --features custom-protocol
```

Node.jsがある場合、外部test frameworkなしで表示helperも検証できます:

```sh
node --test tests/frontend.test.mjs
```

Rust testsはDTO、固定error境界、DB path/directory failure、local time context、
empty list、worker終了→再起動後のcompletion保持を検証します。test用DBはtemporary directoryのみです。

手動確認: 起動→empty state、既存課題のdeadline/Course表示→checkboxで完了切り替え→
終了→再起動後も完了状態が保持されること。DateOnlyは日付のみ、DateTimeはOS local timezone、
締切なしは「締切なし」と表示します。MoodleのCourse表示ラベルは正式科目名を保証しません。

## Remaining

- Phase 8C2: filter controls、sort controls、Course grouping、overdue / upcoming表示、UI polish
- Phase 8D: settings、Moodle feed configuration、OS credential store、WebClass credentials、Sync now、initial / manual sync
- Later: timetable、sync status、Course management / merge
