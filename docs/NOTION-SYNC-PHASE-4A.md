# Notion同期 Phase 4A

`feature/notion-sync` の大学取得をWindows 11で調査し、ローカルfixtureとHTTPモックで検証した。実大学・実Notionへの通信、既存資格情報と状態DBの操作は実施していない。

## 取得と正規化

| 項目 | Moodle | HOPPII / WebClass |
| --- | --- | --- |
| 取得入口 | `acquire_moodle` → `MoodleClient::fetch_calendar`。CourseRelatedのiCalendarをHTTPS GETで1回取得する。 | `acquire_webclass_once` → `WebClassClient::fetch_courses_json` と、授業ごとの `fetch_contents_json`。認証済みセッションで `/webclass/ip_mods.php/plugin/score_summary_table/courses` と同じ配下の `contents?group_id=…` をGETする。 |
| 認証・失効 | エクスポートURLのqueryが資格情報。ログイン処理やCookieを使わない。リダイレクトや認証系HTTPエラーは取得失敗、HTTP 200のログインHTMLはICS解析失敗になる。登録済みURLはWindows Credential ManagerまたはmacOS Keychainから読み取る。 | 大学ID・パスワードで既存の `HoseiAuthenticator::login` を実行し、Cookieを持つHTTPクライアントをメモリに保持する。保護付きトランスポートはoriginごとにCookieを分離する。既知のログイン画面や同一originのログイン先へのリダイレクトは `AuthExpired`。既存の再認証付き取得は失効時にログインと全体再試行を各1回行う。今回の合成CLIは1回の取得を検査する。 |
| 課題・授業 | 期限を示すVEVENTの `SUMMARY` を正規化し、明示的なCourseRelatedで有効な単一 `CATEGORIES` を授業のラベルとキーに使う。識別子は `(Moodle, feed scope, UID)`。 | 表示可能な `Question` の `contents_name` と、授業一覧の `group_name` を使う。課題キーは `(WebClass, group_id, contents_id)`、授業キーは `(WebClass, default, group_id)`。同じcontents IDが別授業にあっても別課題になる。 |
| 期限・終了済み | 対象イベントの `DTSTART` のUTC表記をそのまま使う。終了イベントや過去の期限は除外しない。期限なし課題の発見、DATE・TZID・浮動時刻は未対応。 | `end_date` はJST固定で解釈しUTCに変換する。空欄・null・欠落は期限なし。過去の期限も取得対象で、得点・提出済み時刻から完了状態を推定する処理はない。 |
| 詳細・提出URL | ICSに含まれる範囲を扱う。課題詳細ページは取得しない。提出URLは `None`。 | 授業ごとのcontents配列に含まれる範囲を扱う。課題詳細ページは取得しない。`link_to` の意味と実際の提出先が未確認のため、提出URLは `None`。 |

[取得の共通入口](../src/sync/acquisition.rs) · [Moodle HTTP](../src/moodle_http.rs) · [WebClass HTTP](../src/webclass_http.rs) · [WebClass認証](../src/webclass_auth.rs)

正しい空のICSやJSON配列は0件で成功する。途中で授業のcontents取得に失敗した場合、WebClassの取得結果には検証済みグループと失敗分類が残る。空・重複ID、不正な期限、要求と異なるグループ、壊れた応答は検証で失敗する。今回、HTTP 206、`Content-Range`、`Link` の付いた成功応答も拒否し、WebClassのページング形式のオブジェクトもテストで拒否を確認した。`Link` の用途が実環境で確認できるまで、ヘッダーの存在を保守的に拒否する。正しい形式の応答にサーバーが無通知でかける件数上限、課題詳細ページの欠落、権限やエクスポート期間による漏れは検出できていない。

## CLIとNotionの依存関係

```sh
cargo run --locked --offline --features synthetic-https-fixture -- university-check --fixture
```

合成HTTPSの相手は固定ループバックだけ。WebClassの認証POSTも合成相手に限定される。成功時は `moodle acquired=1 failures=0`、`webclass acquired=9 failures=0`、`result=success`。失敗件数は取得元ごとの最初の失敗を数え、未処理の件数は推定しない。途中失敗時の取得件数は検証済みバッチの件数。大学専用CLIは設定・資格情報の不足を終了コード2、取得失敗を1、成功を0で返す。出力に課題名・課題ID・URL・本文・認証情報を含めず、スナップショットも保存しない。

- 既存の `doctor` と `status` はローカル診断。`sync --dry-run` の通常経路は大学通信無効。`sync --dry-run --fixture` は合成大学とNotionモックを処理し、ローカルの合成用レシートを保存する。`notion-check --allow-notion-read` は実Notionの読み取り用。`sync --dry-run --fixture --allow-notion-read` は大学を合成に限定し、Notion読み取りを明示する既存経路。Phase 3Dの `notion-trial` はmacOSの合成課題を使う別経路。
- Phase 1の取得APIはSQLiteから独立している。Phase 2以降の `SyncInput::from_acquisitions` は課題の外部キーをUTF-8の固定バイト列から `v1:` の同期キーに変換する。期限は取得元と同じ値を使用し、授業Relationは設定済みの外部授業キーとNotion授業ページを照合する。提出URLは現契約で `None` であり、未検証の値は同期入力の検証で拒否する。
- Phase 0〜3で残った実取得側の未確認事項は、対象環境でのID継続性、現行SSO、授業・課題の収録範囲、ページングと詳細ページ、提出URLの仕様。今回接続した実取得経路はMoodle単体の読み取り確認。通常の大学取得から実Notion同期へつなぐCLIと、GET専用の実WebClass入口は未実装。

