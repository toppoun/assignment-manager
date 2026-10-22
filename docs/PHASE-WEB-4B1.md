# Phase Web-4B1: Secure University HTTP Transport

2026-10-22（Asia/Tokyo）、toppoun/assignment-manager、feature/web-migration。
検証環境: macOS / aarch64-apple-darwin。

**判定: Ready for Web-4B2（安全なtransportのMac/offline実装gate）。**
**Web-4AのWindows build / Browser smokeは引き続き未実施・未完了で、Web-4A最終判定はBLOCKEDのまま。**
この判定はWeb-4Aの判定変更、実大学通信、公開運用、Web-4B2実装の開始を意味しない。
今回は合成credentialと隔離loopback serverだけを使用した。commit / pushは行っていない。

## 1. Changed files

開始時点にWeb-4Aの未コミット変更が存在した。それらを保持した上でのWeb-4B1追加差分:

| Files | Change |
| --- | --- |
| src/university_network.rs（新規） | NetworkPolicy、strict URL、DNS guard、Web transport constructor、bounded read、origin分離Cookie store |
| src/university_network_tests.rs（新規） | offline URL/DNS/TLS/proxy/body/timeout回帰21 tests |
| src/lib.rs | network module公開（client実体・sendはcrate内部） |
| src/moodle_http.rs | for_web、元URL syntax検査boolean、shared transport/bounded ICS読取、desktop HTTP retry OFF明示 |
| src/webclass_http.rs | authenticated transport引継ぎ、JSON/login HTML上限、raw client注入はtest限定 |
| src/webclass_auth.rs | for_web、全stepでTransport::send、JSON/HTML上限、Web Cookie partition、既存protocol/redirect/reauth契約維持 |
| src/webclass_auth_tests.rs | 既存mockのconstructor/send変更を最小追従、過大body testだけdisconnect許可、Web full SSO/consent・危険遷移13種・各auth body過大の3 tests追加 |
| fixtures/network/（新規） | 合成CA/leaf DER、公開test key、valid gzip expansion fixture、説明 |
| Cargo.toml / Cargo.lock | tokio net/timeをdirect依存（resolver用）、rustls dev依存（合成TLS server用） |
| crates/application/Cargo.lock、crates/web-server/Cargo.lock、crates/macos-bridge/Cargo.lock、src-tauri/Cargo.lock | rootのdirect tokio dependency edgeだけを追加。既存versionの一括更新なし |
| README.md、crates/web-server/README.md、crates/application/README.md | completed transportと未接続backend/Windows gateの区別 |
| docs/WEB-NETWORK-SECURITY.md（新規） | inventory、policy、制限値、threat model、test例外、残る運用境界 |
| docs/WEB-MIGRATION-REVIEW.md、WEB-AUTHENTICATION.md、WEB-CREDENTIAL-VAULT.md | Web-4B1追補。過去phaseの判定・credential保存contractは維持 |
| docs/PHASE-WEB-4B1.md（新規） | 本報告 |

CoreSyncBackend / SyncService / SyncWorker / ApplicationHandleのRust実装、sync.rs、WebSyncCoordinator、
MockSyncBackend、Web route/state/vault実装、frontend、DB schema、FFI/Swift APIは今回変更していない。
既存Web-4Aの変更をWeb-4B1で実装したものとして集計しない。

## 2. Existing outbound request inventory

