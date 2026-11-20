# Assignment Manager

## Notion単発CLI

macOSとWindowsで使える単発CLIです。合成dry-runでは、固定ループバックの大学HTTPS fixtureから
MoodleとWebClassを取得し、Notion HTTPモックを読み取って差分の件数を表示します。

```powershell
cargo run --locked --offline --features synthetic-https-fixture -- sync --dry-run --fixture
cargo run --locked --offline --features synthetic-https-fixture -- status --fixture
```

`init-state --fixture` で合成用の状態DBを明示的に初期化できます。dry-runはDBを
読み取り専用で使い、未初期化なら作りません。前回結果として、時刻・取得元別の結果・
差分件数・固定エラー分類だけを `last-dry-run.json` に記録します。
同時実行はOSファイルロックで拒否します。強制終了後は同じロックファイルを再利用できます。

ローカル設定はmacOSでは `~/Library/Application Support/AssignmentManagerNotion/config.json`、
Windowsでは `%LOCALAPPDATA%\AssignmentManagerNotion\config.json` に置きます。
コマンドの前に `--config PATH` を指定することもできます。次は架空のIDを使った設定例です。
プロパティには表示名と区別してAPIのIDを指定します。

```json
{
  "notion": {
    "assignments_data_source_id": "00000000-0000-4000-8000-000000000001",
    "courses_data_source_id": "00000000-0000-4000-8000-000000000002",
    "properties": {
      "title": "title", "deadline": "due", "course": "course",
      "submission_url": "url", "sync_key": "sync", "source": "source"
    }
  },
  "moodle": {
    "host": "moodle.example.invalid",
    "path": "/calendar/export",
    "scope": "default.course-related"
  },
  "webclass": true
}
```

`doctor` は設定、OSの資格情報ストアの登録状態、専用DBをローカルで診断します。
`credentials set moodle`、`credentials set webclass`、`credentials set notion` は
対話端末の非表示入力で登録します。既存項目の上書きは `y` で承認します。
キャンセル時は既存項目を保持し、`credential_cancelled` と終了コード `2` を返します。
設定ファイルには秘密情報を入れません。
`init-state` と `status` が使う状態DBは `targets/<data-source-id>/state.sqlite3` にあり、
既存アプリのSQLite v3と分離しています。通常の `sync --dry-run` はローカル設定と
資格情報の検査で止まります。大学データの取得には合成fixtureを使用します。

終了コードは成功 `0`、実行失敗 `1`、設定・資格情報などの不足 `2`、
部分成功または確認待ち `3`、多重実行 `4` です。

### macOSでのビルドとローカル検証

Xcode Command Line ToolsとRustを入れ、初回は `cargo fetch --locked --target "$(rustc --print host-tuple)"` で依存関係を取得します。

```sh
cargo build --locked --offline --features synthetic-https-fixture
target/debug/assignment-manager sync --dry-run --fixture
target/debug/assignment-manager status --fixture
```

実Notionの読み取り検証に向けて、上記の `notion` 設定に課題と授業のデータソースIDを
指定します。`properties.title` は `title` です。期限、授業Relation、提出URL、
「同期キー」（`rich_text`）、「取得元」（`select`、`Moodle` / `WebClass`）には
Notion APIが返すプロパティIDを指定します。大学の合成fixtureを使う場合は、設定から
`moodle` と `webclass` を省略できます。

```sh
target/debug/assignment-manager doctor
target/debug/assignment-manager credentials set notion
target/debug/assignment-manager doctor
```

トークンは対話端末の非表示プロンプトで入力します。CLI専用のサービス名
`io.assignment-manager.notion-cli.v1.<正規化した課題データソースID>`、アカウント名 `notion` で
ログインキーチェーンに保存します。CLIのアクセスをmacOSから確認されたら、実行中のCLIに
対するアクセス要求であることを確認して許可します。未登録は `notion=missing`、不正な値は
`notion=invalid`、キーチェーンにアクセスできない場合は `notion=unavailable` と表示します。
`doctor`、`notion-check`、`sync --dry-run` は、標準入力か標準エラーが端末に接続されていない場合、
キーチェーンの認証UIを表示しません。承認が必要な場合は `credential_unavailable`、
未登録の場合は `credential_missing` を返します。いずれも終了コードは `2` です。
状態DBが未初期化のときの `state=missing pending=0` は正常です。

実トークンを使わずにログインキーチェーンを検証するには、次のテストを実行します。
テスト専用サービス名で登録、更新、非対話の取得、アクセス拒否と未登録の区別を検査します。

```sh
cargo test --locked --offline --bin assignment-manager macos_keychain_round_trip -- --ignored
```

### Notionの読み取り専用検査とdry-run