## 別途承認後の読み取り試験

### Moodle

1. 利用者の対象Moodleの正確なホスト名とエクスポートパス、HTTPS 443、CourseRelated形式、同じフィードを指す既存の安定した `scope` を確認する。サンプルのホストやパスは承認値に使えない。
2. 既存のCLI設定の `moodle.host`、`moodle.path`、`moodle.scope` を確認する。既定の設定はWindowsで `%LOCALAPPDATA%\AssignmentManagerNotion\config.json`、macOSで `~/Library/Application Support/AssignmentManagerNotion/config.json`。代替設定はリポジトリ外に置き、`--config PATH` で指定できる。設定上のNotion課題データソースIDは既存資格情報の名前空間を選ぶために使用する。登録済みMoodle項目がなければ、別途承認の後に既存の非表示入力による登録手順を使う。
3. 実通信の承認後に以下を手動で実行する。承認済みホストとパスにGETを1回送り、プロキシやリダイレクトを使わない。Notionクライアント、状態DB、実行レシートを開かない。ログは件数と固定分類に限定する。

```sh
cargo run --locked --offline -- university-check moodle --allow-university-read
```

実機では大学画面の対象期間・授業・課題と突き合わせ、同じエクスポートの再取得でキーが一致することを確認する。件数だけでは同一IDを確認できないため、照合は承認済みのメモリ内比較や匿名化した集計で行う。URLのqueryと応答原文は保存・共有しない。

### HOPPII / WebClass

現コードが想定するHTTPS 443の通信先は、`hoppii2025.hosei.ac.jp`（ポータル・SAML/CSSOの開始）、`idp.hosei.ac.jp`（SSO・OpenAMと必要時の同意）、`lms2025.hosei.ac.jp`（CSSO・課題GET）。現在もこれらが利用者の正式な遷移先であるかは今回未確認。

現行の認証はOpenAM、SAML、CSSOにPOSTし、条件によっては同意を送信する。既存の `acquire_webclass_once` は認証済みクライアントからGETだけを行うが、承認済みセッションをCLIへ渡す入口はない。GET限定の試験には、大学が許容するセッション受け渡し方式の確認と、実装前の報告・承認が必要。認証POSTを許可する方式を選ぶ場合は、通信先・SSOの操作・同意・再認証の範囲を先に別途承認する。どちらの方式でも、現行originと大学利用条件を確認し、最初の読み取りは授業一覧と授業別contentsを対象とする。

[既存の大学通信方針](WEB-NETWORK-SECURITY.md) · [現行ホストの確認状況](PHASE-WEB-4B3-PREFLIGHT.md) · [既存の実通信計画](WEB-REAL-SYNC-SMOKE-PLAN.md)

## 両OSの検証とPhase 4Bへの条件

- 同じ合成Moodle・WebClass課題に対して、両OSで使う固定の期待同期キーをテストに追加した。別々の取得と別々の空の状態DBで同じNotionモックを参照すると、2台目も既存10ページに同じIDで紐づく。PCごとのロックはローカルのため、実Notionに書き込む段階では単一の実行主体を維持し、送信結果不明の作成意図を別DBに切り替える前に照合する。
- Windowsでパスに空白・日本語を含む一時DB、ファイルロックの多重起動とプロセス終了時の解除、LF/CRLFのICS、UTF-8の日本語、期限のUTC/JST変換を検証対象とした。macOSの一時ディレクトリは既存のテストヘルパーで正規化してから使う。Credential ManagerとKeychainの既存方針を維持し、取得・正規化・同期キーのOS分岐を追加していない。
- macOSネイティブで同じテストを通し、状態DBのパス・ロックとKeychainを確認する。GitHub Actionsを使う場合はWindows/macOSの2ジョブで、`cargo test --locked --all-features`、`cargo fmt --check`、`cargo clippy --locked --all-targets --all-features -- -D warnings` を実行する。ジョブに大学・Notionの資格情報を渡さない。通常CLIの試験を同じジョブで再現する場合は `cargo test --locked` も実行する。
- Phase 4Bへ進む条件は、明示的な実通信承認、Moodleの正確な取得先と対象期間・形式の確認、WebClassの現行SSOと読み取り試験に必要な認証経路の事前承認、両取得元での再取得時の同一ID・期限・授業・件数の突き合わせ、欠落や取得失敗時の挙動確認。詳細・提出URLが要件なら、実データで仕様を確認したうえでURL受け入れ方針を決める。実Notionへの接続・書き込みは別途承認とする。

## Windows 11での結果

| 検証 | 結果 |
| --- | --- |
| Rustルートの通常構成 | 426テストと11 doctestが通過 |
| Rustルートの全機能 | 432テストと11 doctestが通過。合成HTTPSの実行制限があったため、オフラインのまま通常権限で再実行した |
| Rustの関連crate | application 34テストと3 doctest、Tauri 18テスト × 2ターゲット、Web server 61テスト、macOS bridge 18テストが通過。Web serverの実資格情報ストア試験は既定でignoreされた1件 |
| 静的検査 | `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`git diff --check` が通過 |
| macOS | Windowsからの `aarch64-apple-darwin` クロスチェックは依存をCargoキャッシュに展開する段階でアクセス拒否。macOSネイティブのビルド・テストとKeychainは未検証 |
