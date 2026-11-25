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

大学取得だけをローカルで検証するには、合成HTTPSを使用します。

```sh
cargo run --locked --offline --features synthetic-https-fixture -- university-check --fixture
```

Moodleの実データ読み取り手順、WebClassの認証上の制約、両OSの検証条件は
[Notion同期 Phase 4A](docs/NOTION-SYNC-PHASE-4A.md)を参照してください。

### WebClassの授業一覧と1授業の課題を確認する

授業一覧と1授業の課題の確認には、WindowsではMicrosoft Edge、macOSではGoogle Chromeを使います。
PATH上のNode.jsが必要です。ブラウザで本人認証を行うときは、操作できるデスクトップセッションと対話端末で実行します。
実通信は、別途承認を得てから実行します。

```sh
cargo run --locked --offline -- university-check webclass --courses-only --allow-university-read
```

専用ブラウザで大学公式の認証と属性提供の判断を本人が行い、HOPPIIのWebClassリンクを開きます。
同じコンテキストから授業一覧を1回取得し、授業ID・名称・年度・学期・曜日・時限と、
年度・学期ごとの件数を端末に表示します。表示できる授業が現在履修中かどうかは別途確認します。
中止するときは専用ブラウザを閉じます。

対話端末では一時プロファイルが既定です。認証・同意状態を再利用する場合は、末尾に `--keep-profile` を付けます。
非対話の取得は、既存の専用プロファイルをヘッドレスで開きます。
Windowsでは `%LOCALAPPDATA%\AssignmentManagerWebClassEdge`、macOSでは
`~/Library/Application Support/AssignmentManagerWebClassChrome` に保持されます。
どちらも専用の `--user-data-dir` を使用し、一時プロファイルは実行ごとに作成して終了時に削除します。
保持プロファイルを使用中に別のCLIから起動すると `profile_in_use` で終了します。状態の期限が切れたら、次の明示的な実行で
公式画面から認証を行います。`--browser` は従来のコマンドとの互換のため利用できます。
設定ファイルがある場合はWebClassの有効・無効を確認します。ブラウザでの認証に登録済み資格情報は不要です。
[ブラウザ経路の通信と制約](docs/WEB-NETWORK-SECURITY.md#webclassのブラウザ認証)

#### macOSでの準備と合成テスト

Google Chromeを `/Applications` にインストールし、ログイン中のMacのターミナルから実行します。
CLIは `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` を直接起動します。
`chrome_unavailable` が表示された場合はインストール先と実行権限を確認してください。
`chrome_start_failed` の場合は、Chromeが起動できることと、専用プロファイルが使用中でないことを確認してください。
ソースからの実行にはRust/Cargoと取得済みの依存関係が必要です。Node.js 22で動作を確認しています。

本番通信の承認前に、ループバックの合成SSOで認証、授業取得、プロファイル再利用と異常終了を確認できます。

```sh
ASSIGNMENT_MANAGER_TEST_CHROME=1 node --test tests/webclass_edge.test.mjs
cargo test --locked --offline chrome_driver_cleans_up_when_the_cli_disconnects -- --ignored
```

#### 1授業の課題を確認する

授業一覧で表示された対象の `group_id` を指定します。本番の読み取りが承認された場合に実行します。

```sh
cargo run --locked --offline -- university-check webclass --course-id 'GROUP_ID' --allow-university-read
```

一時プロファイルが既定で、末尾に `--keep-profile` を付けると専用プロファイルを再利用します。
毎回、取得した一覧でIDが完全一致したときだけ、取得に使ったブラウザタブから対象授業の課題を1回取得します。
取得対象には提出済みの課題も含まれます。この確認コマンドでは完了状態の判定や更新は行いません。
表示項目は授業名とID、課題名とID、期限（日本時間）、提出先URLです。期限がない場合や、取得データから
提出先URLを確認できない場合は `null` と表示します。表示できる課題が0件の場合も取得と検証が成功すれば
終了コードは `0` です。IDの不一致、認証失効、応答の検証失敗は終了コード `1` です。

#### オフラインとHTTP診断

固定ループバックの合成HTTPSで、認証から授業一覧の検証まで確認できます。Windowsで値を確認する場合は
末尾に `--review-local` を付けます。合成経路は授業一覧GETを最大1回行い、空の一覧を
`validation=empty` と終了コード `1` で区別します。

```powershell
cargo run --locked --offline --features synthetic-https-fixture -- university-check webclass --courses-only --fixture
cargo run --locked --offline --features synthetic-https-fixture -- university-check webclass --courses-only --fixture --review-local
```

`--check-local` はブラウザ経路の事前確認です。通常のブラウザ実行と同じ設定チェックを行い、
WindowsではEdgeとPATH上の `node.exe`、macOSではChromeとPATH上の `node` を探します。
確認はローカルの設定と実行ファイルの検出だけで完結します。
別の設定を確認する場合は、`university-check` の前に `--config PATH` を指定します。

```sh
cargo run --locked --offline -- university-check webclass --courses-only --check-local
```

旧HTTP方式のSSO観測には、登録済みのOS資格情報を使う `--http-probe` を指定します。
`--check-local --http-probe` を組み合わせると、HTTP経路の設定と資格情報項目の有無をオフラインで確認できます。
HTTP診断用の設定には `credential_namespace` だけを指定できます。

```sh
cargo run --locked --offline -- --config PATH university-check webclass --courses-only --check-local --http-probe
cargo run --locked --offline -- --config PATH university-check webclass --courses-only --allow-university-read --http-probe
```

HTTP診断ではSAMLフォームのACS照合または同意画面の観測で停止します。
[HTTP診断の通信制限](docs/WEB-NETWORK-SECURITY.md#webclass授業一覧専用cli)を参照してください。

### Macの運用用CLIと非対話取得

開発には `cargo run` を使い、運用では署名を検証した固定パスのCLIを使います。
Rust、Xcode Command Line Tools、ロック済みのCargo依存関係を用意してから、次を実行します。

```sh
CLI="$(bash macos/install-operation-cli.sh)"
printf '%s\n' "$CLI"
```

スクリプトは `target/operation-build` でリリースビルドし、
`~/Library/Application Support/AssignmentManagerCLI/bin` の下にSHA-256別のディレクトリを作って配置します。
配置済みのCLIは上書きしません。別の配置先を使う場合は絶対パスを引数に渡します。
ビルド環境が管理している署名を使う場合は、`ASSIGNMENT_MANAGER_SIGN_IDENTITY` にその署名IDを指定できます。
更新時は同じ手順で新しいパスに配置し、次の非対話診断に通ってから実行先を切り替えます。
切り戻すときは、直前に動作したCLIの絶対パスを指定します。

キーチェーンの確認が出たら、今回配置したCLIからの要求であることを確認し、
MoodleとNotion Readerなど、そのCLIが担当する項目にだけ「常に許可」を与えます。
Writerの読み取り確認には以下を実行します。トークンの読み取りと形式検証だけを行い、結果を固定値で表示します。

```sh
"$CLI" --config local/notion-production.json credentials check notion-writer
"$CLI" --config local/notion-production.json credentials check notion-writer </dev/null
```

最初の実行はアクセスが未許可のときに対話端末で行います。2行目が
`production_writer=ready network=disabled` なら、その同じCLIで非対話の読み取りが可能です。
`credential_missing` は対象項目が未登録、`credential_unavailable` は許可や解錠などが必要な状態です。
ReaderとMoodleはそれぞれの読み取り専用コマンドで確認できます。
CLI更新後はこの確認を繰り返します。キーチェーンへの許可は必要な項目と対象CLIに限定します。

WebClassで実アカウントを用いた自動取得の可否は大学に確認してください。開発・回帰テストにはローカル模擬環境を使います。
利用条件が確認できた環境では、本人が対話端末で次を実行して、大学公式画面で認証・同意し、HOPPIIのWebClassリンクを開きます。
WebClassのログイン済み画面を検出するとChromeを閉じて専用プロファイルを保存します。この手順では授業・課題APIの取得はありません。

```sh
"$CLI" --config local/notion-production.json university-check webclass --authenticate --allow-university-read
```

通常の取得は次のように実行できます。コマンドは授業ルール・課題の読み取りと差分の確認を行います。

```sh
"$CLI" --config local/notion-production.json sync --dry-run --allow-university-read --allow-notion-read --production-read-only --headless
```

授業一覧や1授業の確認にも、従来の診断コマンドで末尾に `--headless` を指定できます。
非対話のブラウザ取得では既存の専用プロファイルでWebClassへ直接接続します。
実行中は排他ロックを保持します。macOSでは専用ディレクトリを `0700` で作り、開く際に所有者とアクセスモードを検査します。
`reauth_required` または `auth_expired` の場合は取得を中止し、利用者が `--authenticate` で認証し直します。
`access_denied` はアクセス拒否、`network_failed` と `browser_navigation_failed` は通信・遷移失敗、
`server_unavailable` は429または5xx、`invalid_course_response` などは応答形式不一致です。
認証状態を確認できない画面は `webclass_ready_timeout` で停止します。失敗した取得を空の授業・課題として扱いません。

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

#### Macで実課題を読み取る検証用dry-run

授業対応表を設定した検証用Notionを指定し、読み取り用IntegrationとMoodleの授業関連カレンダーURLを
同じ設定先のキーチェーンに登録します（`credentials set notion`、`credentials set moodle`）。
対話端末で次を実行し、専用Chromeで大学公式画面の認証を完了してHOPPIIのWebClassリンクを開きます。

```sh
cargo run --locked --offline -- --config local/notion-verification.json sync --dry-run --allow-university-read --allow-notion-read
```

Moodleの授業関連ICSを読み、HOPPIIは確定ルールの `group_id` と学期が授業一覧に一致した授業だけから課題を取得します。
取得後にNotionの課題・授業対応表を読み直し、取得元別・授業別の件数、対応先、対象外と保留の内訳、同期計画を表示します。
取得できなかった授業やルールの要確認事項があれば終了コード `3` を返します。キーチェーンの承認が必要な場合は、
対話端末に表示されるmacOSの確認画面で実行中のCLIへのアクセスを許可します。
対象は検証用の3データソースに限定され、Notionや状態DBへの書き込みは行いません。

検証用課題DBへの適用には、検証用Writerの課題DBに対する読み取り・追加権限が必要です。
課題名・期限の更新を試す場合は、課題DBへの更新権限も必要です。
最新のdry-runで確認した件数を指定します。次の例は新規34件と変更なし1件の計画です。

```sh
cargo run --locked --offline -- --config local/notion-verification.json sync --apply --allow-university-read --allow-notion-read --allow-notion-write --expect-create 34 --expect-unchanged 1
```

課題名・期限の更新を許可する場合は、同じコマンドに `--expect-update N` を追加します。省略時の期待値は0件です。
適用前に最新の取得データとNotionを照合し、件数や計画が変わった場合、または保留・競合がある場合は停止します。
適用後は登録内容をNotionから再取得し、同じ取得データを使って再dry-runと再適用を行います。
Notionが期限を分精度で返した場合は、取得元の期限と同じ分であれば一致として扱います。
既存の課題ページでは、同期キーとページを再確認したうえで課題名・期限の差分だけを書き込みます。
更新の応答を失った場合は、同じリクエストを再送せず、ページを読み直して更新結果を確認します。
作成結果が不明な場合は、次回に完全なNotion一覧から同期キーを照合します。一意に確認できたページを再利用し、
該当ページを確認できない場合や複数一致した場合は保留します。

#### Macで本番用Notionを読み取り専用で確認

本番用は別の設定ファイル、課題データソースIDごとのキーチェーン項目と状態DBを使います。
課題DB・授業DB・授業対応表を本番の読み取り用Integrationに共有し、授業DBの同期用学期をテキスト型で設定します。
`course_rules.course_term` にそのプロパティIDを指定してください。

```sh
cargo run --locked --offline -- --config local/notion-production.json notion-check --allow-notion-read
cargo run --locked --offline -- --config local/notion-production.json sync --dry-run --allow-university-read --allow-notion-read --production-read-only
```

`--production-read-only` は登録済みの本番用3データソースを照合し、本番用状態DBを読み取り専用で参照します。

#### Macで本番へ初回同期する

本番専用WriterのIntegrationに `Read content` と `Insert content` を設定し、本番の課題DBと授業DBを個別に接続します。
ターミナルで次のコマンドを実行し、非表示プロンプトでWriterのトークンを入力します。
登録時にBot IDと本番のスキーマを確認し、トークンをReaderと別のキーチェーン項目に保存します。
登録時の `production_writer=failed` に `stage` と `http_status` が表示された場合は、対象のAPIとデータソースIDを確認してください。
`assignments_data_source` と `courses_data_source` は元の課題DBと授業DBへの共有をそれぞれ確認します。授業対応表はReaderが参照します。

```sh
cargo run --locked --offline -- --config local/notion-production.json credentials set notion-writer
cargo run --locked --offline -- --config local/notion-production.json sync --apply --allow-university-read --allow-notion-read --allow-notion-write --production-first-sync --writer BOT_ID
```

新規作成の期待件数は既定で35件です。最新のdry-runで新規件数を確認したうえで、`--expect-create N` で期待件数を指定できます。
初回の計画で新規件数が期待値と一致し、更新・変更なし・保留・競合0件と既存の手動課題1件を確認できた場合に、対話承認のプロンプトが表示されます。
表示された計画を確認してから `APPLY PRODUCTION N` の `N` をその件数に置き換えて入力します。
承認後に計画を再照合し、本番課題DBへの課題作成だけを実行します。作成後はNotionを読み戻し、同一の取得データで再同期が書き込み0件になることを確認します。

Phase 3Cの合成HTTPモックでの作成・再同期、障害復旧、実書き込みの有効化条件は
[Notion同期 Phase 3C](docs/NOTION-SYNC-PHASE-3C.md)を参照してください。

### macOSの検証用Notionへの試験同期

`notion-trial --target ID` は独立した検証用課題・授業データソースと合成課題1件を使います。
既定はdry-runです。書き込みには `--allow-notion-write` と、表示された計画の対話承認が必要です。
検証環境の準備・共有と試験書き込みには別途承認が必要です。設定例と検証手順は
[Notion同期 Phase 3D](docs/NOTION-SYNC-PHASE-3D.md)を参照してください。

### 授業の対応表と既存課題の移行案

独立したNotion授業対応表を使う場合は、既存の `notion` 設定に次の `course_rules` を追加します。
`course_term` は既存授業DBの「学期」のプロパティIDです。それ以外のプロパティIDは対応表のものを指定し、
「元の授業名」にはNotionのタイトル列を使います。読み取り接続には対応表も共有します。

```json
"course_rules": {
  "data_source_id": "00000000-0000-4000-8000-000000000004",
  "course_term": "course-term",
  "source": "origin",
  "scope": "scope",
  "external_id": "external",
  "term": "term",
  "course": "destination",
  "status": "rule-status"
}
```

| 対応表の列 | 型と入力値 |
| --- | --- |
| 元の授業名 | タイトル。表示用の授業名またはカテゴリ名 |
| 取得元 (`source`) | 単一選択。`HOPPII` / `Moodle` |
| スコープ (`scope`) | テキスト。HOPPIIは `default`、Moodleは取得設定と同じフィードスコープ |
| 外部識別子 (`external_id`) | テキスト。HOPPIIは `group_id`、Moodleは授業関連ICSの単一 `CATEGORIES` を復号した文字列 |
| 学期 (`term`) | テキスト。`年度:WebClassのsemester原値`。合成データでは `2026:2`。既存授業DBの学期も同じ形式 |
| 対応する授業 (`course`) | 既存授業DBへのRelation。確定行では同じ学期のページを1件選択 |
| 設定状態 (`status`) | 単一選択。`未設定` / `確定` / `対象外` |

対応表を設定した場合、`notion-check` と `sync --dry-run --fixture --allow-notion-read` は
ルールをNotionから読み、取得元・スコープ・外部識別子を完全一致で照合します。Moodleの確定行は、
そのフィード内でカテゴリが1つの開講学期を示すことを確認してから設定します。学期をまたいで同じキーを
複数行に登録すると保留になります。設定状態、重複、不正なRelation、確認できない学期は件数で表示します。
`対象外` は同期対象から外し、対応表を最後まで読み取れないときは課題の変更案を作りません。
確定先と異なる非空の課題Relationは、課題の更新全体を保留します。
`sync --dry-run --fixture` では大学とNotionの両方を合成し、この対応表を検証できます。

#### 授業対応の初期設定（合成環境）

授業DBにはタイトルと学期のほか、新規作成時の照合と復旧に使う `HOPPII group_id`（テキスト）を用意します。
既存授業と対応付ける場合は、WebClassの授業IDと学期を確認し、既存ページの `HOPPII group_id` と学期を設定します。
その2項目が一致すれば既存ページを使います。同名で学期が一致または不明の既存ページに `group_id` がない場合は、
対応表に `未設定` を追加するところまで進めます。Notionで既存ページを選び、Relationを1件設定してから `確定` にします。

実Notion側では、既存の授業DBと課題DBに加えて、上表の7項目を持つ独立した授業対応表DBが必要です。
授業DBの初期設定用プロパティIDは `notion.course_rules.setup` に指定します。`year`、`day`、`period` は任意です。
年度の型は数値で、曜日と時限はテキストです。曜日・時限にはWebClassの値をそのまま記録します。

```json
"setup": {
  "group_id": "hoppii-group",
  "year": "year",
  "day": "day",
  "period": "period"
}
```

合成環境では次の順で操作します。1行目で作成・再利用・保留の件数を確認し、2行目で合成Notionへ反映します。
2行目は同じ合成データに再実行し、追加作成が0件になるところまで表示します。指定学期は `年度:semester原値` です。

```sh
cargo run --locked --offline --features synthetic-https-fixture -- sync --setup-courses --fixture --term 2026:2
cargo run --locked --offline --features synthetic-https-fixture -- sync --setup-courses --fixture --term 2026:2 --apply
```

WebClassの新規授業では、授業ページを作成してから、そのページを参照するHOPPIIルールを `確定` で作成します。
Moodleは有効なカテゴリを `未設定` で登録し、Notion上でユーザーが授業と学期を確認して確定します。
書き込み結果が不明な場合は、同じ状態DBを使って再実行すると、外部キーで確認できた既存ページから再開します。
該当ページを確認できない作成意図は保留されます。このコマンドの書き込み先はループバックの合成Notionに限定されています。

#### Macで実データを使った授業初期設定のプレビュー

検証用設定には `notion.course_rules.setup.group_id`、`webclass: true`、`moodle.host`・`moodle.path`・`moodle.scope` を指定します。
読み取り用Notion Integrationと、コース関連のMoodleカレンダーURLは、同じ検証用設定を指定してMacのKeychainに登録します。
未登録の場合は `credentials set notion` または `credentials set moodle` を使います。MoodleのURL全体は非表示の入力欄に入力します。

```sh
cargo run --locked --offline -- --config local/notion-verification.json university-check webclass --courses-only --check-local
cargo run --locked --offline -- --config local/notion-verification.json sync --setup-courses --term 2026:2 --allow-university-read --allow-notion-read
```

2行目で開く一時Chromeプロファイルの大学公式画面で認証し、HOPPIIのWebClassリンクを開きます。
取得範囲はWebClassの授業一覧、MoodleのICS 1件、Notionの設定対象です。実行結果は端末に表示します。
`candidate` に対象授業・カテゴリ、その作成／再利用／保留、対応ルール案、Notionの既存ページ候補が出ます。
`existing_course_without_group_id` は、Moodle由来の検証ページを確認するための一覧です。現在のカテゴリと名前が一致する場合は候補として表示します。
名前は空白と英数字の全半角・大文字小文字の違いを吸収して比較します。名前だけの一致では授業を結びません。
MoodleのICSは開講学期を持たないため、指定学期は候補の値です。カテゴリの学期と実際の対応先はユーザーが確認します。
終了コード `3` は確認待ちの保留を含むプレビューです。検証用DBのプレビューは `plan_id` を表示します。
プレビューでは作成意図を読み取り専用で照合し、Notionページの作成・再利用・保留を表示します。

#### Macで検証用Notionに授業初期設定を適用

書き込み先は検証用授業DBと授業対応表DBです。既存の検証用Writer（Bot ID `41e85934-7e84-81b1-a761-002702fbd8d2`）に、
両データソースの Read content と Insert content を付与します。既存の読み取り用Integrationで3つのデータソースを検証し、
Writerは新規ページの作成だけに使います。

Macの標準 `trial.json` にある検証用Reader・WriterのBot IDを使い、既存コマンドでWriterのトークンをKeychainに登録します。
トークンは非表示入力欄に入力します。

```sh
cargo run --locked --offline -- notion-trial --target 41e85934-7e84-817e-b1d2-000b68726503 credentials set write
```

現行ビルドでプレビューを実行し、`create_courses`、`create_rules`、`reuse_courses`、`held` と各行を確認します。
そのプレビューに表示された64桁の `plan_id` を使います。

```sh
cargo run --locked --offline -- --config local/notion-verification.json sync --setup-courses --term 2026:2 --allow-university-read --allow-notion-read
cargo run --locked --offline -- --config local/notion-verification.json sync --setup-courses --term 2026:2 --allow-university-read --allow-notion-read --apply --allow-notion-write --expect-plan PLAN_ID
```

適用時は大学データとNotionを再取得し、計画が変わっていれば `issue=plan_changed` で停止します。
作成の直前にも対応ルールの外部キーと授業の対応先を照合します。作成結果が不明なときは、同じMacの
`course-setup-targets/<授業対応表データソースID>/course-setup.sqlite3` を使ってプレビューから再開します。
作成済みのページが確認できる場合は再利用し、作成意図が残っていてページを確認できない場合は保留します。
授業ページと対応ルールは別々のAPIリクエストで作成します。適用中に別のクライアントが同じ対象を編集・作成する場合は、
直前照合からAPI書き込みまでの間に競合が起きる可能性があります。

#### 旧設定と既存課題の移行

以下の `course_mappings` は旧経路と移行候補の形式です。

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