対象ワークスペースのユーザーから読み取りの明示的な許可を得た後に実施します。
Notion Internal connectionに読み取り権限を付与し、課題と授業の元データソースを共有します。
Notionの「データソースを管理」から両方のデータソースIDを取得し、ローカル設定へ記載します。
`credentials set notion` を対話端末から実行して登録したトークンを使います。
次の明示オプションで、公式の `https://api.notion.com` に接続します。
メタデータと個別ページのGET、および一覧を読む
`POST /v1/data_sources/{data_source_id}/query` を利用します。
この読み取り経路からは作成・更新・削除・アーカイブやスキーマ変更を送信しません。

```sh
cargo run --locked --offline -- notion-check --allow-notion-read
cargo run --locked --offline --features synthetic-https-fixture -- sync --dry-run --fixture --allow-notion-read
```

上の設定例から、存在しない `properties.sync_key` と `properties.source` は省略できます。
`notion-check` は型・権限・Relation先を検査し、任意の `sample_assignment_page_ids`
に指定した課題ページとRelationの全項目をGETで確認します。状態、優先度、科目、種別、
繰り返し、メモは現在の列名と型を診断します。Notion UIの「テキスト」はAPI上の
`rich_text` です。ページタイトル、トークン、生ID、URLは診断出力に含めません。

スキーマが適合すると、`notion-check` は全件取得後に同期管理ページ、手動ページ、
重複キーの件数を表示します。`sync --dry-run --fixture` は合成大学データと照合し、
Create / Update / Unchanged / Conflict / Deferred の件数を表示します。
`--allow-notion-read` を省いたfixture実行はループバックのNotionモックを使います。
スキーマ不足、途中ページの失敗、ページの重複、APIの件数上限などで取得の完全性を
確認できない場合は差分を確定せず、終了コード `1` を返します。同期キーの重複や
状態DBとの対応の食い違いで保留した場合は `3` を返します。
同期状態DBは読み取り専用で開きます。件数と固定分類だけを別の
`last-dry-run.json` に記録します。

Phase 3Cの合成HTTPモックでの作成・再同期、障害復旧、実書き込みの有効化条件は
[Notion同期 Phase 3C](docs/NOTION-SYNC-PHASE-3C.md)を参照してください。

### macOSの検証用Notionへの試験同期

`notion-trial --target ID` は独立した検証用課題・授業データソースと合成課題1件を使います。
既定はdry-runです。書き込みには `--allow-notion-write` と、表示された計画の対話承認が必要です。
検証環境の準備・共有と試験書き込みには別途承認が必要です。設定例と検証手順は
[Notion同期 Phase 3D](docs/NOTION-SYNC-PHASE-3D.md)を参照してください。

### 授業の対応表と既存課題の移行案

`course_mappings` は人が確認するローカル設定です。たとえば以下のように
出所・スコープ・外部IDの完全一致を、確認済みの授業ページIDに対応づけます。

```json
"course_mappings": [
  {
    "course_key": {
      "origin": "WebClass",
      "scope": "default",
      "id": "example-group-2026"
    },
    "page_id": "00000000-0000-4000-8000-000000000003"
  }
]
```

WebClassの外部IDはグループID、Moodleはフィードスコープ内の授業ラベルです。
年度や取得範囲で別の授業になる場合は、対応表を作る前に外部キーの区別を確認します。
対応先を一意に確認できた場合、新規課題と「授業」が空欄の同期管理課題にだけRelationを
設定する案を作ります。設定済みのRelationと手動課題は保持します。同期管理課題の
設定済みRelationが対応先の1件と一致しない場合は `course_relation_mismatch` に計上し、
課題名や期限の差分は引き続き更新案に含めます。対応先が曖昧な場合は該当課題を保留します。

1. 承認後に「同期キー」をテキスト（`rich_text`）、「取得元」を単一選択
   （`select`、既存の選択肢 `Moodle`・`WebClass`）として用意し、各IDを設定へ記載します。
   既存プロパティとページ値は維持します。
2. 読み取りが許可された環境で差分を確認します。手動課題は同期キー空欄のまま共存できます。
   重複候補を既存課題へ引き継ぐ場合は、対象のページIDと大学課題の完全な外部キーを
   一対一で指定し、期限、授業、取得元を照合します。タイトル一致だけの統合は行いません。
3. 紐づけ前に変更予定を表示し、同期キーとページIDの重複、移動、ゴミ箱入り、
   未確認のRelationがある組は保留します。完了状態、メモ、優先度、繰り返し、本文は
   移行の更新対象から外します。Phase 3B.5では移行案と読み取り専用の検査・差分計画を提供します。

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

