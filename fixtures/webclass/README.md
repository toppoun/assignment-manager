# WebClass fixtures

実 WebClass API レスポンスの代表行を匿名化した、Phase 4 のオフライン parser / mapper テスト用 fixture です。各 JSON のルートは実レスポンスと同じ配列です。実ユーザー情報、実内部 ID、実際の講義名・課題名、実セッション情報は含みません。日時も合成値に置換しています。HTTP / auth テストには使用しません。

- `courses_basic.json`: 異なる曜日・時限を持つ通常講義 3 件。contents fixture の講義 ID と対応する行を含みます。
- `courses_edge_cases.json`: `day_of_week = "0"` / `class_order = "0"`、PHP serialize 形式の `children` による複数時限、`group_name_en` の `null` と空文字、非空の `group_memo`、`level_one = null` を含みます。
- `contents_basic.json`: 実データで確認できた `contents_kind` の通常課題 3 件と `scores` 配列を含みます。
- `contents_states.json`: 未回答、回答済み、0 点で回答済みの 3 件を含みます。`max_score = 0` と `null` は別の状態です。

`day_of_week = "0"` / `class_order = "0"` は時間割情報のない実データ例であり、これだけで OnDemand と断定しません。string / number / null / bool、空文字、`"0"` / `"1"`、日時文字列、ネストした `scores` と `perms`、PHP serialized string は実 API の型と構造に合わせて意図的に保持しています。
