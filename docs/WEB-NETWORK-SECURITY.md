# University HTTP transport security (Web-4B1 / Web-4B2)

2026-10-22 / macOS。実大学への接続・実credential利用は行わない。
Web-4B2でsecured backendを明示注入できるようにした。通常Web binaryはDisabledのまま。
本書の「Web」は明示的な `MoodleClient::for_web` / `HoseiAuthenticator::for_web` の保証を指す。
desktop constructorへserver用SSRF保証を付与したという意味ではない。

## Architecture and boundary

```text
SecuredCoreSyncBackend (explicit Rust injection only; ordinary Web binary disabled)
  -> MoodleClient::for_web(NetworkPolicy)
  -> HoseiAuthenticator::for_web(NetworkPolicy)
       -> AuthSession -> same Transport -> WebClassClient
  -> private Transport::send
       -> built URL policy validation -> reqwest execute
       -> GuardedResolver -> bounded system DNS lookup
           -> validate the entire A/AAAA result -> return ONLY those SocketAddrs
       -> hostname/SNI verified TLS -> bounded text read -> existing parsers

normal Web binary -> DisabledNetworkBackend
synthetic-sync test/example -> MockSyncBackend (no HTTP clients)
```

`Transport`のreqwest client、policy、request buildersはcrate外へ公開しない。
WebClassClientへのraw client constructorは `cfg(test)` のみ。認証後・再利用中のclientも
同じsecured transportとorigin分離Cookie storeを保持する。WebClass認証stepのsendを
全てこのtransportへ変更した。CoreSyncBackendにHTTPをコピーしていない。

`NetworkPolicy`はtrusted operator側のimmutable valueで、Deserialize、CLI/env設定、
任意resolver/HTTP client注入のproduction APIを持たない。constructorはDNSやHTTPを実行しない。
`for_web`を選択したobjectがdesktop clientへfallbackする経路はない。
既存 `new` / `with_timeout` / `ApplicationHandle::open` のdesktop経路は維持する。

## Outbound inventory

| Entry / method | Destination contract | Response consumed / bound |
| --- | --- | --- |
| MoodleClient::fetch_calendar GET | operator確認済みcalendar exportのexact host/pathのみ | ICS 8 MiB |
| WebClassClient::fetch_courses_json GET | LMS /webclass/ip_mods.php/plugin/score_summary_table/courses | JSON 8 MiB、login HTML 1 MiB |
| WebClassClient::fetch_contents_json GET | 同 /contents、group_idはquery encoding | JSON 8 MiB、login HTML 1 MiB |
| discover_openam GET #1 | HOPPII /portal/u001/index.php → IdP originのLocationを検証 | redirect bodyは読まずdrop |
| discover_openam GET #2 | IdPのSSO URL → 同IdP /openam/XUI/をparse | redirect bodyは読まずdrop。XUI fragmentは認証paramsのparseだけに使用 |
| authenticate POST ×3 | IdP /openam/json/authenticate | initial / credentials / Scripted2 JSON、各256 KiB |
| saml_to_hoppii GET | IdPの検証済みsuccess URL / consent後return URL | SAML form HTML 1 MiB |
| consent POST getAttributes | IdP /openam/json/saml2/consent/{encoded consentID} | JSON 1 MiB |
| consent POST agree | 同IdP、_action=agree | statusのみ確認、bodyは読まずdrop |
| SAML ACS POST | form actionはHOPPII exact originのみ。SAMLResponse / RelayStateをform encode | 301/302/303のみ、その後GET。bodyは読まずdrop |
| follow_hoppii_get_redirects GET | ACS後、portal login後、user-page確認後、それぞれHOPPII same origin | 各chain最大10。最終status/pathのみ確認、bodyは読まずdrop |
| establish_hoppii_session GET ×2 | HOPPII /portal/login/login.php?next=、/portal/u001/index.php | 上記same-origin chain、最終user-page path必須 |
| webclass_sso GET | HOPPII /portal/sso/Parm.php?auth_mode=1&group_id= | CSSO form HTML 1 MiB |
| webclass_sso POST | CSSO actionはLMS exact originのみ、固定Origin header | redirect bodyはdrop、200 HTMLは1 MiB |
| follow_webclass_redirects GET | LMS same origin、HTTP redirectまたは限定JS文字列parserのみ | 各chain最大10、200 HTML 1 MiB |
| login後API verification GET | 既存fetch_courses_jsonを再利用、JSON array必須 | 上記8 MiB |

