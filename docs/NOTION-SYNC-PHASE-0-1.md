# Notion専用化 Phase 0・1

対象ブランチは `feature/notion-sync`。開始時のHEADは `dffb4a9c74b9787954af95dfa6e51be1f7e1a4de`、作業ツリーはクリーンだった。比較元 `feature/web-migration` のローカル参照も同じコミットを指していた。

## Phase 0の基準とPhase 1の再検証

Windows x86_64 MSVC、Rust/Cargo 1.97.1、Node.js 24.12.0で実行。Cargoのテスト・ビルド・Clippyは `--locked --offline` を指定した。

| 対象 | 変更前のテスト | 変更後のテスト | fmt・Clippy・ビルド |
| --- | --- | --- | --- |
| Rust core | 346テスト + 11 doctest、全機能のlib 346テスト | 350テスト + 11 doctest、全機能のlib 350テスト | 前後とも通過 |
| application | 34テスト + 3 doctest | 34テスト + 3 doctest | 前後とも通過 |
| Tauri | 18テスト | 18テスト | 前後とも通過。ビルドは `custom-protocol` 付き |
| Web server | 既定49テスト、全機能61テスト | 既定49テスト、全機能61テスト | 前後とも通過 |
| macOS bridge（Rust） | 18テスト | 18テスト | 前後とも通過 |
| frontend | Node.js 76テスト | Node.js 76テスト | HTML/CSS/JSを直接配信する構成 |

テストは各manifestで `cargo test`、coreは追加で `--all-features --lib`、Webは追加で `--all-features` を実行した。静的検査は `cargo fmt --check` と `cargo clippy --all-targets --all-features -- -D warnings`、ビルドは `cargo build` を使用した。frontendは `node --test tests/*.test.mjs` を実行した。

- 初回のサンドボックス内実行では、coreのループバックHTTPテストが失敗・待機し中断。Webの一時DB準備が `Internal`、Tauriの一時設定ファイル操作が `SettingsWriteFailed` で失敗した。同一コードを通常権限で再実行すると通過した。Phase 1後も該当するスイートは通常権限で実施した。検証先はローカルfixtureと一時ファイル。
- Webの既定機能のテストでは変更前後とも `tests/sync_http.rs` の既存の未使用import警告が出た。Windowsの一部のコンパイルでは増分キャッシュのハードリンク警告が出て、コピーにフォールバックした。全機能のClippyは通過した。
- Webで明示的にignoreされているWindows資格情報ストアの実ストアsmokeは各スイートで1件未実行。macOSのSwift/XcodeビルドとネイティブテストはWindows環境のため未実施。Rust bridgeのWindows上の検証は表のとおり。

## 取得と取り込みの不変条件

- Moodleの課題キーは `(Moodle, feed scope, UID)`。WebClassは `(WebClass, group_id, contents_id)`。同名や同一期限を理由に統合しない。
- Moodleは期限相当のイベントだけを候補化し、明示的なCourseRelatedエクスポートにある有効な単一カテゴリを授業の外部キーとして使う。WebClassは明示的に表示可能な `Question` を候補化し、グループIDで授業に対応づける。課題名、期限の時刻変換、提出URLの `None` は既存のマッパーに従う。
- 取得した全レコードの外部キーを候補とは別に検証する。重複、空ID、異なるグループ、観測に存在しない候補を拒否する。観測があっても候補は空になり得る。対象の欠落を削除・状態変更の指示として扱わない。
- WebClassは授業一覧の検証後に一覧の観測を記録し、応答順で授業のバインディング、当該授業の取得、取り込みを行う。最初の失敗で止まり、先にコミットした内容と失敗した取得の直前に作成済みの授業バインディングを保持する。認証切れ時はログインと全体の再試行を一度だけ行う。
- SQLiteは各課題バッチとその正の観測・スコープ時刻を同じトランザクションで保存する。Moodleの授業作成は従来どおり別コミット。HTTP待ちを課題取り込みのトランザクションに含めない。既存課題のIDとローカル完了状態を維持し、既存の授業名・時間割を保持する。
- 大学のHTTP・SSO・Cookie分離・通信ポリシー・既存の資源チェックを経由する。取得結果には正規化に必要な課題・授業データだけを格納し、認証情報、HTTP応答全体、セッションを含めない。結果の `Debug` とエラーは固定分類や件数で確認できる。

## Phase 1の設計

[`src/sync/acquisition.rs`](../src/sync/acquisition.rs) の `acquire_moodle`、`acquire_webclass_group`、`acquire_webclass_once`、`acquire_webclass_with_reauth` を `assignment_manager::sync` から利用できる。認証済みWebClassセッションまたはMoodleクライアントを借り、SQLiteを開かずに検証済み候補、正の観測、観測時刻、授業の外部識別情報を取得できる。WebClass全体の結果では、成功済みグループと最終結果を保持する。再認証前後の試行を時系列で返すため、同じキーが別の試行に現れ得る。

[`src/sync.rs`](../src/sync.rs) の既存SQLite入口は同じ取得・検証と再認証処理を使用する。Moodleの授業リンクとWebClassの授業・課題バッチを既存順序で受け取ることで、書き込みや検証の失敗で停止する位置を維持する。WebClassの授業スコープ定数は [`src/import.rs`](../src/import.rs) に移し、[`src/persistence.rs`](../src/persistence.rs) からも従来の名前で参照できる。

[`src/secured_sync_tests.rs`](../src/secured_sync_tests.rs) に4件を追加した。SQLiteを開く前に両取得元の取得を完了し、同じ合成応答を従来経路で取り込んだ結果と比較する。取得データの正規化、グループをまたぐ外部キー、除外対象を含む重複・グループ不一致・期限不正、空の候補、途中失敗、再認証の成功・二度目の失効、秘密を含まない診断出力を検証する。

実大学の現行仕様、実データ上のID継続性、提出URLの仕様は未検証。今回の疎通確認は固定ループバックの合成HTTPSで実施した。
