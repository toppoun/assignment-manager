# Phase Web-4B2: Secured Core Sync Backend Integration

2026-10-22（Asia/Tokyo）。Repository: toppoun/assignment-manager。
Branch: feature/web-migration。macOS / aarch64-apple-darwin。

**判定: Ready for Web-4B3（コード統合とMac/offline検証のgate）。**
実大学への通信許可ではない。実大学server/DNS、実token/password、通常credential storeは使用していない。
Web-4B3は設計だけで、実装・実行していない。commit / pushは行っていない。
**Web-4AのWindows build / Browser smokeは未完了、同phase最終判定はBLOCKEDのまま。**

## 1. Changed files

| Files | 内容 |
| --- | --- |
| src/secured_sync.rs（新規） | immutable policyを所有するSecuredSyncClients、run予算、sessionのruntime束縛 |
| src/secured_sync_tests.rs（新規） | 既存SSO scriptをHTTPS化した隔離E2E / smoke、parser→SQLite v3 |
| src/lib.rs | secured factory module公開 |
| src/university_network.rs | shared run予算、response lifetime lease、累積body/時間、bounded origin別Cookie |
| src/university_network_tests.rs | request/同時通信/course/group/record/bytes/deadline/Cookie上限、TLS timeout、private DNS trap |
| src/moodle_http.rs / webclass_http.rs | Web専用checkpoint、session所有権照合。desktopはcheckpoint無効 |
| src/webclass_auth.rs | response leaseを次redirect前に解放、テスト専用HTTPS endpointsの受渡し |
| src/webclass_auth_tests.rs | 既存scriptをTLSでも再利用する内部fixture整理。既存assertionを維持 |
| src/sync.rs | 既存の同じsync関数内にWeb専用course/record/group/commit前checkpointを追加 |
| crates/application/src/sync_service.rs | factory injection、SecuredCoreSyncBackend、run開始hook、同一factoryによる初回login/reauth |
| crates/application/src/runtime.rs / lib.rs | desktop constructor維持、secured backendの明示注入とexport |
| crates/application/src/sync_tests.rs | run hook、実handleの未設定/未承認policy拒否、shutdown回帰 |
| crates/web-server/tests/sync_http.rs | 実secured backend→認証API/Vault/coordinator/workerのfail-closed回帰 |
| docs/PHASE-WEB-4B2.md（新規） / WEB-NETWORK-SECURITY.md | 本報告、資源/運用境界 |
| README.md / crates/application/README.md / crates/web-server/README.md | 統合済みAPIと通常binary無効の区別 |

DB schema、source identity、persistence/import実装、credential暗号format、frontend、FFI/Swift API、
manifest/lockfileは変更していない。Web route/state/coordinator/vault本体も変更していない。

## 2. Backend architecture / 選択理由

| 候補 | 変更範囲 / 判断 |
| --- | --- |
| CoreSyncBackendにfactory injection | client生成3箇所とrun hookを集約。既存3同期methodを1組だけ保持。採用 |
| 独立したWeb backend実装 | 同じMoodleFeed生成、counts/error変換、login/reauth呼出しを再記述するため不採用 |
| Web側に別source runner/import/completion実装 | source semanticsが分岐するため不採用 |

```text
Browser Sync API → WebSyncCoordinator → ApplicationHandle::open_with_backend
  → SyncWorker → SyncService::run（同じ実装）
  → CoreSyncBackend<SecuredSyncClients> = SecuredCoreSyncBackend
       ├ MoodleClient::for_web(policy)
       │   → sync_moodle_once
       └ HoseiAuthenticator::for_web(policy)（初回 / reauth共通）
           → 同じsecured Transport / origin別Jar → WebClassClient
           → sync_webclass_all_with_reauth → 既存sync/import → SQLite v3

通常Web binary: DisabledNetworkBackend（all-featuresでも固定）
既存synthetic example: MockSyncBackend
今回の隔離検証: secured core E2E + application/Web境界の分離tests
```

