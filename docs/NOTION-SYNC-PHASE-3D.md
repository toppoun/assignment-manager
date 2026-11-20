# Notion同期 Phase 3D

macOSの `notion-trial` で、ローカル設定の合成課題1件を検証用Notionに同期する。課題名・期限・授業ページを変更しても同期キーは同じ値を使う。取得元には試験用の `Moodle` を設定する。入力は設定ファイルから読み、大学への通信は発生しない。

## 承認と検証環境

1. 検証用DBの作成・共有・権限設定について承認を得る。その後、空の課題DBと授業DBを別々に用意する。両者のデータソースIDを取得し、授業DBに合成授業ページを1〜2件用意する。
2. 検証専用のNotion内部接続を、読み取り用と書き込み用に1つずつ用意する。両接続の共有先は検証用の2つのDBに限定する。親ページ経由の共有範囲も確認する。読み取り側はRead content、書き込み側はRead/Insert/Update contentを付与する。各接続のbot IDは `GET /v1/users/me` の `id` を使う。資格情報をコマンドライン引数やログに残さない。 [接続の権限](https://developers.notion.com/reference/capabilities) · [接続のbot ID](https://developers.notion.com/reference/get-self)
3. 次の構成とdry-runの結果を確認し、実Notionでの試験書き込みについて別途承認を得る。検証期間中は書き込み用CLIを一つのmacOS環境から実行し、競合試験以外では課題の編集や移動を同期実行と重ねない。

| 検証用の課題プロパティ | APIの型と設定 |
| --- | --- |
| 課題名 | `title` |
| 期限 | `date`、単一の日付または日時 |
| 授業 | `relation`、検証用授業データソースを参照 |
| 同期キー | `rich_text` |
| 取得元 | `select`、`Moodle` と `WebClass` の選択肢 |
| 提出URL | `url` |
| 状態 | `status` |
| 優先度 | `select` |
| メモ | `rich_text` |
| 繰り返し | `select` |

## macOSの設定と実行

`~/Library/Application Support/AssignmentManagerNotion/trial.json` に次の形式で設定する。IDは架空の値なので、検証専用のIDで置き換える。`--config PATH` で検証用設定のファイルを指定できる。期限には `{"DateTime":"2026-12-15T12:00:00Z"}` または `null` も使える。

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
  "read_bot_id": "00000000-0000-4000-8000-000000000011",
  "write_bot_id": "00000000-0000-4000-8000-000000000012",
  "assignment": {
    "title": "Phase 3D 合成課題",
    "deadline": {"DateOnly": "2026-12-15"},
    "course_page_id": "00000000-0000-4000-8000-000000000003"
  }
}
```

初回は `course_page_id` を検証用授業ページのIDにするか、空欄補完から試す場合は `null` にする。次のコマンドの `trial_target` に、検証用課題データソースIDを設定する。通常の `config.json` と通常の読み取り用Keychain項目は別経路で使う。

```sh
cargo build --locked --offline
trial_target='00000000-0000-4000-8000-000000000001'
target/debug/assignment-manager notion-trial --target "$trial_target" init-state
target/debug/assignment-manager notion-trial --target "$trial_target" credentials set read
target/debug/assignment-manager notion-trial --target "$trial_target"
```

書き込み許可が得られた後、対話端末で書き込み用トークンを登録して実行する。表示された計画を確認し、承認する場合は `APPLY` と入力する。入力を拒否した場合や承認後に計画が変わった場合は保留する。

```sh
target/debug/assignment-manager notion-trial --target "$trial_target" credentials set write
target/debug/assignment-manager notion-trial --target "$trial_target" --allow-notion-write
target/debug/assignment-manager notion-trial --target "$trial_target" status
```

トークンは役割とbot IDごとに異なる検証専用Keychain項目に登録する。接続時に `GET /v1/users/me` で設定のbot IDと照合し、資格情報の不一致はデータソース取得前に停止する。書き込みモード自身も書き込み用接続でスキーマ・Relation先・課題の全件取得を検証する。計画の提示から実行まで同じファイルロックを保持し、承認後にスキーマと課題全件から計画を組み直す。課題・授業の本番データソースIDは、どちらの役割でも固定の拒否リストで遮断する。非対話実行と対象選択の不一致はネットワーク送信前に停止する。

状態DBは `trial-targets/<課題データソースID>/state.sqlite3` に置く。dry-runでは読み取り専用で開く。保存済みの作成意図は、照合まで同じ状態DBとロックを維持する。PCの切り替えやDBの復元時も [Phase 3Cの復旧条件](NOTION-SYNC-PHASE-3C.md#障害からの復旧) に従う。

## 実Notionの試験順序

| 順序 | 操作と合格条件 |
| --- | --- |
| 1 | 空の検証用課題DBでdry-runの `create=1` を確認し、承認付きで合成課題1件を作成する。同期キー・取得元・期限をUIで確認する。 |
| 2 | 同じ入力で再同期し、同じページが `unchanged=1` になりページ数が増えないことを確認する。 |
| 3 | 設定の `assignment.title` と `assignment.deadline` を変え、同じページの2項目が更新されることを確認する。 |
| 4 | Notion UIで状態・優先度・メモを変更する。 |
| 5 | 差分のあるPATCHでも手動値が保持されるよう、設定の課題名をもう一度変えて同期し、課題名と3つの手動値をUIで確認する。 |
| 6 | 初回のRelation設定を確認する。設定とは別の検証用授業にUIで変更し、再同期で設定済みRelationが保持されることを確認する。UIで空欄にしてから再同期し、設定した授業ページが補完されることを確認する。 |
| 7 | HTTPモックで作成受付後にレスポンスを切断する。状態DBに意図が残り、同じキーが見つかるまで再送を抑止し、次回照合で同じページに紐づくことを確認する。POST成功後のSQLite保存失敗もモックで再現する。 |
| 8 | HTTPモックで最終PATCH前のGET応答に競合を入れる。PATCHが送信されず `deferred` になることを確認する。承認待ちの間に手動課題が追加された場合も再計画で保留する。 |
| 9 | `state.sqlite3` とロックディレクトリを維持してCLIを再起動する。通常実行では重複作成がないことを確認し、モックではPOST受付後にプロセスを停止したケースを、同じ状態DBで再照合する。 |

障害の注入はループバックHTTPモック、合成トークン、一時SQLiteで行う。検証CLIの実API送信先は公式APIに固定し、プロキシとリダイレクトを無効にしている。Notionには同期キーの一意制約や、このPATCHで使える条件付き更新の公開仕様がない。直前の読み取りと送信の間に外部から書き込まれる競合は残る。共有範囲の限定と単一の書き込み主体で運用する。 [データソースのクエリ](https://developers.notion.com/reference/query-a-data-source) · [ページ更新](https://developers.notion.com/reference/patch-page)

## 手動課題の候補と承認付き紐づけ案

作成対象とタイトル・期限が一致し、同期キーが空の手動課題は作成保留にする。dry-runでは `manual_candidates` に件数を表示し、承認済みの実行では該当する同期キーとページIDのペアをSQLiteの `manual_candidates` に記録する。候補履歴の確認時には現時点のページを読み直す。タイトル・期限が同じ手動課題を複数見つけた場合はペアごとに記録する。

将来の紐づけコマンドでは、オペレーターが実際の取得元の課題と同一だと確認し、同期キーとページIDを明示する。書き込み先・接続・スキーマ・ロックの制御を同期と共有する。全件取得と直前の取得でページの親・状態・空の同期キー・更新時刻を再確認し、同じキーを持つ別ページや未解決の作成意図、別キーからの紐づけがあれば保留する。計画を表示して対話承認を得たら、そのページIDを含む紐づけ意図をSQLiteに永続化し、同期キーだけをPATCHする。結果不明時は同じページとキーを再読し、合致を検証してから `linked` にする。紐づけコマンドはこのフェーズでは設計までとする。