大学HTTP生成箇所はMoodleClient、AuthSession（WebClassClientへ同じclientを渡す）の2系統。
全send/全body消費を調査した。[完全なmethod/endpoint/body一覧](WEB-NETWORK-SECURITY.md#outbound-inventory)を参照。

- Moodle: calendar GET → ICS。
- WebClass: courses GET、contents GET、login後courses検証。HTMLの既知login patternをAuthExpiredとする契約維持。
- OpenAM: HOPPII user page GET → IdP SSO GET → XUI fragment parse → authenticate POST ×3。
- SAML/consent: IdP success GET、getAttributes/agree POST、return GET → HOPPII ACS form POST。
- HOPPII: ACS後GET chain、portal login GET chain、user page確認GET chain、Parm GET。
- WebClass SSO: CSSO POST → bounded HTTP/JS GET chain → courses array検証。

以前からMoodleのHTTPS/userinfo/redirect/Referer制御、WebClassのorigin/step/redirect上限/
retry禁止/secret-safe errorは存在した。任意Moodle HTTPS host/IP/port、DNS/connect制限、
Web proxy無効化、全body上限は存在しなかった。HTTPSだけを安全性の根拠にしていない。

## 3. Security policy architecture

既存HTTP layerにprivate Transportを共通化し、明示Web constructorへNetworkPolicyを注入する。
認証成功時にWebClassClientへpolicy付きtransportをそのまま渡す。Client抽出・resolver交換・
proxy追加・TLS緩和を可能にするproduction public APIは作らない。
既存desktop constructorは維持し、全consumerでbody読取のみ共通の有限処理へ置換した。
desktopをWeb用SSRF保護済みと主張しない。

## 4. URL / origin policy

Moodleのproduction hostname/pathは未確定のためdefault policyを未設定にしてfail closed。
operatorによるexact host/path注入APIはあるが、server設定・env切替には接続していない。
HTTPS/443、exact DNS name/path、userinfo/fragment/IP literal拒否と、元URLの構文偽装拒否を実装。
token queryはsecretのまま。URL変更でscopeを変更せず、CourseRelated contractを維持する。

WebClass production originsは既存ソースのhoppii2025.hosei.ac.jp / idp.hosei.ac.jp /
lms2025.hosei.ac.jpのHTTPS/443だけ。年度hostnameの稼働確認はしていない。
既存step-specific origin検査とtransportのexact allowlistを両方通す。
共有resolve_destinationでも元authorityの空userinfo/escaped host/制御文字等を拒否し、正規化で偽装が消える前に検査する。
変更は明示policy/code reviewを要求し、wildcardやresponse由来のorigin追加を行わない。

## 5. DNS / SSRF protection

reqwest resolver injectionで、実connectに渡すA/AAAA集合そのものを検査する。
private等が1件でも混在すれば全体拒否。最大64、空/失敗/timeoutも拒否。
IPv4/IPv6 special-purpose、loopback/private/link-local/metadata/reserved、mapped IPv6を拒否。
全fallback先は同じ検査済みiterator内のみで、通常DNSへのfallbackはない。
再lookupもguardを通り、pool再利用は既存の検査済みsocketだけ。
public応答・混入・複数A/AAAA・応答変更・別IP fallback防止をfake resolverで検証した。

## 6. TLS / proxy / redirect behavior

reqwest/rustls標準のchain/hostname検証とSNIを維持。URLをIPへ置換しない。
ローカルHTTPS正常系、untrusted CA、hostname不一致を合成証明書で検証した。
system/environment proxyはno_proxyで無効化。大文字/小文字のHTTP(S)/ALL_PROXYを設定した
隔離子processとproxy trapで、private DNS拒否をproxy経由で回避しないことを検証した。

DNS 5秒、TCP/TLS connect 5秒、blocking header/read操作5秒、request全体15秒。
headers/body stallと3秒ごとに届くslow bodyで有限終了を検証した。
blocking Response::Readとasync read_timeoutの非互換を実テストで検出して修正し、
async total timeout + blocking wait timeoutを使用する。独自TLSや無制限retryはない。

Moodle自動redirectは0、WebClass自動redirect/retryも0。手動認証chainは既存最大10と
step別origin検査を維持。SAML POSTをredirect先へ再送しない。既存reauth onceは変更しない。

## 7. Response body limits

| Body | Limit |
| --- | --- |
| Moodle ICS | 8 MiB |
| WebClass JSON（courses / contents / login verification） | 8 MiB |
| login / SAML / consent / CSSO / JS HTML、consent JSON | 1 MiB |
| OpenAM callback JSON | 256 KiB |

fixture最大はMoodle 1,585、WebClass 2,676、auth HTML 583、OpenAM 1,174 bytes。
既存入力に大きい余裕を持たせつつ有限化した初期値で、大学production最大値の測定結果ではない。
詳細な容量の考え方は[body設計](WEB-NETWORK-SECURITY.md#bounded-bodies-and-encoding)参照。
Content-Lengthだけに頼らず、上限+1まで実readする。無限bytes読取を廃止し、過大bodyから
partial parser入力を作らない。未消費redirect/status-only bodyは全体を読まずdropする。
identityを要求しauto decompressionを全てOFF、圧縮responseは展開前に拒否する。

## 8. Credential protection

Web-3B encrypted CredentialStore、zeroizing credentials、boolean-only SettingsStatusは不変。
WebのCookieはexact originごとにpartitionし、parent Domain Cookieのsibling送信も防ぐ。
認証passwordは既存OpenAM authenticate、SAML/CSSOは既存の検証済みform送信先だけへ渡す。
Referer自動送信OFF、Webの明示Refererからquery/fragment除去、MoodleはRefererなし。
NetworkErrorは固定categoryだけで、reqwest/DNS/TLS causeやURL/bodyを保持せず既存safe errorへ変換する。
token/password/Cookie/SAML/request bodyをlog・debug・HTTP responseへ追加していない。

## 9. Backend injection strategy

今回の通常Web binaryはfeature有無にかかわらずDisabledNetworkBackend。
synthetic-sync専用test/exampleは既存MockSyncBackend。実大学backendの切替env/CLIはない。
通常sync routeにはsecured clientさえ接続していない。

Web-4B2候補はCoreSyncBackendの小さなfactory拡張: desktop defaultを維持しつつ、trusted
Web用factoryから両方の `for_web(policy.clone())` を選ぶ。初回loginとreauthに同じpolicyを渡す。
policy更新時はruntime/sessionを再生成し、古いpolicyのcached sessionを再利用しない。
sync/importを複製せず、sync_moodle_once / sync_webclass_all_with_reauth / SyncService /
SyncWorker / ApplicationHandleを再利用する。まだこのbackend/factoryを実装していない。

## 10. Automated tests

core **336 unit + 11 doctests PASS**（従来312 + 新規24）。追加の21 network testsと3 auth testsで:

- URL allow/deny、suffix/authority/path偽装、scheme/port/userinfo/fragment、IPv4/IPv6 literal。
- A/AAAA public集合、private混入、empty/error/過多/timeout、port override拒否、rebind相当の応答変更。
- 検査後connectは同じpeer、次のprivate結果は接続せず拒否。proxy環境変数の迂回拒否。
- ローカルHTTPS/SNI/Host、正しいCAだけで成功、未信頼証明書/hostname不一致を拒否。
- Moodle同origin/別origin redirect禁止、Refererなし。
- 全body kindの小さい入力、上限ちょうど、Content-Lengthなし過大、chunked過大、偽装length、
  truncation、invalid UTF-8、valid gzip expansion拒否、固定error/no source chain。
- DNS/TLS connect/header/body/total timeout。
- Cookie origin partition（親Domainを含む）、Referer query除去。
- secured full SSO with/without consent、危険なSAML/consent/HTTP/JS遷移13種のtrap、
  OpenAM3段階/SAML/consent JSON/CSSO/WebClass HTMLのbody上限。

TLS/private IP/HTTPの例外はcfg(test)だけで、all-features production buildには含まれない。
合成test CAはOSへインストールせず、private test WebPKI trust storeだけで使用した。
最初のsandbox内loopback bind拒否、TLS fixture/timeout/mockの修正前失敗をPASSへ数えていない。
最終suiteはloopbackを許可した環境でlocked/offline実行。大学/外部DNSはtest対象に使っていない。

## 11. Existing regressions

| Target | Tests / docs | fmt --check | clippy all-targets/all-features -D warnings |
| --- | --- | --- | --- |
| core | 336 / 11 PASS | PASS | PASS |
| application | 32 / 2 PASS | PASS | PASS |
| Tauri (Mac) | 18 PASS、binary-onlyでdoctestなし | PASS | PASS |
| macOS bridge Rust | 18 / 0 PASS | PASS | PASS |
| web-server all-features | 56 / 0 PASS | PASS | PASS |
| web-server default | 49 / 0 PASS | 同manifest | 同manifest |
| frontend Node | 76 PASS | — | — |

Swift/Xcodeは共有Rust transportがnative同期にも影響するため実行:
`bash macos/BridgeSmoke/build.sh` のstrict Swift build、6 tests、合成FFI executable smoke PASS。
`bash macos/AssignmentManager/build.sh` のstrict Swift 32 tests、arm64 Debug Xcode build PASS。
XcodeはAppIntents.framework未使用によるmetadata抽出skip warningのみ。native UI/Keychain/
大学のmanual smokeを行ったという意味ではない。generated tracked sourcesに差分はない。

再現コマンド（各独立manifestで実行、MacではtestにTMPDIR=/private/tmp）:

```sh
cargo fmt --check
cargo test --locked --offline
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
# --manifest-path でapplication、src-tauri、macos-bridgeにも同じchecks
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline --all-features
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
cargo fmt --manifest-path crates/web-server/Cargo.toml --check
cargo clippy --manifest-path crates/web-server/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
node --test tests/*.test.mjs
bash macos/BridgeSmoke/build.sh
bash macos/AssignmentManager/build.sh
```

Windows build / smokeは要求範囲外のため試行していない。Web-4Aの未完了記録を維持する。

## 12. Security review

| Review | Result / evidence |
| --- | --- |
| 全大学HTTP pathのpolicy | PASS（Web constructorsのscope）。全AuthSession sendとWebClassClient引継ぎを追跡。通常Webはdisabled |
| 新client生成の迂回 | PASS（secured object内）。client private、raw client入力test-only、reauth用authenticatorもpolicy保持。desktop constructorは意図的に別contract |
| HTTPS/TLS検証 | PASS（offline）。SNI/Hostを維持、valid/invalid CA/hostname tests。大学実証明書は未確認 |
| 任意URL fetch | PASS（Web constructors）。exact host/path/origin + guarded DNS。Moodle未設定は拒否 |
| redirect検証 | PASS（offline）。Moodle0、SSOは既存step別origin/max10、未知遷移trap |
| 別originへのsecret漏洩 | PASS（offline contract）。password送信先/role検証、Cookie partition、SAML/CSSO正当なcross-origin POSTのみ |
| body上限 | PASS。消費するbodyは共通bounded read、残りは未読drop、圧縮拒否 |
| secret-safe error | PASS。固定既存errorへ変換、causeなし、Debug/log追加なし、既存Web safe DTO回帰 |
| desktop互換性 | PASS（Mac automated）。312既存core、consumer、Swift/Xcode回帰。実大学/Windowsは未確認 |
| 通常Webから大学通信不能 | PASS（現コード/回帰）。DisabledNetworkBackend固定、startup OFF、env有効化なし、mock tripwire維持 |

PASSの対象は記載した実装・offline検証範囲だけ。実大学SSO/HTTP、hosting egress、production
certificate運用、Windowsは**未確認**であり、PASSに含めない。

## 13. Remaining risks

- Moodle exact host/path、現行年度3 origins、hostingからのSSO/MFA/WAFとCookie partition互換性はoperator確認待ち。
- 上限値はfixture基準。大学payloadの上限、identity encoding、圧縮必須有無は未測定。policy違反時は停止する。
- per-requestの有限性を実装した。全run時間、course/group数、累積Cookie数、network concurrency/
  memory budget、OS DNS処理の中断保証は未実装。大量sourceでshutdown drainが長くなり得る。
- IANAの将来変更、特殊なroute/NAT/transparent proxy、大学origin自体の侵害はapplication guardだけで解決しない。
  hostingのegress firewall、DNS運用、dependency保守・log/APM/proxy redactionの確認が必要。
- Windows未検証、Web-4A最終BLOCKED、公開用HTTPS/key provider/ACL/backup/restore未完成。

## 14. Web-4B2 implementation proposal

1. operatorがsecretを文書/logへ載せず、Moodle exact hostname/path・CourseRelated export契約を確認。
   年度3 origins、SSO手順、hostingネットワークからの利用可否/大学条件も確認する。
2. CoreSyncBackendにtrusted transport factory/policyを最小注入し、同一policyをMoodle・初回login・
   reauth onceへ適用。SyncService/session revision/partial commits/completionをそのまま再利用する。
3. まず別の隔離test経路でApplicationHandle→core parser/import→secured transportを検証する。
   任意envで通常serverを有効化せず、backend constructionを明示reviewできる構造にする。
4. course/group数・全run deadline・累積資源上限を設計し、正常入力を不必要に制限しない値をreview。
5. ユーザーの別途明示依頼の下でのみ、実credentialによる限定smokeを実施。サイズ/固定stage/codeだけを
   記録し、URL/query/password/Cookie/SAMLを記録しない。失敗時にallowlist/TLSを緩めない。
6. Windows build/smokeの未完了gateを独立して完了させる。公開/deployment判定はさらに別gate。

**Ready for Web-4B2** — Mac/offlineのtransport準備完了まで。今回はWeb-4B2へ進んでいない。
**Web-4AのWindows検証は引き続き未完了、Web-4A最終判定BLOCKED。**