private ClientFactoryをdesktop / securedの2種類だけ実装。public aliasのSecuredCoreSyncBackend::newは
NetworkPolicyを必須とし、Default、Optionからのfallback、frontend設定、CLI/env切替はない。
ApplicationHandle::openはdesktop factoryを従来どおり選ぶ。通常Webの起動/設定に接続していない。

## 3. NetworkPolicy propagation

SecuredSyncClientsはoperatorのpolicyから独立したrun予算を1つ作る。NetworkPolicyは不変な設定だけを保持し、mutableなRunBudgetはfactory/transportの別fieldで所有する。
policyのcloneと同じ予算ArcをMoodle、
初回authenticator、reauth authenticator、auth成功後のWebClassClientへ一貫して渡す。
SyncServiceがrun開始時に予算だけをresetする。再同期のたびにWebClassClient/Jarを作り直さない。
policyのhot reload/replacement APIはない。policy変更時はadmission停止→drain→旧runtime破棄→新runtime。
古いsessionと新factoryは予算Arcのidentityが一致せず、backendでもinvalid_configurationで拒否する。
このidentityはDBに保存しないruntime境界であり、credential revisionの代替やpolicy revision番号ではない。

## 4. Moodle integration

raw secretのstrict syntaxとoperator確認済みexact host/pathを先に検証し、MoodleFeedは既存の
`default.course-related` / CourseRelatedを使う。URL/token変更でidentityを変更しない。
未設定のMoodle allowlistはinvalid_configuration、HTTP/timeout/body失敗はnetwork_failed。
同期本体はsync_moodle_onceだけ。parser、category course discovery、positive observation、既存importを再利用。
Moodle失敗後もSyncServiceはWebClassを試す。共有run予算を使い切った場合は次sourceも安全に失敗する。

## 5. WebClass login / reauthentication

同じfactoryのauthenticator()を初回loginとsync_webclass_all_with_reauthの引数に使用する。
secured経路からHoseiAuthenticator::newへ戻る分岐はない。返されたsessionのtransport/partitioned Jarは維持。
AuthExpiredだけfresh credential callback→login→full retryを1回。2度目のexpiry、その他失敗で停止。
retryのrequest/body/group予算は同じrunに累積し、resetしない。成功countsは最後のattemptだけ。
redirect応答は検証後に明示dropしてから次requestへ進み、response leaseが2通信を同時に許可しない。

## 6. Session / credential revision / source semantics

SyncServiceの1 cached session、SettingsServiceの成功したsave/delete後のrevision更新と、
revision不一致時に古いsessionを先にdropする実装を維持した。設定read error/削除もsessionを破棄する。
Web-4Aのowner別mutation admissionは不変。同期待ち中のsave/deleteは409、mutation中のsyncは503。
HTTP切断でもworker完了までguardを保持する。ApplicationHandleの直接呼出しをWeb admissionの代用にしない。

source独立実行、partial commit、positive observation、stable identity、completion所有権、single-flight、
shouldReload、safe error mappingは既存実装を使用。課題の削除・scope変更・batchの分割は行わない。
超過batchはそのbatchのimport前に拒否し、以前commitしたcourse/group/他sourceは残す。

## 7. Whole-run resource limits

値は本番大学payloadを測定した上限ではなく、既存コード・fixture・過去desktop記録から定めた
単一accountの初期運用上限。実接続前にoperator reviewを必要とする。browser/envから上限を増やせない。

