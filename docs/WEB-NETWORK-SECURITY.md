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