未消費bodyは `bytes/text/json/read_to_end` で回収・drainせずResponseをdropする。
これらのbodyをparserへ渡さないため、未消費body全体をbufferする経路はない。
status/redirect段階で拒否するresponseも同じ。消費する全bodyは1箇所の `read_text` を通る。
group取得数・全sync時間は単独constructorのper-request保証外。Web-4B2のfactoryが別途run予算を付与する。

## URL and origin policy

Moodle policyのdefaultは**未設定**。資料・fixtureから実host/pathを推測していない。
`with_moodle_calendar(exact_host, exact_path)` はoperatorが別途確認した値の注入口であり、
普通のWeb binaryには接続されていない。synthetic.invalidのtest値はproduction承認値ではない。

URL parserを用い、HTTPS、443（省略または明示443）、ASCIIの正確なDNS hostname、
userinfoなし（空の `@` も拒否）、fragmentなし、Moodle exact pathを要求する。
IP literal、localhost、trailing dot、suffix偽装、percent-escaped authority、非正規authority、
backslash、制御文字、pathのdot segment正規化・encoded path偽装を拒否する。
MoodleFeedは元の入力syntaxの適否だけをbooleanで記録し、URL parserで偽装が消えた後も
Web fetch時に拒否できる。元secret文字列のコピーやdebug fieldは増やさない。
queryはbearer secretとして保持し、host許可・CourseRelated判定・scope生成には使わない。
`default.course-related` / `MoodleExportKind::CourseRelated` とtoken rotation時のidentityを維持する。

既存ソースで確認した3 origins（現時点の大学稼働確認ではない）:

| Role | Exact origin |
| --- | --- |
| HOPPII SP / portal | https://hoppii2025.hosei.ac.jp |
| IdP / OpenAM | https://idp.hosei.ac.jp |
| WebClass LMS | https://lms2025.hosei.ac.jp |

この定数をpolicyとAuthEndpoints/WebClassClientで共有する。年度変更・SSO変更は明示的な
コード/policy reviewとtest更新が必要。suffix / wildcard / response由来のorigin追加はない。
WebClassの動的SSO pathは既存protocol parserに任せ、network側は3 exact originsを制限する。
各stepはさらに既存 `resolve_destination` で期待roleのoriginを検証する。
同関数にも元authorityの空userinfo/escape/非canonical host検査を追加し、正規化による見落としを防ぐ。
IdP→HOPPII SAML POST、HOPPII→LMS CSSO POST以外の任意cross-origin replayはない。
XUIのfragmentはparseに必要だがfetchしない。実送信URLのfragmentはtransportで拒否する。

## DNS / SSRF / TOCTOU

reqwest 0.13.5の `dns_resolver` / `Resolve` にGuardedResolverを注入する。
独立した事前DNS検査と通常resolverの二重lookupにはしない。1 lookupで得た集合全体を検査し、
1件でも不許可なら全体を拒否する。空/失敗/5秒超過/65件以上の応答もfail closed。
productionではSocketAddrのportを0へ固定し、DNSからのport overrideも許可しない。
IPv4/IPv6のfallback先はこの検査済みiteratorだけ。再lookupが起きた場合も同じguardを通る。
既存poolのconnectionは検査済みIPに接続したsocketを再利用するだけで、DNS変更で宛先が変わらない。

IPv4は0/8、RFC1918、100.64/10、127/8、169.254/16、protocol/documentation/benchmark/
deprecated relay、multicast、240/4、broadcast、およびAzure platform addressを拒否する。
IPv6は2000::/3のglobal unicastだけを候補とし、2001::/23、2001:db8::/32、2002::/16、
AS112、retired 6bone、3fff::/20も拒否する。mapped/compatible IPv6、NAT64 well-known prefix、
ULA、link/site-local、multicast、metadata等は候補外。IANA special-purpose集合は保守対象であり、
未知の将来ネットワーク割当への自動追従はしない。