| Resource | Web secured integrationの上限 / 根拠 |
| --- | --- |
| run経過時間 | 180秒。Web-4A UIの1.5秒×120回pollingと同じ時間幅。request開始時のremainingで15秒timeoutを短縮し、read/parse後/commit前/各courseでも検査 |
| course list | 128 records。3-course fixtureの42倍超、通常の単一accountを対象とする保守的な作業量の上限。大学の最大course数を確認済みという意味ではない |
| group取得 | 累積256。128 courseのfull retry onceを許容。最初の失敗/再認証も同じ予算 |
| HTTP request | 累積512。既存SSOの全redirect chain（各10）/consent/verificationを含むloginは最大約55 request、2回の128 group試行でも約369。余裕を含め512、無制限retryなし |
| 同時外部HTTP | runtimeのrun当たり1。bodyの消費/dropまでleaseを保持し、2件目を拒否。単一account/単一runtimeの範囲。将来multi-userのprocess全体semaphoreは別設計 |
| 累積body | 128 MiB + 超過検出1 byte。Web-4B1の8 MiB bodyを16個分。全courseが8 MiBなら途中で停止する意図的な総量上限。headers、TLS overhead、未読dropしたbody、reqwest内部read-aheadを含むwire byte計測ではない |
| SQLite import | parsed source records最大4,096/batch（filtered/hiddenを含む）。8 MiBを2 KiB/recordで割った作業量の目安。過去desktopの15 Moodle / 146 WebClassに約28倍以上の余裕。body capと併用し、既存の1batch transactionを分割しない |
| Cookie | exact origin最大3。session lifetime累積Set-Cookie更新1,024、header bytes合計256 KiB、1 cookie header4 KiB。既存fixtureは少数のcookie、認証本文より小さいstateに制限。期限切れ/path違いも累積数に数えてJar内の残留も有限化 |
| session | cached 1、reauth中だけ旧/新の2。Cookie超過はpoisonして送受信を拒否。silent evictionやorigin共有、同期ごとの無条件再loginをしない。回復にはcredential更新または明示runtime再生成が必要 |
| shutdown | 既存のHTTP/job admission停止→job/mutation drain→application shutdown→auth closeを維持。新予算はdrainの改善でありhard killではない |

**180秒はrunの強制終了保証ではない。** Rust parser/mapper、SQLiteの実行中transaction/commit、
credential provider/SettingsWorkerのblocking reply、OS内のgetaddrinfoを強制cancelするAPIはない。
SQLite busy timeoutは既存5秒だが、各statement/ディスクstall/CPU/commit全体のwall timeを保証しない。
checkpoint間の処理で180秒を超え得る。最終commitの直前に開始できても完了が期限後になる場合がある。
期限後にworkerをdetachしてadmissionを解放する処理は追加していない。shutdownは真のworker終了を待つ。
外部supervisorによる強制終了では、commit済みpartial dataを保持し、結果不明を成功としない。

## 8. Security boundaries

Web-4B1のexact host/origin/path、HTTPS/443、DNS-to-connect検査、no_proxy、標準TLS/hostname/SNI、
redirect constraints、bounded bodies、identity encoding/no decompression、Cookie origin isolation、safe Refererを維持。
productionで任意CA、任意DNS、任意clientを渡すAPIを追加していない。TestNetwork追加fieldもcfg(test)のみ。
SSOがCookie共有を要求する場合は失敗させ、理由は固定AuthStep/categoryで調査する。許可origin/TLSを緩めない。

NetworkErrorのResourceLimitも固定文言/no source chain。Web DTOは既存SyncFailureだけで、
上限種別を含むraw errorやsecretを返さない。HTTPでの期限/総量はnetwork_failed、
course/record validation checkpointでの超過（期限を含む）はsource_data_invalid。初回loginのtransport失敗は既存auth mapping、reauth login失敗はauthentication_failed。
URL/ID/password/Cookie/SAML/bodyを新たにlogしない。暗号化Vault/zeroizing secrets/owner/CSRF/recent reauthは不変。

## 9. Offline end-to-end verification

別crateからcoreのcfg(test)を利用できるとは仮定していない。次の2境界に分割した。

1. core unit E2Eは実際のSecuredSyncClients、isolated CA、fake DNS、loopback HTTPS、既存SSO script、
   既存sync/parser/import、SQLite v3を通す。HOPPII/IdP/WebClass/Moodleはsynthetic.invalidだけ。
