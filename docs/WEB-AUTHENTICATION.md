# Web-3B authentication / session / reauthentication design

対象は単一account・synthetic data・HTTP 127.0.0.1:3000のlocal PoC。
Web専用の暗号化credential保存を含む。合成credentialだけを使用し、大学通信・公開運用は含まない。

## Authenticationとsessionの分離

```text
local terminal bootstrap ─→ Web control DB v2
                                 ↑
Browser login → LocalPasswordProvider.authenticate → internal UserId
                    ↓ credential verification only        ↓
              Argon2id / blocking pool          AuthService session issuance
                                                         ↓
Browser Cookie → AuthService.resolve → request extension AuthenticatedSession
                                                ↓
                                    TenantRuntime.for_user(UserId)
                                                ↓ exact owner equality
                                  one ApplicationHandle → synthetic SQLite v3
```

`auth.rs`のLocalPasswordProviderはlogin/passwordを検証してUserIdを返すだけで、
Cookieを発行せず課題DBを選ばない。AuthServiceはその結果をsessionへ束縛し、
resolve/logout/expiryを管理する。将来のOIDC providerは検証済みsubjectをinternal UserIdへ
解決してこのsession境界へ接続する。今回外部providerやOIDC flowは実装していない。

RustCrypto [Argon2 API](https://docs.rs/argon2/0.5.3/argon2/)を使用する。
Argon2id v19、m=19456 KiB、t=2、p=1、32-byte output、OS乱数16-byte salt。
algorithm/version/parameters/salt/hashをPHC stringへ保存し、検証時にも固定policyを確認する。
これは[OWASP password storage guidance](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)
の19 MiB / 2 iterations / 1 laneを採用した値。独自hashや暗号は作っていない。
unknown accountも同parameterのdummy verifierで検証し、disabled/wrong passwordと同じ401を返す。
malformed inputは400で、内部cause/credentialをresponseやlogへ含めない。

hash/verifyとcontrol SQLite操作はspawn_blockingへ移し、async executorでは実行しない。
同時loginはSemaphore 1 permitで制限し、cancel後もblocking job終了までpermitを保持する。
control DBアクセスは1つのMutexで直列化する。login hash中のsession操作は短時間待つ。
多数利用者向けpool/registry最適化は今回の対象外。

## Bootstrapと保存

`bootstrap-admin`だけがaccountを作る。IDをterminalで入力し、[rpassword](https://docs.rs/rpassword/7.5.4/rpassword/)
でpasswordと確認入力を非表示取得する。CLI引数/env password/HTTP signupはない。
新しいUserIdは独立した256-bit OS乱数で、大学ID・email・Moodle scopeを使用しない。
Web passwordは15–256 bytes、制御文字なし。inputをtrim/正規化してpasswordを変更しない。
IDはASCII英数字と`._-`、1–64 bytes。

`web-control.db`の独立user_version=2:

| Table | Stored values |
| --- | --- |
| accounts | singleton=1のCHECK、unique user_id/login、PHC verifier、enabled/disabled |
| sessions | token_hash BLOB PK、FK user_id、CSRF random token、created/last_seen/absolute_expiry/reauthenticated_at |
| login_attempts | login/reauth共通、rolling 60秒のattempt timestamp (最大5件) |
| vault_binding / web_credentials / vault_nonces | key確認用ciphertext / ownerと種別別ciphertext / keyごとの使用nonce履歴 |

課題schema v3へtable/columnを追加しない。singleton constraintで追加accountを拒否する。
bootstrapはtransaction内で既存account確認→hash→insert→commit。失敗でaccount行を残さず、
既存行を更新/削除しない。新規control schemaもtransactionで公開する。
未作成はbootstrap_required、future version/不明な非空v0/破損は固定errorで停止する。

固定のPoC marker付きdirectoryを使用し、control DB/lockもsymlink/reparse pointを拒否する。
serverとbootstrapは同じOS exclusive file lockを保持する。2つ目のprocessは最初のserverの
sessionを消せない。lock fileの存在ではなくOS lockを検証し、終了/crashでOSが解放する。
ローカルservice identity/ACLのhardening、backup/restore運用は公開前の別作業。
ローカルOS accountやDBを書換えられる攻撃者から守るvaultではない。

password bufferはRust側でZeroizing/Zeroizeを使う。JSON/Browser/HTTP内部の全copyの
完全消去は保証しない。password plaintextやraw session tokenをdiskへ保存しない。
clientが生成するrequest bodyは通常の認証処理に必要な一時データだけとする。

## Session policy

256-bit OS CSPRNG tokenを64文字lowercase hexでCookieへ入れ、serverはSHA-256 hashだけを保存する。
CSRFは別の256-bit乱数で、session行と一緒に保持。Cookie tokenと違いCSRF単独では認証できない。
256-bit tokenのhashはfast hashでよく、passwordのArgon2とは用途が異なる。
[OWASP session guidance](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html)
に沿い、serverが期限・失効・所有者を検証する。

| Event | Behavior |
| --- | --- |
| login成功 | 新しいtoken/CSRFを発行、提示された旧sessionを同一transactionで削除 |
| idle | 最後の認証済みAPIから30分、境界時刻以上は401 |
| absolute | loginから12時間、活動で延長しない |
| clock巻戻り | now < last_seenを拒否して失効 |
| reload | Cookieを使ってsession APIを再取得、CSRFをmemoryへ復元 |
| logout | server行を削除しCookieをMax-Age=0へ。旧token/CSRFの再利用不可 |
| account disabled | loginは他失敗と同じ401。既存sessionの利用は毎回403 |
| server restart/crash後のopen | persisted session行をすべて削除。account/課題/completionは保持 |
| 多数login | expired行のcleanupと最新16 sessionまでのrecord上限 |

session responseはauthenticated/userId/csrfToken/expiresAt/absoluteExpiresAtだけ。
時刻はUnix seconds、expiresAtはidleとabsoluteの小さい方。
session token/PHC verifier/server secretはJSONへ出さない。
一度認証を通ってapplicationにadmit済みの処理はlogoutと並行して完了し得る。
logoutは過去にadmitされたSQLite writeをrollbackする操作ではない。

HTTP開発Cookieは`am_loopback_session; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200`。
Domainなし、Secureなし。runtimeは固定IPv4 loopbackだけにbindし、公開origin/proxy設定を持たない。
将来のHTTPS production policyは別構造で`__Host-`、Secure、HttpOnly、Path=/、Domainなしを必須にする。
現行binaryにdevelopment Cookieをpublicへ持ち込めるproduction flagはない。
同じhostの他local process/portは信頼境界外であり、このHTTP例外は公開HTTPSの代わりにならない。

## Authorization / TenantRuntime

起動時にcontrol DBのsingleton ownerを読み、固定synthetic DBを開く1つのTenantRuntimeへ束縛する。
stateはimmutableなowner-to-handle対応とauth/clock/admission serviceを共有する。
「現在ログイン中のuser」をglobal stateで書き換えない。

request extensionの検証済みUserIdをGET/PUTが受け取り、`for_user`でowner一致を確認してから
ApplicationHandleを取得する。不一致は403。requestのheader/query/bodyからUserId、DB path、
tenant path、credential ownerを選ばせない。assignment IDはownerのDB内でのみ検索する。
startup readiness queryはtrusted ownerを使用する。ApplicationHandleはHTTP requestごとに作らず、
local/settings/syncの3 workerを1組再利用する。実WebClass sessionを共有する経路はない。

## CSRF / input / frontend

[OWASP CSRF guidance](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)
を参照し、synchronizer tokenをsessionへ束縛する。
mutationはexact Host/Origin、JSON、X-Assignment-Manager: web-3a、Fetch Metadata検証を通し、
logout/reauth/Settings PUT/DELETE/completion PUTにはさらにX-CSRF-Tokenをconstant-time比較する。欠落/重複/不一致は403。
login CSRFはsession発行前なのでOrigin/JSON/custom header/Metadataで防御する。
CORS/preflight許可を追加せず、cross-origin redirectはadapterで拒否する。
Fetch Metadataを送らないCLIもOrigin/custom header/JSONを省略できない。
GETでは直接入力navigationのSite=noneを許可し、同一site別originとcross-siteは拒否する。

Settings body 4096 bytes、他のmutation body 1024 bytes、query 2048 bytes。serdeはunknown/duplicate fieldを拒否。
malformed inputは400、wrong Content-Typeは415、oversized bodyは413。
login/reauthは共通で全ID合計5回/60秒、成功も数え、超過/並行hashは429 + Retry-After: 60。
IP headerやclient指定IDをrate-limit keyにしない。attempt timestampはrestartでも保持する。
ローカル攻撃者による一時的なlogin妨害を防ぐ仕組みではない。

全応答にno-storeと既存CSP/nosniff/no-referrer/frame拒否/CORPを維持。
静的assetはcompile-time allowlist。DB/source/desktop modulesを配信しない。
課題titleはtextContent、passwordは送信後すぐ入力clear、storageへ保持しない。
logout/401/一般403はview/modelをdisposeし、rows/options/countsと保持snapshotを消去する。
adapter/model generationで古いGET/PUTの復活とlogout後のreconciliationを止める。
通信失敗したlogoutは安全に隠したまま再試行を案内する。

UIは次のAPI/reloadで失効を知る。background keepaliveやtab間logout通知は今回ない。
他tabの既表示snapshotは次のrequestまで残り得る。一般公開・multi-user承認の根拠にはしない。
WebVault + DisabledNetworkBackendを使用し、startup syncはOFF。Web-4A手動Sync APIは認証とowner確認を必須にし、大学への実通信は追加していない。

## Recent reauthentication (Web-3B)

POST `/api/v1/auth/reauth`はWeb専用passwordだけを受け、既存LocalPasswordProviderの
Argon2id検証、global 5試行/60秒とSemaphore 1 permitをloginと共有する。
University passwordはこの本人確認に使用しない。成功は204で、該当session行だけの
`reauthenticated_at`をserver時刻へ更新する。loginだけではこの値を設定しない。

Settings mutationはsession resolve → TenantRuntime owner一致 → CSRFと既存transport検証 →
DB上の現在session/owner/expiry/recent reauth再確認 → shared ApplicationHandleへのenqueueで実行する。
Host/Origin/Content-Type/Fetch Metadataの検証は従来同様middleware入口で先に行い、
credential accessはすべての検証後になる。GET statusでも認証/owner確認を省略しない。

`0 <= now - reauthenticated_at < 300`だけを許可する。境界300秒・未来時刻・未確認は
403 `reauth_required`。誤passwordは403 `reauth_failed`、limitは429。これらreauth固有403は
Browser sessionを捨てず、本人確認のやり直しを案内する。その他の403は従来どおりsigned outにする。
成功時刻を他sessionへコピーせず、logout/rotation/expiry/restartのsession削除で一緒に失効する。
reauth時も生きているDB sessionを再確認し、logout後に古いsnapshotで本人確認を付与できない。
既にadmitされたwriteが並行logout後に完了する既存contractは維持する。

Browserは本人確認送信開始時に入力を即時clearする。viewに保存するのは安全なboolean/messageだけ。
画面切替、tab非表示、pagehide、logoutでもすべてのsecret入力をclearする。
login/reauth/settings JSON入力はnon-Debug/non-Serializeのzeroizing wrapperで受け取る。
HTTP/JSON/Browser/OS内の全copyが完全消去される保証ではない。

control v1→v2はtransactionでsession columnとVault tablesを追加し、account/verifier、既存session行、
rate-limit timestampを保持する。server openではその後、従来のrestart policyでsession行だけを削除する。
Future/partial/corrupt schemaは停止し、migration failureで旧DBをresetしない。
鍵custody、暗号化format、nonce履歴、backup制約は[Vault設計](WEB-CREDENTIAL-VAULT.md)を参照。


## Manual sync authentication (Web-4A)

POST /api/v1/sync、GET /api/v1/sync/status、GET /api/v1/sync/runs/{runId} は
既存Cookie resolve（disabled/logout/expiryを含む）とTenantRuntime owner照合を必須とする。
POSTはさらにsession-bound CSRF、Origin/Host/Fetch Metadata/JSON/web-3a headerを検証。
通常manual syncにrecent reauthは要求しない。owner単位のrun admissionと5新規run/60秒、
bounded memory key/historyを追加した。詳細は[Phase Web-4A](PHASE-WEB-4A.md)。
既に認証・admission済みjobはlogoutと並行して完了し得る。run IDは認可の代わりにならず、
旧sessionや別ownerでは照会できない。通常binaryの大学通信は無効のまま。


## University transport preparation (Web-4B1)

[Web-4B1](PHASE-WEB-4B1.md)でcoreに[安全な大学HTTP constructor](WEB-NETWORK-SECURITY.md)を追加した。
通常Web serverは引き続きDisabledNetworkBackendで、実大学backendへは接続していない。
Moodleの保存時validationは通信許可ではなく、Web fetchには別のexact host/pathとDNS policyが必要。
Moodle実host/pathは未設定でfail closed。暗号化Vault、boolean-only status、owner/reauth/CSRF、
jobとcredential mutationの排他は変更していない。実credentialの使用は今回行っていない。
Web-4AのWindows検証は未完了・最終BLOCKEDのまま。Web-4B2はまだ実装していない。

## Web-4B3 synthetic secured HTTP verification

[Preflight](PHASE-WEB-4B3-PREFLIGHT.md)で、実loopback HTTPの認証/CSRF/recent reauth/Vaultから
secured synthetic HTTPS、既存parser/import、SQLite、Browserまでを接続して検証した。
認証/owner/coordinator/admissionの本体と通常binaryのDisabled選択は変更していない。
固定fixtureのCA/DNSは専用offline検証featureだけで、任意値をrequest/env/browserから渡せない。
all-featuresでも通常binaryからfixture/real backendは選択できない。
現時点の実接続gateはBLOCKED、実大学HTTP/DNSと実credentialは使用していない。