参照した一次資料: [reqwest resolver API](https://docs.rs/reqwest/0.13.5/reqwest/dns/trait.Resolve.html)、
[IANA IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry/)、
[IANA IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry/)。

これはapplicationのDNS-to-connect制御であり、host管理者のroute/NAT/透過proxy改変まで
検証するものではない。hosting環境のprivate/metadata egress firewallを別途設ける。
OS getaddrinfoに委譲したlookupは待機を5秒で終了するが、OS内の処理そのものをcancelする保証はない。

## WebClass授業一覧専用CLI

授業一覧の取得は、Windowsでは専用Edge、macOSでは専用Chromeを既定とする。以下は `--http-probe` を明示したときの
HTTP診断の通信制限。既存のTLS検証に加えてHTTPS 443の2ホストに限定し、合計最大7件で停止する。

| ホスト | 許可する本番パスとメソッド | GET / POST 上限 |
|---|---|---:|
| `hoppii2025.hosei.ac.jp` | 初回GET `/portal/u001/index.php` | 1 / 0 |
| `idp.hosei.ac.jp` | 認証前GET `/idp/profile/SAML2/Redirect/SSO`、POST `/openam/json/authenticate`、条件を満たす場合のみ `successUrl` GETと `/openam/saml2/consent/` GET | 3 / 3 |
| `lms2025.hosei.ac.jp` | 本番試験の範囲外 | 0 / 0 |

実測済みのIdP GETはパスが完全一致し、クエリに `SAMLRequest` と `RelayState` が
各1個ある場合だけ許可する。順序は任意。空の値、不正なパーセントエンコード、
制御文字、重複、追加のキーを拒否する。`/openam/XUI/` は直前の `Location` から
フラグメントをメモリ内で解析する。このパスへのGETは0回。

一覧専用CLIはHOPPIIの検証済みHTTPS応答から `SAMLRequest` を取り出す。標準Base64と
raw DEFLATEを使い、展開上限64 KiB、XML深さ上限32で解析する。DTDと外部エンティティは
許可しない。この経路では `AuthnRequest` にリクエストID、SPの `Issuer`、IdP SSOと
一致する `Destination`、HTTPS 443のHOPPII上のACS URL、`HTTP-POST` を要求する。
ACSは正規のパスを持つ絶対URLに限り、ACS indexによる選択は受け付けない。

認証POST前に `Issuer` とXUIの `spEntityID` を完全一致で照合する。認証成功後は同じ
試行の `goto` と `successUrl` を、パスとクエリを含めて照合する。値は試行中のみ
メモリに保持する。SAMLのリクエストIDとOpenAMの `ReqID` は別の識別子として扱う。

本番試験では `/openam/SSORedirect/metaAlias/` に英数字2セグメントが続き、クエリが
`ReqID`、`index`、`acsURL`、`spEntityID`、`binding` 各1個の場合に限り、GETを検討する。
`acsURL`、`spEntityID`、`binding` は同じ試行のAuthnRequestと照合する。この経路は直接
ACS URL指定に限定し、PingAMの公開例にある `index=null` を要求する。パスと値の全照合に
成功した時点で、同じ試行のGETを送信直前に1回予約する。

フォームの `action` はACS候補とメモリ内で照合し、SAML ACS POSTの前に停止する。
公開例との整合性に基づく試験であり、正式なSP登録情報の確認は引き続き必要である。
合成HTTPSの `--fixture` では従来の授業一覧取得を確認できる。

同意画面のGETは、検証済み `successUrl` の応答がHTTP 302で、単一の `Location` が
同じIdPのHTTPS 443上の正確な `/openam/saml2/consent/` を指す場合に限り検討する。
クエリは4 KiB・16項目までとし、重複、制御文字、不正なエンコードを拒否する。
本番で確認した `realm`、`idpEntityId`、`spEntityId`、`consentID`、`originalRequestURI` の
5キーを対象とする。`realm` は同じ試行のXUIのrealm、`spEntityId` はHOPPIIの
AuthnRequest.Issuer、`originalRequestURI` は取得済みの `successUrl` のパスとクエリに
照合する。`idpEntityId` はIdPのTLS応答に含まれた同一オリジンの正規のHTTPS識別子に
限定する。その識別子への通信は発生しない。正式なIdPメタデータとの照合は未確認。
未知のキーや不一致があれば、クエリ構造と照合結果を報告して送信前に停止する。
GETで受け取ったHTMLはメモリ内で解析し、
フォームの送信、スクリプトの実行、後続リダイレクトの追跡は行わない。
同意・拒否・ACSへのPOST、授業一覧、課題の `contents` は本番試験の許可対象外とする。

ホスト・メソッド別のカウンターはHTTPクライアントへの送信直前に加算し、上限到達後は
カウントを増やさず拒否する。TLSや接続で失敗した送信試行も上限に含まれる。
集計値は応答の有無を問わず表示する。失敗時は固定の診断カテゴリと、
該当する場合は固定の認証段階を表示する。認証値、URL、クエリ、レスポンス本文は出力しない。

### 承認後に実施するSSO経路の観測

VPNと既存設定を使い、本人の承認後に既存の一覧専用CLIを1回実行する。
OpenAMの3つのPOSTのうち、資格情報を送るのは2回目だけ。自動リダイレクトと
自動再試行は0回。同意画面を取得した時点か、そのGET条件が成立しなかった時点で停止する。
未対応のMFA、照合不成立、認証エラーでも停止する。

本人が大学公式の画面で同意した後、その状態が新しいCLI試行に適用されるかは
`university-check webclass --courses-only --allow-university-read --stop-before-consent` で確認する。
上限はHOPPII GET 1件、IdP GET 2件、OpenAM POST 3件の計6件。同意画面へのGETは
送信前のポリシーで拒否する。SAMLフォームが返った場合は、method、ACSとの一致、
`SAMLResponse` と `RelayState` の有無を報告する。SAML Responseの `InResponseTo` は
64 KiB・深さ32の範囲で、DTDを拒否して同じ試行のAuthnRequest.IDと照合する。
ここで確認できるのは応答との対応関係まで。SAMLの署名は未検証のまま停止する。

CLIはホスト・メソッド別件数、戻り先のクエリ構造とACSの照合結果、GETのHTTP
ステータス、SAMLフォームの認識と `action` の照合結果を報告する。同意画面の分岐では、
302のクエリ構造と、取得した画面の既知のラベル・属性名・フォームの構造も報告する。
URL、Cookie、トークン、資格情報、SAML本文を出力・保存しない。

### WebClassのブラウザ認証

ブラウザで本人が承諾した後の新しいCLI試行でも、検証済み `successUrl` は同意画面への
302を返した。OSSTechの実装では、送信属性の同意機能が暗号化した情報をブラウザの
ローカルストレージに保持する。大学がこの方式を使う場合、独立したHTTPセッションでの
再要求を説明できる。大学の製品・設定と、本人の同意選択の適用範囲は未確認。
[OSSTech OpenAM 14 SAML設定ガイド 7.1.1・7.2](https://www.osstech.co.jp/download/updates/docs/openam/OSTD-OpenAM14-SAML-SetupGuide.pdf#page=28)

授業一覧と1授業の課題確認CLIの `--allow-university-read` は、一時プロファイルでWindowsのMicrosoft Edge
またはmacOSのGoogle Chromeを起動する。Windowsでは標準のEdgeインストール先を探して `node.exe` を起動する。
macOSでは `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` とPATH上の `node` を使う。
WebClassが既存設定で無効な場合はブラウザ起動前に終了する。
`--check-local` は同じ設定チェックに加え、対象ブラウザとPATH上のNode.jsの実行ファイルを確認する。
ブラウザ経路の事前確認は設定の読み取りとファイルの検出だけで完結する。
`--check-local --http-probe` はHTTP経路の設定とOS資格情報項目をローカルで確認する。
ユーザーは大学公式のHOPPIIで認証と同意を行い、WebClassリンクを開く。SAMLフォームの送信と
セッション確立は大学のページが担う。中止は専用ブラウザを閉じる。待機上限は5分。
Windowsは終了時にジョブで子プロセスを終了し、Rustが一時プロファイルを削除する。
macOSはCLIとのパイプ切断時もNodeがCDPで専用Chromeを閉じ、必要な場合は自分で起動した
Chromeプロセスに終了シグナルを送り、その終了を待って一時プロファイルを削除する。
`--keep-profile` で指定する専用 `--user-data-dir` は、Windowsでは
`%LOCALAPPDATA%\AssignmentManagerWebClassEdge`、macOSでは
`~/Library/Application Support/AssignmentManagerWebClassChrome`。
アプリの所有マーカーを必須とし、パス上の再解析ポイントとシンボリックリンクを拒否する。
macOSではユーザー所有と専用ディレクトリの権限も確認し、CLIとのパイプが切れてもNodeがOSの排他ロックを保持する。
認証と同意の保持期間は大学とブラウザの設定に従う。異常終了後も新しいタブから開始し、復元タブがあれば停止する。
`courses` または `contents` が401または403の場合はその試行を終了し、再認証は次回の本人操作に委ねる。

ブラウザの制御は継承したDevToolsパイプに限定する。
専用のループバックプロキシはHOPPII、IdP、WebClassのHTTPS `CONNECT` のみ転送する。
TLS証明書はブラウザが検証する。許可済みオリジン内のリダイレクトと新規タブを処理し、
未知のトップレベルオリジンでは終了する。ダウンロードを拒否し、WebClass取得の前にService Workerを
バイパスする。

遷移先の診断は `webclass browser_navigation` 行に固定値だけを表示する。`stage` はブラウザ起動時の
`startup`、HOPPIIの `hoppii`、認証側の `idp`、WebClassへのSSO引き渡しの `webclass_sso`、
WebClassの `webclass` のいずれか。未知の遷移要求では、そのタブで最後に確認した段階を表示する。
`kind=top_level_request` はトップレベルの遷移要求、`top_level_commit` は表示先が切り替わった時点、
`browser_internal_page` はブラウザ内部ページを表す。ブラウザが通信失敗を示すページをトップレベルに
表示した場合は `browser_navigation_failed` と `kind=browser_error_page` を返す。iframeで拒否された
遷移やサブリソースの拒否だけでは認証を中断しない。URL、クエリ、Cookie、トークン、ブラウザの
エラー原文は診断に含めない。

エラーページを同じタブ、フレーム、読み込みIDのDevTools通信失敗と照合し、`cause` に
`dns_failure`、`tls_failure`、`connection_refused`、`connection_reset`、`connection_closed`、
`proxy_failure`、`timeout`、`navigation_blocked`、`unknown_network_failure` のいずれかを出す。
照合できたリクエストの `request_method` は `get` または `post` で、送信の試行を表す。
同じ許可先へのCONNECT失敗をプロキシが観測した場合は `proxy_connect_observed` を併記する。
これはブラウザがトンネル確立失敗を報告した遷移中の観測であり、個々のHTTPSリクエストとCONNECTの
対応までは確定できない。複数の異なる失敗が観測された場合は省略する。通信の証拠を照合できない
場合は `unknown_network_failure` とし、ログイン待ち時間切れや、取得APIの `auth_expired` と区別する。

WebClassのタブでログアウト操作または既知のダッシュボードを確認したら、同一オリジンの固定
`courses` に1回だけGETを送る。`mode` と `credentials` は `same-origin`、
`redirect` は `error` とする。完全なHTTP 200 JSON、UTF-8、本文上限、タイムアウトを確認する。
Cookieやローカルストレージはブラウザ内で管理する。Rust側は既存の `parse_courses`、
授業IDの検証、件数上限を共用し、授業の取得値と年度・学期別件数を端末に表示する。

`--course-id ID` を指定した場合、Rustで検証した授業一覧に完全一致する `group_id` がある場合だけ、
既存HTTPクライアントと同じ形式の `contents?group_id=…` を組み立てる。ブラウザタブ内で授業一覧を取得したフレームに
限定して1回のGETを許可する。埋め込みフレームにもリクエスト制限を適用し、ワーカーからWebClassへの
通信を拒否する。
授業一覧・課題取得のどちらでも、WebClass宛てのPOSTは認証中の `/webclass/singlesignon.php` への画面遷移に限る。
フォームの送信先にクエリが付いていても、許可済みのHTTPSオリジンと完全一致するパスで判定する。
応答全体を既存の
`parse_contents` と授業グループ・課題IDの検証、`map_contents_for_group` に渡し、検証を通った課題の
取得値を端末に表示する。認証情報はブラウザのプロファイル内で管理する。

ブラウザ経路から従来の固定 `ASK_IF_CHANGE` を送るHTTP同意処理を呼び出さない。
既存のデスクトップ・Web同期は `HoseiAuthenticator` とHTTPセッションの再認証を利用中。
課題取得と既存同期の移行が済むまでこれらを維持する。授業一覧用の `--browser` 互換指定、
`--http-probe`、`--stop-before-consent`、プローブ専用の観測コードは利用状況を確認して整理する。
同意APIを外部クライアントから利用する契約は、大学について未確認。
[OSSTech OpenAM 14 SAML設定ガイド 7.5](https://www.osstech.co.jp/download/updates/docs/openam/OSTD-OpenAM14-SAML-SetupGuide.pdf#page=36)

合成検証ではWindows EdgeまたはmacOS ChromeとループバックのHTTPSのIdP・SPを使う。テスト専用の操作でSAML遷移を開始し、
同じブラウザから授業と指定した1グループの課題のfixtureを取得する。プロファイルの再利用と分離、
異常終了後の再認証、未知の遷移先、ブラウザ終了、認証失効、リダイレクト、ページやワーカーからの
余分なGETとSSO以外のPOSTの拒否を確認する。IdPとWebClassのSSOでの通信失敗をブラウザエラーページと
して報告し、iframeや画像で通信が拒否されても正常なSSOを継続できることも確認する。CONNECT拒否、
切断、TLS失敗にはループバックのソケットを使い、名前解決失敗と接続タイムアウトは合成ダイヤルで
再現する。テストでは一時作成した専用プロファイルとループバックのサービスだけを使う。

```powershell
$env:ASSIGNMENT_MANAGER_TEST_EDGE = '1'
node --test tests/webclass_edge.test.mjs
```

```sh
ASSIGNMENT_MANAGER_TEST_CHROME=1 node --test tests/webclass_edge.test.mjs
cargo test --locked --offline chrome_driver_cleans_up_when_the_cli_disconnects -- --ignored
```

Edgeを既定にした際、従来の無効設定をEdge起動前に確認していなかったため、通常・全機能の
CLI回帰テストで実Edgeが計2件起動した。プロセスは停止し、テスト用の一時プロファイルを
削除した。大学へのHTTP送信の有無と件数は不明。再発防止として、無効設定を起動前に拒否し、
CLI回帰テストはブラウザを起動できない環境で無効設定の各モードを確認する。

### 授業の分類と連携

初回の本番取得で確認できたのは52件と、その応答内での授業IDの一意性。授業レコードを保存して
いないため、当回の年度・学期別内訳と過年度授業の有無は未確認。CLIは新しい取得や合成データの
検証時に、`year` と取得したままの `semester` ごとの件数を表示する。対象年度・学期の決定と
履修中かどうかの確認には、公式の履修情報、または本人が確認した授業の選択を使う。WebClassで
表示できることだけで履修中と判定せず、全件をNotionへ登録対象にしない。

WebClassの `group_id` は同じ一覧内で一意として扱う。年度をまたいだ安定性は、年度・学期の
異なる検証済み一覧で再割当や同一授業の識別子変化を確認するまで未確定。Moodleとの対応は
WebClassの `group_id` とMoodleのソース・カテゴリ識別情報を記録し、年度、学期、公式授業コード
または公式の対応情報で照合する。名称や曜日・時限は候補の絞り込みに使い、重複や曖昧な候補は
本人確認まで未対応とする。

課題取得を追加するときは、選択・確認済みの授業IDだけを同じEdgeセッションの固定 `contents`
GETに渡す。送信回数とレスポンス上限、返却された `group_id` の照合、既存の `parse_contents` と
マッピングを合成環境で検証し、最初は読み取りで確認する。その後に既存同期への接続を検討する。

## TLS / proxy / redirects / timeouts

URLをIPへ置換しない。reqwest/rustlsの標準certificate chain/hostname verificationとSNIを維持。
証明書/hostname検証無効化、独自TLS stackはない。TLS key loggingも明示OFF。
`no_proxy()` でsystem/environment proxyを無効にし、proxy独自の名前解決・CONNECTで
resolver制限が迂回される経路を閉じる。将来proxyが必要なら独立した検証済みegress設計が必要。

| Control | Web value |
| --- | --- |
| DNS lookup | 5秒、最大64 addresses |
| connect (TCP/TLS) | 5秒 |
| blocking header / body read operation | 5秒 |
| request全体（送信からbody終了まで） | 15秒 |
| automatic redirects / HTTP retry | 0 / 0 |
| manual auth redirect | 既存chainごと最大10、毎回origin検証、SAML POSTはreplayしない |
| WebClass reauth | 既存syncがAuthExpired時だけ1回 |

async ClientBuilderのtotal timeoutを保持してblocking builderへ変換し、blocking側の待機も5秒にする。
reqwest 0.13.5のasync `read_timeout` はblocking Response::ReadのTokio外pollでpanicするため
使用しない。この組合せはheader stall、body stall、3秒ごとに届くslow bodyの15秒停止で検証済み。
独自のretry loopは追加していない。全syncはcourse数・各redirect chain・再認証を含むので
15秒で終わる保証はない。

API参照: [reqwest ClientBuilder](https://docs.rs/reqwest/0.13.5/reqwest/struct.ClientBuilder.html)。
blocking builder/responseの挙動はlock済み0.13.5のローカルソースも確認した。

## Bounded bodies and encoding

| Type | Limit | 根拠 / compatibility |
| --- | --- | --- |
| Moodle ICS | 8 MiB | fixture最大1,585 bytesより十分大きい。例として8 KiB/eventを1,024件まで保持できる規模 |
| WebClass JSON courses / contents / verification | 8 MiB | fixture最大2,676 bytes。course単位の多めのmetadataを許容しつつ全件無制限bufferを廃止 |
| login / SAML / consent / CSSO / JS HTML、consent JSON | 1 MiB | form HTML fixture最大583 bytesに大きい余裕。SAML assertionはfixtureより大きいことを想定 |
| OpenAM callback JSON ×3 | 256 KiB | fixture最大1,174 bytes。既知callback treeに200倍以上の余裕 |

値は安全側の初期提案であり、実大学payloadの最大値を測定したものではない。
実大学payloadのサイズ測定は未実施。Web-4B3で別途承認された検証時にサイズのみを測り、必要時は種別ごとにreviewする。
既存正常fixture/parser contractは全て維持する。

Content-Lengthは早期拒否にのみ利用し、`Read::take(limit + 1)` で実bodyをbounded readする。
上限+1を読めば固定errorにし、partial textをparserへ返さない。truncationとinvalid UTF-8も拒否。
小さく偽装したContent-Lengthの後ろはHTTP framing上bodyではなく、余剰bytesをparserへ渡さない。
close-delimited / chunked / lengthなしも同じ上限。body全体の無制限 `bytes()` は3 moduleから除去した。

Accept-Encodingはidentity。4種のauto decoderを明示無効にし、Content-Encodingがidentity以外なら
bodyを展開せず拒否する。gzip feature等が別consumerから統合されても設定は変わらない。
有効なgzip（2 MiB超へ展開する約2 KiBのfixture）を拒否するtestを含む。
大学がidentityを無視して圧縮必須なら安全に失敗する。将来bounded decompressionを別reviewする。

## Credentials / errors / cookies

Web用SessionCookiesは正確なscheme/host/portごとにJarを分割する。
Domain=.hosei.ac.jpを受信しても同じresponse origin以外ではCookieを取り出せない。
IdP CookieはSPへ転送せず、SAML/CSSOの必要form fieldsだけを既存parserが渡す。
desktopは従来のshared Jarを使う。Webで大学のparent-domain Cookie共有が必須だった場合は
現在のstrict policyで失敗する。根拠なくCookie共有へ拡大しない。

自動RefererはOFF。WebClassの明示Refererはquery/fragmentを除去して送る。
Moodle fetchはRefererを設定しない。固定のprotocol Referer path/Originは維持する。
新TransportとNetworkErrorはraw reqwest/DNS/TLS errorを保存・log・source chainへ返さない。
既存Moodle/WebClass/Hosei error categoriesへ固定変換し、sync/HTTPにもraw causeを渡さない。
URL、request body、Cookie、password、SAML fieldをDebug/logへ追加していない。
VaultのAES-GCM、zeroizing CredentialStore、SettingsStatusのbooleanのみのDTOは変更していない。

## Test-only exceptions and remaining boundaries

`cfg(test)` にだけ存在するTestNetworkでsynthetic origins、loopback peer、fake DNS、isolated CAを注入する。
通常crate / all-features Web binaryには存在しない。HTTP例外やtrust rootのproduction設定口はない。
TLS正常系はprivate synthetic CAのみをtrustする標準WebPKI verifierを使用し、wrong host/untrusted certも拒否する。
これは大学証明書・hosting network・Windows verifierの実検証ではない。

policyはimmutableでhot reload経路を持たない。将来のpolicy更新時はruntime/sessionを再生成する。
Web-4B1の完了は安全なtransport constructorとoffline tests。Web-4B2でbackend統合と
per-run/course制限を追加した。実host/path・実SSO/hosting適合性、egress firewall、
公開用HTTPS/key/backup、Windows build/smokeは未確認/未実装。
[Phase Web-4B1](PHASE-WEB-4B1.md)に検証結果とWeb-4B2の接続計画を記す。


## Secured backend / whole-run budgets (Web-4B2)

[Phase Web-4B2](PHASE-WEB-4B2.md)でprivate client factoryによるapplication統合を追加した。
同じimmutable policyがMoodle、初回login、reauth、cached WebClassClientへ伝播する。
runごとの予算resetはsessionを再作成しない。別runtimeのsessionはbudget identity照合で拒否。
policy変更はadmission停止→drain→runtime再生成を必要とし、credential revisionとは独立する。

secured integrationは180秒の協調的deadline、512 requests、response lifetimeを含め同時1通信、
累積128 MiB body、128 courses/累積256 groups、4,096 parsed records/import batchを上限とする。
Cookieはexact origin最大3、session lifetime累積1,024更新/256 KiB、1 header4 KiB。
超過時は固定failureで停止し、既存commitを保持。shared sync/importを複製・batch分割しない。
Web-4B1 constructorを単独で使う場合のper-request制限と、factoryが付与するrun制限を区別する。

数値の根拠、制限をかける位置、safe error mapping、分割E2E証拠はphase報告を参照。
180秒はparser/SQLite commit/credential provider/OS DNSの強制cancel保証ではなく、
shutdownもworker終了までdrainする。body予算は消費した本文でありwire全体のbyte計測ではない。
Cookie上限は黙ってevictせずpoisonし、origin分離を維持する。desktopにはrun/Cookie予算を適用しない。

通常Web binaryはall-featuresでもDisabledNetworkBackend。任意CA/DNS/HTTP例外のproduction API、
CLI/envによるreal backend有効化はない。今回のReadyはoffline integration gateだけ。
Web-4AのWindows build/smokeは未完了・最終BLOCKEDのまま。

## Web-4B3 fixed offline HTTPS fixture

[Preflight](PHASE-WEB-4B3-PREFLIGHT.md)で跨crateの成功E2Eを追加した。
既存の汎用TestNetwork/任意CA・resolver seamはcfg(test)内部のまま。
加えて、明示的なoffline検証用build feature `synthetic-https-fixture` は、固定CA/4 synthetic hostsと
fixture自身のloopback listenersだけを持つ固定factoryを提供する。
all-featuresではこの固定facilityもcompileされるため、「全例外がcfg(test)だけ」というWeb-4B1時点の
説明からこの点が変わった。任意CA/DNS入力のproduction APIを提供するものではない。

fixed policyはproduction originsへfallbackせず、with_moodle_calendarでも許可先を拡張できない。
unknown DNS nameはOS resolverへ渡さない。HTTPS/443/TLS/SNI/hostnameと既存資源上限を維持し、
Moodleは既知の合成URLだけ、WebClassは既知の合成ID/passwordだけを送信前に受理する。
通常Web binaryはall-featuresでもDisabled固定。fixture起動もreal backendの選択もHTTP/env/CLIで
通常binaryへ追加していない。専用secured-synthetic exampleだけが固定factoryを使う。
大学exact host/path/current originsの未確認を埋める証拠にはしない。実通信は別途の明示承認が必要。