2. application/Web testsは実SecuredCoreSyncBackendを明示注入。認証API→暗号化Vault→coordinator→
   ApplicationHandle→SyncWorker→SyncService→secured factoryの未設定/SSRF拒否を確認。
   成功source orchestration/revision/admission/HTTP切断/drainは既存mock回帰で検証する。

**同一のWeb HTTP requestからsynthetic HTTPS成功までを1つのcross-crateテストで通したという主張はしない。**
安全なproduction APIにテストCA/DNSを公開せず、実装を共有する境界ごとの検証を選んだ。

| 要件 | Evidence |
| --- | --- |
| A / B / C: secured Moodle/WebClass/両成功 | core secured_sources_matrix…、TLS/SNI/Host、full SSO+consent、SQLite import |
| D / E / F: 片方/両方失敗 | 同matrix4組、既存application/Web source matrix。第二source独立実行 |
| G: 未設定 | 実application handle / Web secured backendで両not_configured、reload=false |
| H: session reuse | core secured_session_reuse…で2回目にSSOなし・同Cookie、run予算だけreset |
| I: credential revision | 既存application replacement/removal、Web stable_import… / successful_delete…（mock）。policy epochは別factory owns_session=false |
| J / K: reauth once / secured維持 | core secured_partial_commit…：初回と再認証を同fake DNS/isolated CA/TLS/partitioned Cookieで成功。2度目expiryで停止 |
| L: policy missing | 引数必須compile-fail doctest、default Moodle policy拒否、Web safe DTO |
| M: SSRF / 無通信 | private DNSを許可しないsecured sync + bound trap、URL拒否時lookup0、Web trap |
| N: bounded response | secured Moodle 8 MiB+1、既存全auth body/JSON limits、圧縮/truncation tests |
| O: timeout | remaining run timeを300 msにした実TLSの600 ms header stall拒否、既存connect/header/body/total timeout |
| P: partial commit | secured first group commit→expiry→2度目expiryでも保持。course/record制限後もMoodle commit保持 |
| Q / R: completion / duplicate | 同一sessionで再同期、created0、unchanged、10件一覧保持、completion維持、空listでもpositive seen値保持 |
| S: secret/error非漏洩 | core固定error/no cause、Web safe DTO、Vault ciphertextにsynthetic secretなし、既存error tests |

追加資源testsは正確なrequest/group/record上限、response lifetime中の並列拒否、累積body超過、
期限超過、3-origin/Cookie header/count/bytesの各上限、超過時lease解放を検証。

## 10. Existing regressions

locked/offline実行（MacではTMPDIR=/private/tmp）。

| Target | Tests / doctests | fmt --check | clippy all-targets/all-features -D warnings |
| --- | --- | --- | --- |
| core | 346 / 11 PASS | PASS | PASS |
| application | 34 / 3 PASS | PASS | PASS |
| Web all-features | 57 / 0 PASS | PASS | PASS |
| Web default | 49 / 0 PASS | 同manifest | 同manifest |
| Tauri Rust (Mac) | 18（binary-only）PASS | PASS | PASS |
| macOS bridge Rust | 18 / 0 PASS | PASS | PASS |
| frontend Node | 76 PASS | — | — |

core追加10 tests、application追加2 tests + policy必須doctest、Web追加1 test。
最初のsandbox内loopback bind拒否とfixtureのhostname assertion修正前の失敗はPASSに含めていない。
最終検証ではloopbackを許可し、大学/外部DNSへは接続していない。

再現:

```sh
TMPDIR=/private/tmp cargo test --locked --offline
TMPDIR=/private/tmp cargo test --manifest-path crates/application/Cargo.toml --locked --offline
TMPDIR=/private/tmp cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline --all-features
TMPDIR=/private/tmp cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
TMPDIR=/private/tmp cargo test --manifest-path src-tauri/Cargo.toml --locked --offline
TMPDIR=/private/tmp cargo test --manifest-path crates/macos-bridge/Cargo.toml --locked --offline
node --test tests/*.test.mjs
# 各manifestでcargo fmt --checkおよび以下も実行
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
bash macos/BridgeSmoke/build.sh
bash macos/AssignmentManager/build.sh
```