Web-4Aの合成データ専用Browser UI + 非同期Sync API PoCは[crates/web-server](crates/web-server/README.md)にあります。
serverを停止し、次の明示的な管理操作を順に実行します。

```powershell
cargo run --manifest-path crates/web-server/Cargo.toml --locked -- bootstrap-admin
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline -- init-vault-key
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline
```

Web専用ID/passwordはterminalの非表示入力で作成します。`init-vault-key`はWindows Credential Managerの
Web専用serviceへ256-bit鍵を生成し、既存鍵・既存Vaultは上書きしません。通常起動で鍵を自動生成しません。
鍵はcontrol DBやGitに含めず、Tauriのcredential entryとも分離します。
[ローカルUI](http://127.0.0.1:3000/)で課題一覧・完了変更・Settingsを利用できます。
Settingsの保存・置換・削除にはWeb専用passwordによる再認証（5分）が必要です。
**合成URL/ID/passwordだけを使用してください。大学通信・実同期は無効です。**
AES-256-GCMでUserId/record種別に束縛した認証情報と課題completionは再起動後も保持します。
server再起動ではWeb sessionを失効させ、再ログインを要求します。課題schema v3、desktop/native DBは維持します。
**127.0.0.1:3000のHTTP開発専用です。外部公開・LAN bind・reverse proxy・tunnel運用は禁止です。**
本番HTTPS/Secure Cookie、proxy trust、production鍵provider/ACL、backup/restoreは未実装です。
Web-4B1で大学HTTP用の安全なtransport constructorとoffline testsを追加しました。通常Web binaryへの接続は無効のまま、Web-4B2で明示注入用backendを統合しました。
[Web認証](docs/WEB-AUTHENTICATION.md)、[Vault設計](docs/WEB-CREDENTIAL-VAULT.md)、
[Web-4A報告](docs/PHASE-WEB-4A.md)、[frontend統合](docs/FRONTEND-INTEGRATION.md)を参照してください。
非同期Sync API・bounded polling・部分失敗時の再取得を実装し、通常serverはDisabledNetworkBackendを維持します。
mockは専用synthetic-sync featureの隔離test/exampleでのみ明示注入します。Windows検証の未完了項目はWeb-4A報告を参照してください。
[Web-4B1報告](docs/PHASE-WEB-4B1.md)と[大学通信policy](docs/WEB-NETWORK-SECURITY.md)を参照してください。
Moodle実host/pathは未設定でfail closedです。Web-4B2は隔離offline検証まで完了し、実大学通信は行っていません。
Web-4AのWindows build/smokeは未完了で、Web-4A最終判定はBLOCKEDのままです。

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
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs tests/web_frontend.test.mjs
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


## Web-4B2 secured backend integration (2026-10-22)

共有applicationへ`SecuredCoreSyncBackend`とtrusted immutable NetworkPolicyを統合しました。
Moodle、WebClass初回login/reauth、cached sessionが同じsecured transport policyを使い、
既存sync/import/SQLite v3/completion/revisionの契約を維持します。Web専用のrun資源上限も追加しました。
通常Web binaryはall-featuresでもDisabledNetworkBackend固定、既存synthetic exampleはMockSyncBackendです。
CLI/envによる実backend切替、実大学通信、実credential入力、Web-4B3実装/実行は行っていません。

結果・changed files・資源上限とcancelの限界・隔離E2E・回帰・将来の実通信検証計画は
[Phase Web-4B2](docs/PHASE-WEB-4B2.md)を参照してください。
判定はReady for Web-4B3（Mac/offlineコード統合gate）。実通信の自動許可ではありません。
Web-4AのWindows build / Browser smokeは未完了、同phase最終判定はBLOCKEDのままです。

## Web-4B3 preflight (2026-10-22)

固定loopback HTTPS大学fixtureを使い、実Web HTTP APIの認証/Vault/coordinatorから
SecuredCoreSyncBackend、既存parser/import、SQLite、Browser一覧までの合成E2Eを追加しました。
通常binaryはDisabled、既存synthetic-syncはMockを維持します。大学通信・実credential使用・commit/pushは行っていません。

判定は **BLOCKED**。Moodle exact host/path、現行IdP/LMSと大学利用条件、Mac Web専用永続鍵管理、
実接続専用runtimeのreviewが残ります。実接続は別途明示承認が必要です。
[Preflight報告](docs/PHASE-WEB-4B3-PREFLIGHT.md)、[将来の手動smoke手順](docs/WEB-REAL-SYNC-SMOKE-PLAN.md)、
[隔離exampleの起動方法](crates/web-server/README.md#web-4b3-preflight--fixed-synthetic-https-runtime)を参照してください。
Windows Web-4A build/smoke未完了・BLOCKEDは引き続き維持します。
