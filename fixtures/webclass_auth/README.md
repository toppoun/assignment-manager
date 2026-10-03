# WebClass authentication fixtures

Phase 5B の将来のオフライン response parser 用の匿名 fixture です。`openam_probe.py` が使用する JSON / HTML の構造と、実験で確認済みの callback type・stage・form field・JS redirect パターンを基準にしています。実験コードの実行、通信、認証処理は行いません。

PoC はレスポンス本文の完全な標本をコード内に保存していません。そのため、これらは実レスポンス全文のコピーではなく、確認済みの構造を表す最小の合成例です。表示文言、識別子、値、任意のメタデータを実環境の固定仕様として扱わないでください。`input` / `output` は OpenAM の `name` / `value` オブジェクトの配列として表し、文字列・数値・配列の区別を保持しています。

## 各 fixture

### `openam_initial_callbacks.json`

- **response**: OpenAM の初期 callback 応答。確認済みの `NameCallback`、`PasswordCallback`、`ConfirmationCallback`、`TextOutputCallback` を含みます。PoC は `callbacks` を走査し、`type` によって名前入力・password 入力・確認操作を識別しています。
- **parser が見る予定の field**: `callbacks[*].type`、`input[*].name` / `value`、`output[*].name` / `value`。`authId` は不透明な状態識別子、`stage` は段階のメタデータとして扱います。
- **synthetic 化**: `authId`、初期 `stage`、callback の入力識別子 `IDToken101` / `IDToken201` / `IDToken301`、表示文言、選択肢。username と password の初期入力は合成した空文字で、実資格情報はありません。
- **保証しないもの**: callback array index は仕様としません。名前・password・確認 callback を配列先頭から順に置かず、`TextOutputCallback`、`ConfirmationCallback`、`PasswordCallback`、`NameCallback` の順にしています。この順序も固定仕様ではありません。`IDToken` の番号と配列位置の対応、初期 stage 名、選択肢番号、表示文言、callback の常時同一構成、レスポンスの全 field は保証しません。

### `openam_scripted2.json`

- **response**: 実験で確認済みの `stage = "Scripted2"` と `HiddenValueCallback` / `TextOutputCallback` を表します。PoC はこの stage を確認した後、受け取った state をそのまま次の認証段階へ渡しています。
- **parser が見る予定の field**: `stage`、`callbacks[*].type`、各 `input` / `output` の `name` / `value`、不透明な `authId`。
- **synthetic 化**: `authId`、`IDToken401`、hidden field の `id`・出力値・入力値、表示メッセージ。hidden 値は解析対象ではなく不透明な文字列です。
- **保証しないもの**: callback 順序、識別子、hidden 値の形式や意味、`messageType` の実環境の値、script の実行・生成・検証、stage 遷移の網羅性、再送による認証成功。合成した `TextOutputCallback` のメッセージは実際の script 内容を再現しません。

### `openam_success.json`

- **response**: OpenAM 認証成功時に PoC が取り出す `tokenId` と `successUrl` を含む応答です。
- **parser が見る予定の field**: `tokenId`、`successUrl`。token は不透明な文字列、URL は応答元の origin と解決可能な相対 path として扱います。
- **synthetic 化**: token、IdP path の識別部分、query の request ID。`successUrl` は localhost の絶対 URL へ置換せず、`/openam/...` の相対 path と query の形を保持した合成例です。
- **保証しないもの**: token の形式・有効性・期限、実 IdP の具体的 path / query、URL の接続可能性、実際の認証成功。他の成功レスポンス field の有無も保証しません。fixture テストで `tokenId` / `successUrl` をログへ出力する用途には使わず、存在・型・期待値をオフラインで確認してください。

### `saml_auto_submit.html`

- **response**: SAML 応答を POST する auto-submit HTML form の最小例です。PoC は form の action と名前付き input を取り出し、`SAMLResponse` の存在を確認しています。実験で確認済みの `RelayState` も含みます。
- **parser が見る予定の field**: form の `action`、named inputs の `name` / `value`（`SAMLResponse`、`RelayState`）。auto-submit の実行は必要ありません。
- **synthetic 化**: action の host / path / query、form ID、`SAMLResponse`、`RelayState`。`.invalid` の合成 host を使い、SAMLResponse は assertion ではない完全な偽文字列です。署名やユーザー属性はありません。
- **保証しないもの**: SAMLResponse を生成・検証しません。Base64 / XML の妥当性、署名、ユーザー属性、ACS の実 URL、RelayState の意味、ブラウザーでの送信、複数 form の選択規則は保証しません。named inputs の取り出しだけを対象にします。

### `hoppii_csso_form.html`

- **response**: 実験で確認済みの HOPPII CSSO form。`uid`、`password`、`timestamp`、`hash`、`AC_LOGIN`、`btnLogin` の 6 field を含み、action は確認済みの `/webclass/singlesignon.php` です。
- **parser が見る予定の field**: form の `action` と 6 つの named inputs の `name` / `value`。password の空文字も値として保持します。
- **synthetic 化**: `uid`、`timestamp`、`hash`、`AC_LOGIN`、`btnLogin` の値。`1900000000` は手で決めた合成値で、実験の timestamp ではありません。password は実験で確認した空文字の挙動を表します。
- **保証しないもの**: hash を解析・生成しません。hash のアルゴリズム・長さ・妥当性、timestamp の単位・期限・時刻判定、uid の実形式、`AC_LOGIN` / `btnLogin` の実値、input の type / 順序、送信時の submit button のブラウザー挙動、CSSO の成功は保証しません。

### `webclass_js_redirect.html`

- **response**: 実験で確認済みの `window.top.location.href="/webclass/login.php";` を含む最小 HTML です。
- **parser が見る予定の field**: `window.top.location.href` に直接代入された文字列の移動先 path。
- **synthetic 化**: 周囲の HTML と title。確認済みの代入式・path は保持し、認証情報や query は含めていません。
- **保証しないもの**: JavaScript 全般には対応しません。式評価、変数参照、DOM 操作、ブラウザー実行、別の代入構文、HTTP redirect は対象外です。

### `webclass_js_redirect_escaped.html`

- **response**: 実験で確認済みの `window.top.location.href="\/webclass\/";` を含む最小 HTML です。PoC は取り出した文字列の `\/` を `/` に戻しています。
- **parser が見る予定の field**: 同じ直接代入の文字列。escaped slash を戻した移動先は `/webclass/` です。
- **synthetic 化**: 周囲の HTML と title。確認済みの escaped slash の形と path は保持し、認証情報や query は含めていません。
- **保証しないもの**: JavaScript 全般には対応しません。任意の escape sequence、式の連結・評価、汎用 JavaScript parser、ブラウザー実行は対象外です。

## 共通の制限

実大学 ID、実 password、実 token、実 SAMLResponse / RelayState、実 uid / timestamp / hash、session cookie の値・header、HAR 由来の個人情報は含めていません。query 値と状態・callback・request の識別子も合成しています。SAMLResponse と hash は不透明な入力値として扱い、解析・生成・検証の素材にはしません。

このディレクトリは response 解析用の静的データだけを提供します。HTTP request、OpenAM login、SAML POST、HOPPII request、WebClass SSO、cookie jar、scraper、Authenticator、retry、reauthentication の動作や接続先の安全性を保証するものではありません。Rust 実装・Cargo 設定・regex の追加は本フェーズの対象外です。