## 11. Mac build / isolated synthetic smoke

UniFFI生成→strict Swift build→BridgeSmoke **6 tests + 合成FFI executable PASS**。
AssignmentManager **strict Swift 32 tests + arm64 Debug Xcode build PASS**。
生成済みtracked FFI sourcesに差分なし。XcodeはAppIntents.framework非依存によるmetadata skip warningのみ。
実native大学login/Keychain UIのmanual smokeではない。

新secured経路のsmokeは隔離fixture harnessを明示起動して確認した。Browser画面で大学同期したものではない。

```sh
TMPDIR=/private/tmp cargo test --locked --offline --lib secured_backend_tests -- --nocapture
```

出力・assertionで確認: policy/factory生成、synthetic credentials、HTTPS Moodle取得、HTTPS SSO完了、
SQLite一覧10件（Moodle1/WebClass9）、再同期created0、completion保持、partial failure/commit保持、
secured reauth once、body/course/batch制限。SSRF/timeoutは前節の実TLS/trap tests、credential変更後の
session破棄は既存application/Web testsで別に確認した。CAをOSへインストールせず、memory/temporary DBだけを使用。

## 12. Windows pending verification

Windows build / Browser smokeは今回は要求外・未実施。
Web-4A報告書を変更せず、**Web-4A最終判定BLOCKED**を維持する。
本phaseのMac/offline Readyと、Windows/公開運用/実大学接続の承認を混同しない。

## 13. Remaining blockers / limitations

このphaseのコード統合・offline gateに残るblockerはない。次の実大学/公開運用gateには残項目がある。

- Moodle実exact host/pathとCourseRelated契約、年度3 origins、大学側利用条件のoperator確認。
- strict origin Cookie分離・identity encodingで実SSO/MFA/WAFが動くか、hosting環境からの到達性は未確認。
- 上限値の本番適合性、全runのhard cancellation、OS DNS/credential/SQLite stallの中断保証はない。
- Web-4A Windows gate、production HTTPS/key provider/ACL/backup/restore、egress firewall、multi-user上限は未完了。
- cross-crateの成功経路検証は上記の分割方式。通常Web binaryにsecured backendを接続する実装はない。

## 14. Web-4B3 real connectivity validation plan（未実装・未実行）

1. 別途の明示依頼と大学側条件の確認後にのみ開始。既存通常Web serverのCLI/env toggleは作らない。
2. operatorがMoodle exact hostname/path、scope/export契約、年度3 origins、hosting egress/DNS/TLSを確認。
   secret query/credentialは記録しない。policyをcode reviewで固定し、専用runtime/隔離DB/Vaultを準備。
3. 通信許可範囲・利用account・停止条件・egress firewall・ログ/APM redactionを具体的にreview。
   synthetic gateとWindows未検証gateを記録し、実接続の個別承認を得る。
4. 明示的にreviewされた専用backend選択経路でMoodleだけ、WebClassだけ、両sourceを順に検証。
   TLS/SNI/hostname/allowlist/cookie isolationを失敗時に緩和しない。固定AuthStep/codeで調査する。
5. 再同期、session reuse、credential更新後の破棄、AuthExpired一回、partial commit、completion、
   identity/duplicate、shouldReloadを確認。破壊的な障害注入は実大学ではせずsyntheticを使う。
6. payloadのbyte数、course/group/request数、所要時間、Cookie数/bytes、固定categoryだけを測定。
   token URL、ID/password、Cookie値、SAML、response bodyは保存/表示しない。制限変更は測定根拠を添えて別review。
7. admission停止→drain→runtime破棄。隔離credential/DBの保持・削除方針を確認して終了し、結果を別phaseへ記録。
   Web-4B3成功もproduction deploymentやLAN/public bindを自動承認しない。

**Ready for Web-4B3 — コード統合とMac/offline検証の完了だけを意味する。実通信の自動許可ではない。**
