# Phase Web-0: Web Migration Architecture Review

調査日: 2026-10-21（Asia/Tokyo）

Repository: [toppoun/assignment-manager](https://github.com/toppoun/assignment-manager)

調査branch: feature/web-migration

調査HEAD / 指定base: a9fa429cc6483d47a342aaf18511cb15850242b5（Phase 10C2）

本書は設計レビューである。今回の変更は本ファイルだけとし、Rust / frontend / DB schema / manifest / lockfileは変更しない。大学への通信、実credentialへのアクセス・移行、deployment、Web-1実装は行っていない。ローカルcheckoutのbranchとHEADが指定値に一致し、調査開始時にworking treeがcleanであることを確認した。

「確認済み」はこのHEADのソース・テストを読んだ結果を指す。「提案」は今後の実装、「要検証」は未実証の事項である。既存のPhase報告書にあるPASSは過去の実行記録であり、本レビューで再実行した結果ではない。参照リンクは本書からのrepository相対path、行番号は調査HEADの値である。

## 1. Executive conclusion

**判定: Ready for Web-1。** synthetic DB、loopback限定、大学通信なしのRust HTTP API PoCを始めるための構造上のblockerはない。既存のroot coreを移動せず、独立した crates/web-server を追加し、Web transportからshared applicationを利用する。

公開MVPの推奨構成は **同一originのBrowser UI + Rust/Axum API + ユーザー単位のApplicationHandle + ユーザー専用SQLite v3**。最初は1サーバープロセス・1許可ユーザーとし、Webログイン、authorization、暗号化credential store、HTTPSを公開前に必須とする。単一ユーザーは「全requestを無条件に同一利用者として扱う」という意味ではない。

主な判断と根拠:

| 判断 | 実装上の根拠 / 帰結 |
| --- | --- |
| coreの大半を維持できる | [domain](../src/domain.rs)のDeadline / CompletionState、[import](../src/import.rs)のAssignmentCandidateBatch / apply_source_update、[query](../src/query.rs)のQueryTimeContext / filter_sort_assignment_listはUI非依存。 |
| globalな単一handleを全ユーザーへ渡してはいけない | [runtime.rs](../crates/application/src/runtime.rs)のRuntimeはlocal/settings/sync worker、startup_checkedを1組所有し、[SyncService](../crates/application/src/sync_service.rs)は1つのstoreとsessionを保持する。利用者IDを受け取る契約がない。 |
| SQLite維持が最小変更 | [persistence.rs](../src/persistence.rs) L24のschema v3とSqliteStoreをそのままユーザー専用DBとして使える。共有DBへのuser_id追加は初期MVPには不要。 |
| Web専用の小さなapplication拡張が必要 | local_worker.rs L61のLocal::now依存と、runtime.rs L130のtoggleだけの公開APIを、明示clock・明示completion設定の追加APIで補う。既存APIは保持する。 |
| frontend framework導入は当面不要 | [frontend](../frontend/)は9ファイル、963行。presentation / settings / syncモデルは関数で分離され、invoke注入をすでに使う。HTTP adapterを追加する方が変更が小さい。 |
| 大学SSOのhosting適合性は未確認 | [HoseiAuthenticator::login](../src/webclass_auth.rs)は観測済みOpenAM/SAML手順を実装するが、MFA等未知callbackを拒否する。mock成功とhosting環境での実login成功は別である。 |

認証・データ分離・SSRF・鍵管理・restore検証が完了するまで、実データを扱うインターネット公開は不可。Web-1の開始可否と、公開運用可否を分ける。学期フィルターはproduct目標に残すが、現在未実装であり今回およびWeb-1の範囲に含めない。

## 2. Current architecture

### 2.1 依存と所有権

```text
Windows Tauri commands ──────────────┐
                                    ↓
SwiftUI → Swift actor → UniFFI → ApplicationHandle → Rust core
                                    ├ local worker → SQLite connection A
                                    ├ sync worker  → SQLite connection B / HTTP / cookie jar
                                    └ settings worker → SettingsStore / CredentialStore
```

root Cargo.tomlはworkspaceではなくcore packageであり、application、src-tauri、macos-bridgeは独立manifest/lockfileでpath依存する。[README](../README.md)、[application README](../crates/application/README.md)、[bridge lib.rs](../crates/macos-bridge/src/lib.rs)のMacApplicationを確認した。Web追加のためにroot crate全体の移動やworkspace化を前提にする必要はない。

### 2.2 読み取った主要contract

| 領域 / 実ソース | 確認した型・関数・制約 |
| --- | --- |
| [domain.rs](../src/domain.rs) L24–72 | DeadlineはDateOnly(NaiveDate)とDateTime(Utc)。CompletionStateは利用者所有。AssignmentId / CourseIdは外部IDとは別。course_id、title、deadline、submission、submission_urlがある。tenantや学期のfieldはない。 |
| [persistence.rs](../src/persistence.rs) L24–144、L230以降 | 6 tables: courses / assignments / assignment_sources / course_sources / course_schedule / sync_scopes。v3のsource keyは(origin, scope, external_id)。user_idがなく、DB全体がsingle-profile。ConnectionはSqliteStoreのprivate field。 |
| [import.rs](../src/import.rs) AssignmentCandidateBatch::try_new / apply_source_update | 空identityと重複を拒否。source更新はtitle/deadlineを置換するが、internal IDとcompletionを維持する。未取得course / URLは既存値を維持。opaque identityのtrim・正規化・名前によるmergeをしない。 |
| [observation.rs](../src/observation.rs) AssignmentObservation::try_new / validate_candidates | parser後・分類前のpositive observationを検証する。candidate不在・空batchは削除や消失の証拠にならない。SyncScopeは現行single-profileの処理scopeでありtenant境界ではない。 |
| [query.rs](../src/query.rs) load_assignment_list / filter_sort_assignment_list | 一度のSELECTでCourse付きsnapshotを取得。filter/sortはmemory上、時計は引数。CourseはIDで区別し、同名でも統合しない。sortは昇順だけ、最後にAssignmentIdでtie break。 |
| [application query.rs](../crates/application/src/query.rs) AssignmentQuery / QueryClock / response_from_snapshot | defaultはIncomplete、全Course、全deadline、締切順。1snapshot・1clockからitems、filter前total_count、全参照Course候補、deadline badgeを作る。list_from_storeはpub(crate)なのでWeb crateから直接呼べない。 |
| [actions.rs](../src/actions.rs) toggle_assignment_completion、persistence.rs L629–689 | toggleはIMMEDIATE transaction内でread→反転→commit。set_completionはすでにpublicで、同じ状態ならupdated_atも変えない。applicationの公開口は現在toggleのみ。 |
| [moodle.rs](../src/moodle.rs) parse_calendar / classify_event / map_event / single_course_category | ICSをoffline parse。現実装のdatetimeはUTC Z形式に限定し、DateOnly/TZID入力の汎用対応を意味しない。UIDは課題そのもののIDではなくevent identity。CourseRelatedかつ単一の有効CATEGORIESのみCourse候補にする。 |
| [moodle_http.rs](../src/moodle_http.rs) MoodleFeed::new / MoodleClient::fetch_calendar | HTTPS・host存在・userinfo禁止、scopeとexport kind suffixの一致を検証。redirect禁止・自動Refererなし・15秒timeout。ただし任意HTTPS host/IP/portを拒否せず、response.bytes()はbodyサイズ制限を持たない。 |
| [webclass.rs](../src/webclass.rs) parse_courses / parse_contents / map_contents_for_group | 全recordのgroup一致を検証。Questionかつhidden_content=falseのみ課題化。end_dateをJSTとしてUTCに変換。scores/answer_datetimeはcompletionに反映しない。semester等はsource modelにあるが学期query/storage contractは未実装。 |
| [webclass_http.rs](../src/webclass_http.rs) WebClassClient / fetch_json | production originはhttps://lms2025.hosei.ac.jp固定。group_idはquery parameter encoding。確認済みlogin redirect / HTML patternだけをAuthExpiredとする。全401等を同じ意味と仮定しない。 |
| [webclass_auth.rs](../src/webclass_auth.rs) HoseiAuthenticator::login / AuthSession / resolve_destination | HOPPII→IdP/OpenAM→SAML/consent→HOPPII session→WebClass SSO→courses配列検証。新規Jar、redirect/retry自動実行なし、手動redirectはorigin検証・最大10回。JSは実行せず限定patternをparseする。 |
| [sync.rs](../src/sync.rs) sync_moodle_once / sync_webclass_all_once / sync_webclass_all_with_reauth | network中にDB transactionを保持しない。Course作成、group単位import等は別commit。AuthExpired時だけfresh credential→login→full retry once。以前のcommitは維持し、成功件数は最終成功attemptだけ。 |
| [models.rs](../crates/application/src/models.rs)、[error.rs](../crates/application/src/error.rs) | application read modelはraw submission URL、source identity、secretを返さずhas_submission_urlのみ。ApplicationErrorは固定codeでcauseを保持しない。これらはserde JSON DTOではない。 |
| [Tauri commands.rs](../src-tauri/src/commands.rs) / [dto.rs](../src-tauri/src/dto.rs) | command→application変換→JSON DTO。date_only / date_time / noneを区別、camelCase field、WebClassのtagはweb_class。HTTP serverがTauri crateに依存する理由はない。 |
| [Tauri storage.rs](../src-tauri/src/storage.rs)、[settings.rs](../src-tauri/src/settings.rs) | app_data_dirの固定filename。SettingsFileはversion / identity / unknown fieldsを厳格検証し同directoryのtemporary fileをatomicに置換。破損fileをsilent resetしない。 |
| [credential_store.rs](../src-tauri/src/credential_store.rs) OsCredentialStore / CredentialVault | Windows Credential Manager、macOS向けRust Keychain provider。固定accountはmoodle-feed-v1 / webclass-credentials-v1。ID/passwordは単一recordで置換し、失敗時に旧recordを維持する。Linuxではこのadapterは利用不可。 |
| [frontend/app.js](../frontend/app.js) / [settings_view.js](../frontend/settings_view.js) | window.__TAURI__.core.invokeへの直接依存はこの2箇所。DOMはtextContentで構築。toggle成功後query失敗はneedsRefreshで別扱いだが、HTTPのresponse喪失時まで「未変更」を断定できる実装ではない。 |
| [RustApplicationClient.swift](../macos/AssignmentManager/Sources/AssignmentManagerKit/Bridge/RustApplicationClient.swift) | application-lifetime actorがMacApplication proxyを1つ所有。openは単一Taskで共有。NativeStorageの正式DBはApplication Support/org.assignmentmanager.native/assignment-manager.db。 |
| [NativeKeychain.swift](../macos/AssignmentManager/Sources/AssignmentManagerKit/Bridge/NativeKeychain.swift) / [NativeSettingsStorage.swift](../macos/AssignmentManager/Sources/AssignmentManagerKit/Bridge/NativeSettingsStorage.swift) | 正式SwiftUI shellはSecurity.frameworkによるSwift foreign portを使用。Keychain serviceはorg.assignmentmanager.native.credentials.v1、非secret設定はUserDefaults。Windowsや旧macOS Tauriと同一保存場所ではない。 |
| [BridgeSmoke](../macos/BridgeSmoke/Sources/BridgeSmokeSupport/SmokeChecks.swift) / [PHASE-10C2.md](../macos/AssignmentManager/PHASE-10C2.md) | synthetic DB / memory storesでFFI、lifetime、settings、syncを検証する独立regression資産。大学実接続の確認済み保証ではない。READMEの古いPhase節は最新10C2節・実ソースと区別して読んだ。 |

### 2.3 検証証拠の範囲

| 証拠 | 何が言えるか / 言えないか |
| --- | --- |
| 現在の実ソースとsynthetic tests | HTTP/auth/DB/workerのcontractと、mockで確認する失敗・再試行・秘匿境界を特定できる。今回はtestの再実行はしていない。 |
| READMEのWindows Phase 8D2 manual smoke記録（2026-10-04） | Moodle 15件 + WebClass 146件の取得、再同期、completion/restart保持の過去実績が記載されている。この実績はdesktop環境のもので、今回再検証した事実や外部hostingからのSSO保証ではない。 |
| macOS Phase 10C2報告 | native shell→shared syncへの接続とsynthetic regressionの過去PASSを記録。大学server/native windowのmanual項目は利用者確認待ちと明記されている。 |
| [bridge manifest](../crates/macos-bridge/Cargo.toml)と[Swift app実装](../macos/AssignmentManager/App/AssignmentManagerApp.swift) | UniFFIは=0.32.2固定。ApplicationDelegateがclient/modelを所有し、Quitでasync shutdownを待つ構造が実装されている。Web HTTPへUniFFIを経由させる必要はない。 |
| 本レビューの対象外 | hosting IPからの大学到達・認証、MFA/規約、phone実機PWA、production security/restoreの実証は含まれない。 |

## 3. Code reuse assessment

A=変更なしで再利用、B=小規模変更で再利用、C=Web adapter追加、D=大きな再設計、E=Web実行には不要だが保持。以下は今後必要な作業の分類であり、今回の変更ではない。

| 対象 | 分類 | 判断と具体的な境界 |
| --- | --- | --- |
| domain | A | Assignment / Course / Deadline / CompletionStateを維持。tenantは当面DB所有権で分離する。学期・submission state追加を混ぜない。 |
| SQLite persistence | A（MVP）、C/D（将来PostgreSQL） | SqliteStore / schema v3をユーザー専用fileで使用。Web metadataは別store。PostgreSQLはconcrete SqliteStore依存があるため接続文字列変更だけでは移行不可。 |
| import / observation | A | validated batch、positive observation、field ownership、source identityを維持。tenant DBを選ぶ責任はcallerに置く。 |
| query / filter / sort | A + B | core規則はそのまま。applicationへserver生成QueryClockを渡す追加口を作る。公開APIから任意のnowを受け取らない。 |
| completion | B | coreのset_completionをlocal worker / ApplicationHandleへ追加公開する。desktop toggleはそのまま残す。 |
| Moodle parser | A | parse_calendar / map_events再利用。未対応のICS形式をWeb化ついでに拡張しない。 |
| Moodle HTTP | B + C | server egress policy、厳密なhost/path/port policy、DNS接続制御・body上限を追加。MoodleFeed::newだけではSSRF防御にならない。 |
| WebClass parser | A | parse_courses / parse_contents / map_contents_for_groupのidentity・visibility・JST契約を維持。 |
| WebClass HTTP | B + C | 固定origin / encoding / AuthExpired識別を維持し、server向けbody上限・egress制御を追加。blocking clientは専用worker内に保持。 |
| OpenAM / SAML authentication | B + C、動作は要検証 | observed callback / form / origin検証を維持。外部hosting、MFA、WAF、年度host変更を実験で確認。未対応callbackを無条件承認しない。 |
| shared application | C + B（MVP）、D（大規模multi-user時） | Web SettingsStore / CredentialStore / tenant runtime管理を追加。clock/set APIを加算。大量userのworker pool・DB repository化は別段階。 |
| Tauri frontend | B + C | presentation / settings / syncの関数を再利用、transportを注入。HTTPでの不確実なwrite結果とsync job pollingは新規対応が必要。Tauri用adapterを保持。 |
| SwiftUI | E | macOS正式shell、ViewModels、testsを維持。Web UIとは別consumer。Web版との双方向データ共有は現行native DBから自動では成立しない。 |
| UniFFI | E | HTTP pathには不要。MacApplication / DTO / bindgen / BridgeSmokeを保持し、既存ABIを不用意に変更しない。 |
| Keychain / Windows Credential Manager | E + C | desktop credential backendを保持。Webにはユーザー束縛された暗号化vault adapterを新設する。OS storeの内容をWebへ自動exportしない。 |

## 4. Recommended Web architecture

### 4.1 公開MVPの境界

```text
Browser / future PWA（課題の表示、入力中secretだけ）
  ↓ HTTPS、同一origin、opaque session cookie / CSRF token
TLS reverse proxy（固定upstream、size/rate制限）
  ↓ private / loopback
crates/web-server（Axum候補）
  ├ authentication middleware → AuthenticatedUser { internal_user_id }
  ├ authorization / transport DTO / fixed error / WebSyncCoordinator
  ├ TenantRuntimeRegistry → TenantRuntime(user U)
  │                         └ ApplicationHandle(U)
  │                           ├ local worker → users/U/assignments.db（v3）
  │                           ├ settings worker → WebSettingsStore(U) / EncryptedCredentialStore(U)
  │                           └ sync worker → core HTTP/auth → approved university origins
  └ separate web control store（account/session/settings/ciphertext/job metadata）
      + server-only key provider（DB/backupから分離）
```

global AppStateに置くのはregistry、認証session store、policy、clock provider等のservice。認証済み利用者はrequest extension/extractorで解決し、global stateをrequestごとに書き換えない。[Axum Router::with_state](https://docs.rs/axum/latest/axum/struct.Router.html)もrequest由来authorizationをglobal stateに入れる用途ではないと説明する。これはAxum採用の適合性を示す資料であり、導入versionはWeb-1時点で選定・lockする。

大学への通信はbackendだけが行う。Browser→大学APIへのfetch、Moodle token URLをURL queryへ埋めたAPI、大学CookieをBrowserへ渡すAPIは設計しない。frontendとAPIを同じoriginで配信し、CORSのcredential付きcross-origin運用を初期構成に持ち込まない。

Web自身の認証IDはopaqueなinternal UserIdとし、大学ID、email、Moodle scopeをtenant keyに流用しない。PCとスマホは同じWebアカウント・server DBを利用する。現行Windows/macOS native appはlocal DBのconsumerであり、そのままでWebとのcompletion共有が始まるわけではない。native API接続は別案件とする。

### 4.2 Date / timezone

確認済み: [local_worker.rs](../crates/application/src/local_worker.rs)はList処理時にLocal::now()を1回sampleする。[QueryClock::from_local](../crates/application/src/query.rs)はgeneric TimeZoneを受け取れるが、ApplicationHandle経由でそのclockを指定する口はない。core [QueryTimeContext](../src/query.rs)自体はclock非依存で、utc_offsetは固定offsetでありIANA timezoneではない。

提案:

1. 公開MVPのcalendar timezoneをアカウント単位のAsia/Tokyoに固定し、画面に表示する。server OSのTZやBrowserのoffsetへ暗黙依存しない。WebClass sourceのJST解釈はこの表示設定と別のsource contractとして維持する。
2. serverがUtc::now()を一度sampleし、+09:00へ投影してQueryClock::from_localを呼ぶ。新規のApplicationHandle::query_assignments_with_clock(query, clock)を追加し、その値をlocal workerへ渡す。既存query_assignmentsは従来のLocal::now動作を保持する。Web-1からこの小さなseamを入れる。
3. Browserはnow / local_date / utc_offset / today範囲を指定できない。将来のtimezone選択は検証済みIANA名のuser preferenceとして保存し、serverのtimezone DBからnowと実際の当日境界を構成する。timezoneは表示条件でありauthorizationに使わない。
4. responseへevaluatedAt（UTC）とtimeZoneを付け、frontendのIntl.DateTimeFormatにも同じtimeZoneを指定する。現在のpresentation.jsはBrowser default timezoneを使うので変更対象。画面表示とbadge判定を一致させる。

| 仕様 | 維持する意味 |
| --- | --- |
| DateOnly | calendar dateのまま。UTC midnight化しない。 |
| DateTime | UTC instantを保存・JSON返却し、選択timezoneで表示。 |
| Overdue | Incompleteのみ。DateOnly < local_date、DateTime < now。締切ちょうどは期限切れでない。 |
| Today | DateOnlyは当日、DateTimeは当日開始〜翌日開始の直前。過去の当日instantもToday filterに入る。badge優先順位はOverdue→Today→Upcoming。 |
| WithinSevenDays | DateTimeはnow〜168時間後、DateOnlyは今日〜7日後、両端inclusive。全8 calendar datesを含み得る。 |
| deadline sort | 現行固定offsetで同じcalendar dayを比較し、同日にDateTime→DateOnly、期限なし最後、同順位はID順。 |

将来DST timezoneを導入する際、Todayの実midnightはQueryClockのtimezoneから求める一方、混在deadline sortは現行「現在の固定offset」の仕様である点を明文化する。全deadline日付ごとのDST offsetへ変えるなら仕様変更として別にtestする。午夜が存在しないtimezoneではfrom_localが失敗し得るため、IANA対応を無検証で一般化しない。日付変更・復帰時にはserverへ再queryし、offline snapshotのbadgeは評価時刻付きで扱う。

### 4.3 Completion API

| 選択 | 評価 |
| --- | --- |
| HTTPで既存toggleをそのまま公開 | response喪失後のretryで二重反転する。desktop向けAPIは維持するがWeb public endpointには採用しない。 |
| 明示状態のPUT | 推奨。PUT /api/v1/assignments/{id}/completion、bodyはcompletion=incomplete/completed。coreのset_completionを利用し、同じ要求のretryは同じ状態になる。 |
| toggle + Idempotency-Key | transactionと同じ耐久境界にuser/key/request hash/resultを保存すれば可能だが、MVPには過大。memoryだけのdedupではrestart後の二重反転を防げない。 |

applicationへset_completion(id, state)とlocal worker requestを加算する。core set_completionはResult<()>なので、成功した指定stateをcommit後のackとして返す。read→toggleでsetを模倣しない。HTTP timeout/abort時は「未変更」と断定せず結果不明として再queryする。Browserに最後の希望状態を保ち、必要なら同じPUTをretryする。syncはAssignmentCandidate::apply_source_updateのcompletion保持を必ず通す。

同時に2端末が逆状態を指定する場合、MVPはserver commit順のlast write wins。冪等PUTは古い要求と他の変更との競合を全て解決するものではない。UIは同一課題へのpending writeを直列化し、古いrequestの自動retryを新しい操作の後に行わない。厳密なlost update防止が必要になればdurable completion version + If-Match / 412を別段階で追加し、source syncのupdated_atをそのままcompletion revisionに流用しない。

## 5. ApplicationHandle / multi-user concerns

### 5.1 現在のlifetimeとWebでの影響

| 項目 | 確認済み実装 | Webへの適用 |
| --- | --- | --- |
| worker lifetime | runtime.rsの`Arc<Runtime>`をcloneで共有。openごとに3 OS threadsを作る。 | request / tab / loginごとにopenしない。user runtimeをapplicationより長いserver管理単位にする。 |
| local worker | local_worker.rsのsync_channel(32)、private SqliteStoreを順次処理、try_send。 | 並列requestは安全に直列化されるが無限受付しない。queue fullはHTTP 503 + Retry-After。 |
| sync worker | sync_worker.rsのchannel容量1、status mutexでadmission、追加runはAlreadyRunning。 | channel容量1は「次runを1件予約」ではない。running中の追加runをqueueしない。 |
| settings worker | settings_worker.rsのsync_channel(8)、initializerとport操作を同threadで実行。 | 暗号化storeもこのportへ束縛。queueが満杯ならsyncのsecret取得も失敗し得るためWeb側rate limitが必要。 |
| SQLite ownership | localとsyncが同じpathへの別Connectionを所有。handleからConnectionを返さない。 | async handlerで新規Connection作成やblocking HTTPを直接行わない。DB初期化失敗はopenの戻り値だけでは検出できない。 |
| single-flight | 同一SyncWorkerのstatusを共有するhandle clones間だけ。reply喪失でもFlightGuardはrun完了まで維持。 | 同じuserの別handle、別process、別serverは保護しない。 |
| startup once | runtime.rs L166のAtomicBool。OFF / 未設定 / status失敗でもそのruntimeでは消費。 | server全体の起動やBrowser page lifetimeの意味とは一致しない。Webの自動同期には直接使用しない。 |
| WebClass session | SyncService.session: Option<(u64, Session)>。CoreSyncBackendではWebClassClient / in-memory Jar。 | user runtime外へ共有しない。server restart後は次syncで新規login。 |
| credential revision | SettingsService.webclass_revisionはRAMのu64、成功save/deleteだけwrapping_add。次runでrevision不一致ならsessionをdrop。 | 同じSettingsService経由の変更に限る。外部vault更新・別processの変更は検知できない。 |
| shutdown | begin_shutdownがadmissionを閉じ、accepted jobをdrain。syncはsettings senderを保持しreauth可能。shutdown().awaitはresource drop完了を待つ。 | Browser切断でhandleをshutdownしない。server停止/tenant削除時に管理者がdrainする。 |

根拠: [runtime.rs](../crates/application/src/runtime.rs)、[local_worker.rs](../crates/application/src/local_worker.rs)、[sync_worker.rs](../crates/application/src/sync_worker.rs)、[settings_worker.rs](../crates/application/src/settings_worker.rs)、[lifecycle.rs](../crates/application/src/lifecycle.rs)、[sync_service.rs](../crates/application/src/sync_service.rs)。

globalな単一ApplicationHandleの危険は、同時実行のthread安全性ではなく、所有権の欠落である。queryはDB内全課題を読み、toggle/setはIDだけを使う。全ユーザーに同じDBを見せる上、設定save/deleteは他人のsecretを置換し、cached WebClass sessionは別人のrequestから再利用される。source keyが大学内で同じなら他人のデータへupsertされ得る。startup gate / last report / queue混雑まで共有される。mutexやunpredictable IDを加えてもtenant isolationにはならない。

### 5.2 候補比較

| 候補 | 長所 | 欠点・変更範囲 | 採否 |
| --- | --- | --- | --- |
| A. userごとに独立ApplicationHandle + DB + ports | core/schema/session/single-flightをほぼそのまま再利用。userごとのqueueとfailure境界が明確。 | 3 threads + 2 DB connections/userに加えHTTP資源。runtime registry、同一userのopen共有、drain、上限・idle evictionが必要。vaultもuser束縛必須。 | MVPと小規模multi-userに推奨。 |
| B. user単位request context + application servicesへ再設計 | DB poolとbounded executorを共有でき、PostgreSQL/分散jobへつなげやすい。 | 全repository操作へのUserId強制、sync lock/session/revisionのper-user管理、lifecycleの再設計が必要。現行syncはSqliteStore直結なので単なるwrapper追加では済まない。 | 実測でAの資源・運用限界が見えた段階。初期から実装しない。 |
| C. userごとの独立process / container | OS境界と専用DBで障害・secretの範囲を狭められる。 | process/key/upgrade/backup数が増え、routing・運用負担が大きい。single user self-hostには成立するが学生一般提供の標準案にはしにくい。 | 特別な隔離要件時の選択肢。 |

### 5.3 段階的運用

公開MVPは許可アカウント1件をcontrol storeのinternal UserIdへ対応付ける。認証後にもそのアカウントが許可対象か検証し、別の有効ログインIDは403にする。runtimeはserver起動で1回openしてreadiness queryを通し、requestはそこへ借用する。user path、credential owner、settings ownerをfactoryの引数に束縛し、request body / URLからpathやownerを選べないようにする。

少人数multi-user時はregistryをUserId→Opening/Ready/Closing状態に拡張し、同じuserへの並列初回requestでopen taskを共有する。user数・active runtime数の上限を設け、job / request leaseが残るruntimeはevictしない。Closing完了前に同userの新handleを作らない。logoutはWeb session失効と端末cache削除であり、他端末のhandleまでshutdownしない。account削除は新規requestを停止→accepted work drain→session/vault/DBを削除する別手順とする。

現行revision invalidationは「次sync前」であり、credential save/deleteと既に走るsyncがraceすると、そのrunは取得済み旧credential/sessionを使い続け得る。これは即時cancelの保証ではない。Webでは大学credential変更を同userのsyncと直列化し、running中の変更要求は409 sync_runningとしてretryさせる。処理中のstepまで即時失効させたい場合は別のcancellation/revision再確認契約が必要。再暗号化だけの鍵rotationと大学credentialのrevision更新は分離する。

server停止ではHTTP/job admissionを閉じ、採用済みWeb jobとaccepted application workを待ち、全handle.shutdown().awaitを完了させる。graceful deadlineは「HTTP1回15秒」だけではなく多数Course・redirect・reauthを含む全runを考慮する。現行にcancel APIがないため強制終了時はpartial commitと応答結果不明を許容し、restart後のrun状態を成功へ偽装しない。[Axum graceful shutdown](https://docs.rs/axum/latest/axum/serve/struct.WithGracefulShutdown.html)だけで独自worker/jobのdrainまで自動保証されるとは扱わない。

## 6. Database strategy

### 6.1 SQLite v3を維持する条件

**単一ユーザーMVPはSQLiteで十分検証可能。** 同一hostの永続local volumeへ置き、1つのuser runtimeがDBを所有する。DB schemaへWeb sessionやcredential fieldを追加する必要はない。[SQLiteの適用指針](https://www.sqlite.org/whentouse.html)にある1 DB fileあたり1 writerの制約を踏まえ、短いtransactionと現行workerの直列化を利用する。

現行 [configure](../src/persistence.rs) L1080はbusy_timeout=5000ms、foreign_keys=ON、journal_mode=DELETE、synchronous=FULLを明示する。**現状はWALではなく、WALを前提に性能を説明しない。** 2 Connectionでqueryとsyncのnetwork待ちを分けるが、DB書込時のlock待ちがゼロという意味ではない。WAL変更は必要性を計測した別変更とする。PersistenceError→ApplicationErrorでは多くのDB errorがStorageへ集約されるため、SQLITE_BUSYが常にHTTP 503になるという保証もない。Web request admissionのBusyとDB driver errorを区別する。

| 論点 | 推奨 |
| --- | --- |
| DB ownership | user専用のassignments.db。server生成UserIdからregistryでpathを解決。clientsからDB path / filenameを受けない。 |
| user isolation | user A / Bは別file、別handle、別settings/vault owner。DB内にuser_idがないことをこの境界で補う。owner解決前にqueryを発行しない。 |
| path保護 | data rootを公開static rootから分離。registryのtrusted mappingだけを使用し、root外・symlink/reparse point経由の逸脱を拒否。OS service account以外の書込を禁止。UserIdの単純文字列連結をauthorizationにしない。 |
| concurrent access | MVPは1 process、replica=1。2 worker connectionsの既存contractを維持し、DB fileをnative appや別serverから同時共有しない。load / contentionを実測する。 |
| migration compatibility | SqliteStore::openのv0(empty) / v1 / v2→v3を再利用。future version / 不明な非empty v0は失敗させる。v3にWeb metadata tableを足さない。 |
| Web metadata | 別control DBにaccount/session、user mapping、Web policy、暗号化record等を保持し独立versioning。single-process MVPではSQLiteでよい。既存core migrationと混ぜない。 |
| restart | assignments/completion/settings/ciphertextはvolumeから復元。大学cookie/sessionと現行last_reportは復元されない。Web sessionは設計したsession storeの期限・失効に従う。 |
| container | DB fileだけでなく親directoryを永続mountしjournal/temp/atomic置換にも書込可能にする。image layerや/tmpを本番保存先にしない。キーはimage/同volumeへ埋めない。 |
| scale | NFS共有SQLiteや複数serverから同じfileを開く構成を初期設計にしない。volumeがinstance限定ならreplica増設で別DBが生まれることも防ぐ。 |

### 6.2 Backup / recovery

最初は **Web/job受付停止→全handleをshutdownして接続close→課題DBとcontrol DBの整合snapshotをcopy→暗号化backup→再open** という短いmaintenance手順を採用する。動作中のDBだけを単純copyし、journalや別control DBとの整合を無視する手順は不可。後にonline backupが必要なら[SQLite Backup API / VACUUM INTO](https://www.sqlite.org/backup.html)を用いる専用toolを追加する。現在のroot Cargo.tomlのrusqlite featuresはbundledだけなので、既存SqliteStoreにbackup APIがあるとは扱わない。

公開前に、日次backup・最低7世代保持、目標RPO 24時間/RTO 4時間を暫定運用値として決め、実際のrestore演習で達成可能か確認する。これは現在の達成済みSLAではない。暗号化backupを稼働hostとは別の保管先へ保存し、管理権限・鍵のbackup・保管期間を分ける。restoreは隔離directoryでschema version / integrity_check / foreign_key_check / 件数 / internal ID / completion / settings / vault復号を検証してから切り替える。失敗時は元volumeへ戻す。

backupにも課題タイトル、大学IDを含むciphertext等が残る。大学credential削除後のbackup残存期間を運用方針に含め、漏洩時は暗号化されていてもtoken/passwordのrotation判断を行う。鍵紛失時はciphertextを復号できず、大学credentialの再入力が必要になる。旧keyを廃棄する前に保持backupの復元方針を決める。

### 6.3 既存desktop DBの引継ぎ

将来、所有者が明示的に選んだ1つの停止済みdesktop DBのcopyを、認証済み同一所有者のserver DBとして管理手順で採用できる。native正式macOSとTauriのDB保存先・Keychain serviceは異なるため、どちらが正本か先に選ぶ。

手順は、元app終了・backup→隔離copyのschema/整合検証→SqliteStore::openで必要なmigration→title/deadline/course/binding/completion照合→user専用volumeへ配置→server reopen。internal AssignmentId / CourseId、source scope、observationをそのまま維持し、URL rotationでscopeを書き換えない。v2からのlegacy course scopeは現行migrationのまま残し、無根拠に現在のMoodle feedへ割り当てない。

source identityはsingle-profileなので、複数人のDBを結合してはいけない。同じ利用者のWindows/macOS DBも、completion競合と同一外部keyの重複を解く別migration設計なしにはmergeしない。最初は一方を正本にする。一般利用者向け任意SQLite upload APIはMVPに追加しない。credentialはDBに含まれない契約であり、OS vaultをcopyしてもWeb用keyにはならない。Webで再入力する。今回いずれのDB/credentialも移行しない。

### 6.4 PostgreSQLへの将来移行

開始条件は、active users / thread数 / DB lock待ち / backup時間 / 複数instance要求の実測。単一ユーザーの段階で全面repository化しない。

移行時は、core domain / import ownership / query規則を維持し、persistence操作をuser必須のrepository契約へ切り出す。[sync.rs](../src/sync.rs)と[SyncBackend](../crates/application/src/sync_service.rs)は現在SqliteStoreを直接受け取るため、sync serviceも変更対象になる。全tableとqueryへtenantを導入し、unique keyを(user_id, origin, scope, external_id)、scope freshnessを(user_id, kind, scope)にする。Course参照も(user_id, course_id)の複合foreign key等で他tenant参照をDBでも拒否する。user_id columnを追加しただけで完了としない。

各SQLite DBのownerを付けてID・timestamp・DateOnly/UTC・completion・bindingを取り込み、件数/参照/再syncを検証する。ID衝突に備えtenant付きprimary keyまたは明示的remapを決める。read切替前にdry run、切替時のwrite停止、元DB保持、rollbackを用意する。分散single-flight/credential generation/job復旧も同時に設計し、PostgreSQL採用だけでこれらが解決したとは扱わない。

## 7. Web authentication and credential security

### 7.1 4種類のidentity / secretを分離する

| 対象 | 提案する保持場所・扱い | Browserへ返す情報 |
| --- | --- | --- |
| 1. Webアプリ自身のログイン | MVPは独立したOIDC identity providerを候補にし、issuer + subjectをinternal UserIdへ対応。許可アカウント1件、signup無効。大学SSO利用権があるとは仮定しない。 | opaqueなapp session cookie、safe user表示情報、CSRF token。OIDC tokenをlocalStorageへ保存しない。 |
| 2. Moodle token URL | server vaultの暗号化record。bearer secretとして保存・利用時だけ復号し、送信先policyを適用。 | configured booleanだけ。保存済みURL・tokenを返さない。 |
| 3. WebClass大学ID/password | owner付きserver vaultへ単一暗号化recordとしてatomic置換。backend loginの直前だけ復号。 | configured booleanだけ。大学IDも既存同様に再送しない。 |
| 4. WebClass session/cookie | user runtimeのWebClassClient / Jar内のmemoryだけ。restart後は再login。 | 渡さない。Web app session cookieとは別物。 |

既存[SecretText](../crates/application/src/credentials.rs)はDebug/Clone/Serializeなし、owned bufferをzeroizeする。[SettingsStatus](../crates/application/src/settings.rs)はbooleanのみ。[CredentialStore](../crates/application/src/credentials.rs)のreplacement失敗時に旧値を維持するcontractをEncryptedCredentialStoreにも適用する。ただしHTTP/JSON/OS/JS内部のcopy全てをzeroizeできる保証にはしない。

OIDCはAuthorization Code + PKCE、state、nonce、issuer/audience/signature、redirect URI完全一致、subject mappingを検証する既存libraryを選ぶ。providerの新規契約・大学SSOとの連携は未決であり、Web-3開始時に確定する。local Web password方式を選ぶ場合は大学passwordと別のpasswordを[Argon2id等でhash](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)し、回復/失効手順を作る。大学passwordはbackend再loginに必要なのでhashだけでは代替できない。

### 7.2 Web sessionとauthorization

server生成の十分な乱数（例: 32 bytes）のopaque session IDをCookieに置き、serverにはhashとUserId、作成/last activity/absolute expiryを保持する。login時にsessionをrotateし、logout/失効/アカウント無効化でserver側recordを無効にする。初期案はidle 12時間・absolute 7日、大学credential操作前の再認証を要求する。値は運用試験で確定し、永続無期限loginを作らない。

公開Cookie案は __Host-am_session、Secure、HttpOnly、SameSite=Lax、Path=/、Domainなし。OIDC top-level returnと整合するLaxを選び、state変更APIではsessionに束縛したCSRF tokenをX-CSRF-Tokenに要求する。logout、sync、設定変更も対象。Originの固定allowlist照合、Fetch Metadata等を補助に用い、SameSiteだけで済ませない。[OWASP session guidance](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html)、[CSRF guidance](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)を参照。

user-facing APIはすべて認証後にownerを解決する。courseId / assignmentId / runIdが他人のものでも、そのuserのstore内だけで検索し404にする。Cookieがあるだけ、IDが推測困難、画面に隠れている、同大学IDらしい、という条件をauthorizationにしない。proxy認証を使う場合も外部から到達可能なbackendや偽造headerを許さず、検証済みidentityをUserIdへ束縛する。

### 7.3 暗号化、鍵、rotation

Web-3でuser-bound CredentialStore adapterを追加する。初期案は認証付き暗号AES-256-GCMをmaintained libraryで使用し、recordごとの安全な乱数nonce、ciphertext、authentication tag、key ID、format versionを保存する。AADにinternal UserId、secret種別、record versionを含め、他user / 他種別へのciphertext差替えを失敗させる。nonce再利用を防ぐ。[OWASP cryptographic storage](https://cheatsheetseries.owasp.org/cheatsheets/Cryptographic_Storage_Cheat_Sheet.html)のauthenticated encryption・key/data分離に基づく設計案で、独自暗号を実装しない。

鍵はrepo / image / 課題DB / control DB / 同じbackupへ保存しない。VPSなら専用service identityだけが読める外部secret mountを最低構成にし、利用環境に応じてsecret manager/KMSとwrapped data keyを採用する。OS disk encryptionも併用するがrecord暗号化の代替にしない。起動時にkey/version不整合を検出したらcredential機能をfail closedにし、平文fallbackやsilent resetをしない。offline課題readは既存application同様に維持できる。

暗号化recordの書込完了後にだけsave成功を返す。失敗・途中crashで旧recordを消さない。大学credentialのsave/deleteは同じSettingsServiceを必ず経由し、revision更新→次syncの旧session破棄を維持する。MVPはsingle runtimeなので現行RAM revisionで成立する。process restartではsessionもなくなる。multi-instance / vault外部書換えを導入する前にdurable credential_generationとsession generation照合、変更通知、per-user sync ownerを設計する必要がある。adapterのloadだけを変えてもSettingsServiceのRAM revisionが分散対応するわけではない。

暗号鍵rotationは新key IDを追加→新規writeを新key→旧recordを復号・再暗号化→検証→保持backupを考慮して旧keyをretireする。単なる同一plaintextの再暗号化で大学credential revisionを進める必要はない。大学password/tokenのrotationはsave API経由で行い、WebClass session invalidationを伴う。事故時はapp session失効、大学credential再設定、必要なkey再発行を分けて実施する。

### 7.4 Web securityとserver egress

| 領域 | 公開前に実装・検証する条件 |
| --- | --- |
| HTTPS / proxy | 全外部trafficをTLS化、HTTP→HTTPS、証明書自動更新監視。HSTSはdomain運用を確認して有効化。backend bindはprivate/loopback、trusted proxy以外のForwarded/X-Forwarded-*を信用しない。 |
| authentication / authorization | session検証・許可account・owner解決を共通middleware化。未認証401、許可外403、他user resourceは404。A/Bの交差read/write/settings/jobテストを必須化。 |
| CSRF | unsafe method全てにtokenとOrigin検証。GETでsyncや設定変更を起こさない。JSON Content-Typeを厳格化しform postを受けない。OIDC login CSRFもstateで防ぐ。 |
| XSS / CSP | textContent方式を維持し、source title / Course labelをHTML化しない。CSP例はdefault-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'; worker-src 'self'; manifest-src 'self'。OIDCの方式に合わせ必要最小限のみ調整。unsafe-inline/eval、外部analytics、第三者scriptを導入しない。 |
| request制限 | body/URL長・JSON nesting・timeout上限、user/IPごとのlogin/sync/settings rate limit、全体network concurrency制限。queue overflowは即座に安全なerror。DB全件snapshotが大きくなる場合は計測後pagination契約を検討。 |
| logs / errors | body/query/cookie/header/secret DTOをDebug・tracingへ入れない。proxy/access/APMも対象。URL token、大学ID/password、SAMLResponse/RelayState、Location、raw cause、課題本文をlogへ出さない。固定code + request/run ID + 所要時間/件数だけを基本にする。 |
| credential handling | 入力はHTTPS POST/PUT bodyのみ、URL/ブラウザstorageへ入れず、入力欄は送信・終了・画面離脱時clear。saved secretのGET APIなし。status read failureは未設定と偽らずunknown/errorにする。 |
| secure configuration | 非root、最小filesystem権限、read-only image + 専用volume、debug無効、secretなしのhealth、static root分離、dependency更新/audit、error redaction。productionでauth無効flagを受け付けない。 |

CSPのconnect-srcはBrowserからの接続制限でありserver SSRF対策ではない。[CSP一次資料](https://cheatsheetseries.owasp.org/cheatsheets/Content_Security_Policy_Cheat_Sheet.html)に基づきHTTP response headerとして配信し、Tauri固有のipc: / http://ipc.localhost policyをWebへそのままcopyしない。

**Moodle HTTPS validationだけではSSRFは防げない。** MoodleFeed::newはhttps://127.0.0.1、private network host、任意portも仕様上排除していない。Web版の保存時と実行時に、operatorが検証した正確な大学Moodle hostname・port 443・必要なcalendar export pathだけをallowlist化する。実tokenのhost/pathは今回読んでおらず、正しいallowlist値は未確定である。*.hosei.ac.jpを無制限許可する案にしない。

URL libraryでparseし、userinfo、IP literal、fragment、非許可scheme/port/path、紛らわしいhost suffixを拒否する。query内のtoken値はsecretとして保持し、その内容からtenantやcourse scopeを生成しない。保存後にもpolicy変更やDNS変化があるので、fetch直前に再検証する。

DNSのA/AAAAでloopback/private/link-local/metadata/reserved等の非公開宛先を拒否し、**検証したIP集合と実際の接続先を一致させる**resolver/egress gatewayを採用する。先にDNS検証して通常clientでもう一度解決するだけではrebinding/TOCTOUを防げない。TLSのhostname検証を残し、raw IPへURLを置換してTLS検証を無効にしない。network firewall/egress proxyでもprivate networkとmetadataへの到達を遮断する。[OWASP SSRF guidance](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html)を参照。

Moodleのredirect禁止を維持する。WebClass/OpenAMは既存resolve_destinationのorigin検証を維持し、固定3 originsと必要宛先だけをserver policyへ登録する。serverの環境proxy設定で制御を迂回しないよう明示的に管理する。HTTP body、Course数、全run時間にも上限を設計し、途中失敗は従来のpartial commitとして扱う。現行fetch_calendar / fetch_json / read_okはbody全体を読むので、Web-4ではbounded readとserver policyを注入する小さなHTTP constructor拡張が必要。save APIの検証だけを加えて完了とはしない。

### 7.5 大学credentialを第三者serverへ預けるリスク

record暗号化はDB/backup単体流出の被害を減らすが、稼働serverの侵害、operatorの不正、依存libraryやmemory dumpからのsecret漏洩を防ぎ切れない。大学passwordが他大学サービスへ共通利用されるなら影響範囲は課題一覧を越える。利用者へ保管主体・目的・保持期間・削除・incident連絡方法を説明し、明示的に預ける判断を求める。公開一般提供前に大学の利用条件、SSO自動操作・credential保管の許容範囲、データ取扱方針を確認する。現状から許可済みとは断定しない。

hosting IP制限、MFA、追加challenge等で自動認証できない場合はWebClassを利用不可として固定errorを返し、既存課題readとMoodleを継続可能にする。ブラウザへ大学Cookieをexportしたり認証検証を緩めたりして回避しない。self-hostを選ぶ選択肢も残す。

## 8. API contract proposal

### 8.1 Transport境界と共通規則

REST/JSONを第一候補とし、/api/v1をprefixにする。新規 dto.rs / requests.rs / error.rs はWeb crate内に置き、[application models](../crates/application/src/models.rs)、AssignmentQuery、SettingsStatus、SyncReportから明示変換する。Tauri DTOの形を参考にするがsrc-tauriへ依存しない。core AssignmentをそのままSerializeしない（submission_urlが含まれるため）。

全user-data responseはCache-Control: no-store、Content-Type: application/json、X-Content-Type-Options: nosniff。401時にAPIがHTML loginページへredirectしてJSON parse errorにしない。errorは {"error":{"code":"invalid_query"},"requestId":"opaque"} のようなallowlist codeのみ。serde rejectionのraw説明や入力値をresponseへ含めない。

GET assignmentsのqueryは completion=all/incomplete/completed、courseMode=all/unassigned/course、courseId（course時必須）、deadline=all/overdue/today/within_seven_days、sort=deadline/course/title。省略はapplication default。unknown / duplicate / 非許可値を400、ID長上限超過等も固定errorにする。courseIdはexact opaque IDであり名前やsource scopeではない。MVPはtimezone入力を受けずAsia/Tokyoを返す。term / direction / 任意SQL / 任意nowは受け付けない。

### 8.2 最小use-case API

下表は公開MVP時のcontract。Web-1ではassignments GETとhealthだけをloopbackで実装する。**要認証=必須**のendpointを公開環境で無認証にする例外はない。unsafe methodは認証に加えてCSRFを要求する。

| Method / endpoint | Request | Response概要 | 正常 / 主なstatus | 認証 |
| --- | --- | --- | --- | --- |
| GET /api/v1/assignments | 上記filter/sort query | items / courses / totalCount / evaluatedAt / timeZone | 200、400、401、503、500 | 必須 |
| PUT /api/v1/assignments/{id}/completion | {completion:"completed"または"incomplete"} | {id, completion}、commit後ack | 200（同状態も200）、400、404、401、503、500 | 必須 |
| GET /api/v1/settings/status | なし | moodleConfigured / webclassConfigured、Web用sync policyのsafe flags | 200、401、503、500 | 必須 |
| PUT /api/v1/settings/moodle | {calendarUrl:"入力したsecret"} | 空body。status取得は別request | 204、400、401、409、503、500 | 必須・最近の再認証 |
| DELETE /api/v1/settings/moodle | なし | 空body。未設定削除も成功 | 204、401、409、503、500 | 必須・最近の再認証 |
| PUT /api/v1/settings/webclass | {universityId:"入力値",password:"入力値"} | 空body。saveはloginを起動しない | 204、400、401、409 sync_running、503、500 | 必須・最近の再認証 |
| DELETE /api/v1/settings/webclass | なし | 空body。次syncは旧sessionを使わない | 204、401、409 sync_running、503、500 | 必須・最近の再認証 |
| POST /api/v1/sync | bodyなし、Idempotency-Key header必須（Web用） | {runId, state:"running", statusUrl} | 202、新規受付不可429/503、401 | 必須 |
| GET /api/v1/sync/status | なし | running / activeRunId / lastRun、安全なsource結果・時刻 | 200、401、503 | 必須 |
| GET /api/v1/sync/runs/{runId} | opaque run ID | state / report / startedAt / finishedAt | 200、404（他user含む）、401、503 | 必須 |
| GET /health/live | なし | 固定のalive | 200 | 外部公開なら最小情報のみ |

共通で、CSRF不正403、許可外account403、unsupported media type415、body過大413、rate limit429、queue full / draining503 + Retry-After、内部storage/credential失敗500の固定code。既存ApplicationError::NotFoundは404、InvalidQuery/InvalidAssignmentId/InvalidMoodleFeed/InvalidCredentialsは400へ変換する。credential errorの種類はsafe codeだけで、saved secretやprovider causeを返さない。

認証support endpointの追加案は GET /auth/login（OIDCへredirect、未認証可）、GET /auth/callback（state/PKCE等の検証後session作成、失敗は固定error）、GET /api/v1/session（認証済みuserのsafe identity / csrfToken / expiry）、POST /auth/logout（認証+CSRF、204・Cookie破棄）。callbackのcode/tokenはlogしない。これらは課題use caseとは別のWeb transport機能である。

### 8.3 JSON例とDTO再利用

```json
{
  "items": [{
    "id": "synthetic-internal-id",
    "title": "合成レポート",
    "deadline": {"kind": "date_only", "value": "2026-10-31"},
    "completion": "incomplete",
    "submission": "web_class",
    "hasSubmissionUrl": false,
    "course": {"id": "synthetic-course", "displayLabel": "合成Course", "origin": "web_class"},
    "status": "none"
  }],
  "courses": [{"id": "synthetic-course", "displayLabel": "合成Course", "origin": "web_class"}],
  "totalCount": 1,
  "evaluatedAt": "2026-10-21T12:00:00Z",
  "timeZone": "Asia/Tokyo"
}
```

deadlineのdate_timeはUTC RFC3339（必要な小数秒を保持）、noneは {kind:"none"}。Course originのweb_class等、既存[Tauri dto.rs](../src-tauri/src/dto.rs)のtagを揃えるとpresentation.jsを再利用しやすい。上例のstatusは例示でありserverの実clock判定が正本。saved token URL / 大学ID/password / Cookie / source external identity / raw submission URLはどのresponseにも含めない。

SettingsStatusのsync_on_startupはdesktop lifetimeの設定である。Webへそのまま「サーバー起動時同期」として公開しない。Web statusにはautoSyncOnOpen（初期false、別Web設定）を必要時に追加し、desktop設定とは別名・別policyにする。自動同期を実装するまではWeb UIにこの設定を出さない。settings save成功後status GETが失敗しても、save自体を失敗扱いしてsecretを再送させない。

### 8.4 Sync受付と観測

初期HTTP syncを長い200待ちにする案は既存sync_now().awaitを直接使えて最小だが、hosting timeoutと切断後の結果把握が難しい。Web-4では **202 + server-owned job** を推奨する。ただし現在のApplicationHandleにrun IDや永続jobはないので、WebSyncCoordinatorを明示的に追加する。

coordinatorはuser単位の短いadmission lockでrunを予約し、server-owned taskをspawnしてsync_now().awaitを保持する。予約が完了してから202を返し、予約直後にstatus GETしてもrunningが見えるようにする。HTTP response切断でtaskをdrop/cancelしない。全Web manual / future auto要求はここを経由し、既存SyncWorkerのsingle-flightも最後の防御として維持する。mutexをnetwork await中に保持しない。

同じuserの同じIdempotency-Keyは実行中/完了済みを問わず同じrunへ結び付ける。別keyでもrunning中なら既存active runを202で返し、queueしない。同じkeyの違う要求は409。記録にはowner/key hash/request hash/run outcome/有効期限を持たせ、初期保持24時間・件数上限を設定する。単一process MVPでは完了履歴をmemoryに置く選択もあるが、**その保証はprocess lifetime内だけ**とresponse/documentationで明示する。restart後は直前runを再実行せず、未知/中断としてqueryと状態確認を先に行う。必要ならWeb control DBでdurable記録を導入する。

reportは既存SyncReport / SourceSyncResultから投影し、Completedはstate=finishedでありsuccessという名称にしない。全source失敗でもjobが正常終了すればGETは200、report内にsource別Failedを返す。FailedがDB unchangedを意味しないこと、再queryが必要なことをshouldReloadで明示する。process再起動でlast reportが消えた状態はnull/unknownで返す。

Browserはrunning中だけ数秒間隔でpollし、終了時に現在のfilter/sortで再queryする。複数tabは同じrunを観測し、終了前に再submitしない。runIdのowner検証は必須。GET statusは大学通信を起動しない。

## 9. Frontend strategy

### 9.1 比較と採用

調査HEADのfrontendはapp.js 234行、presentation.js 131行、settings.js 127行、sync.js 102行、settings_view.js 84行、sync_view.js 29行、list_tasks.js 9行、index.html 140行、styles.css 107行（合計963行、JS部分716行）。npmやfrontend framework依存はない。

| 案 | 再利用性 / 保守性 | スマホ・将来機能 / コスト | 判断 |
| --- | --- | --- | --- |
| A. vanilla維持 + API adapter | presentationのgrouping/date表示、settingsモデル、sync結果表示を大部分維持。transport注入の既存構造を利用できる。型・JSON契約の検証は別途必要。 | responsive CSSを拡張でき、PWAもframework必須ではない。変更面が最小。 | Web-2の第一候補。 |
| B. TypeScript + Vite | 型付きAPI contractとmodule buildを導入できるが、TS化とHTTP接続を同時に行う必要はない。DOMコード自体は再利用可能。 | build/package/lockfile管理が増える。画面や状態が増えた際に段階的導入を検討。 | 保守上の具体的要求が生まれた後。 |
| C. React / Next.js等 | component/state構造は刷新が必要。現行関数の再利用は可能でもViewの大半を書き換える。 | 現要件の一覧・設定・同期だけでは費用に見合う根拠がない。SSR/BFFを追加するならRustの認証/大学通信との責任を再整理する必要。 | 初期は採用しない。 |

判断は現在のコード量とUI責務に基づく。スマホ対応を理由にframeworkを必須にしない。filter / sort / deadline判定 / source sync / credential復号はRustの責務とし、JSへbusiness ruleを複製しない。

### 9.2 invokeの具体的置換

新規 frontend/api.js に画面向けAPIをまとめ、frontend/tauri_api.js とfrontend/http_api.jsを分ける。browser向けentrypointとTauri向けentrypointでadapterを明示的に選ぶ。__TAURI__の存在判定はtransportの選択にだけ使え、認証・公開可否の判定には使わない。productionの誤設定で無認証HTTPへfallbackさせない。

| 現在のcall site | Webでの変更 |
| --- | --- |
| app.jsのinvoke / query | api.listAssignments(buildQueryRequest(selected))を呼ぶ。HTTP adapterがquery stringへ変換しGETする。Tauri adapterは既存list_assignments invokeへ変換する。 |
| app.jsのtoggleNow | api.setCompletion(id, desiredState)を呼ぶ。checkbox操作時の希望状態を引数にし、HTTPではPUT。既存desktop toggle pathは保持し、Tauri側を無理にHTTP retry契約へ合わせない。 |
| settings_view.jsのcreateSettingsModel注入 | direct window.__TAURI__をやめ、adapterのsafe command mappingを注入する。secret requestは一時変数のみ、保存済み値の再取得なし。 |
| settings.jsのstartup設定 | Webでは未実装policyを表示しない。後にautoSyncOnOpenを別modelとして扱い、set_startup_syncへ誤変換しない。 |
| sync.jsのexecute | Web adapterは202受付→run status poll→既存形のsafe SyncReportを返す専用flowを追加。単なるfetch置換では現在のcompleted即時response前提を満たせない。 |

HTTP fetchはsame-origin cookieを使用し、mutationにCSRF header、application/json、timeout/AbortSignalを設定する。AbortはBrowser側の待機終了でありserver rollbackではない。adapterはresponse shape / statusを検証し、任意error.messageやHTTP bodyを画面/logへ出さず、既知codeへ変換する。401はlogin案内へ、offlineは前回snapshotと結果不明の状態へ明示的に遷移する。

app.jsの「committed=falseなら完了状態は変更されていません」はIPCのエラー前提でも注意が要るが、HTTPへはそのまま持ち込まない。request送信後のnetwork failureは結果不明と表示し、現在の状態を再取得する。query error時の前回一覧保持、commit成功後のrefresh failureの別表示、filter rollback、focus保持を引き継ぐ。

createListTaskRunnerはtab内の処理順を保つが複数tab間の制御ではない。Webではquery generationで古いresponseによる上書きを防ぎ、同一課題のwriteを直列化する。BroadcastChannel等はtab間の再取得通知に使えても、DB整合やsingle-flightの正本にはしない。設定画面の「OSの認証情報ストアへ保存」はWeb配信時にserver暗号化保管の説明へ変更し、Tauri表示は維持する。

## 10. Mobile / PWA considerations

### 10.1 Phone UI

[index.html](../frontend/index.html)はviewportとlabel/aria-live、[styles.css](../frontend/styles.css)はflex-wrapとmax-width:540pxのmedia queryをすでに持つ。ただしTauriの最小window幅420pxはWebの320–390pxでの品質保証ではない。Web-5で以下を検証する。

- 320/360/390/430pxおよびtablet/desktopで横scrollを出さず、header actions・filtersを縦積み/折返しにする。長い日本語Course/課題名を切らずに読めること。
- checkbox本体は現在22pxだが、labelを含むtouch targetを44px以上目安にし、誤操作しにくい間隔を確保する。hover必須操作を作らない。
- zoom、screen reader、keyboard navigation、focus ring、読み込み/結果不明/保存済み・refresh失敗の通知を維持する。
- phone keyboardとsafe area、portrait/landscape、tab復帰、複数端末でのcompletion反映を実機で確認する。fontや入力欄の都合で画面拡大を禁止しない。

### 10.2 PWA設計（今回実装しない）

Manifestはid / name / short_name / start_url / scope / display=standalone / theme_color / background_color / icons（192/512、maskable等）を用意しHTMLから参照する。start_urlにuser ID・token・sessionを入れない。install UIや要件はbrowser/OS差があり、Service Worker追加だけでinstall完了とはしない。[MDN installability](https://developer.mozilla.org/en-US/docs/Web/Progressive_web_apps/Guides/Making_PWAs_installable)を参照。iOS/iPadOS 26ではHome Screen追加時のWeb App扱いに変更があるため、古い「manifestがないと単なるbookmark」という説明へ固定しない。[WebKit 26.0](https://webkit.org/blog/17333/webkit-features-in-safari-26-0/)

Service WorkerはHTTPS（localhostは開発例外）で登録し、app shellだけをversion付きprecacheにする。[Service Worker lifecycle](https://developer.mozilla.org/en-US/docs/Web/API/Service_Worker_API/Using_Service_Workers)に従いinstall/activate/waitingを扱う。新version通知→pending mutation終了→利用者のreloadで更新する方式を初期案とし、無条件skipWaitingによる操作途中の旧UI/新API混在を避ける。static assetsはhash/version付き、API v1との互換範囲を持たせ、activate後に古いcacheを整理する。

| データ | cache方針 |
| --- | --- |
| HTML/CSS/JS/iconsのapp shell | version付きcache。認証情報やuser dataを埋め込まない。login/callback responseはcacheしない。 |
| 課題一覧 | 通常HTTP cache / CDNではno-store。将来、利用者が「この端末にoffline一覧を保存」を明示的に有効にした場合だけsafe DTO snapshotをIndexedDB等へ明示保存。SWで全/api GETをcacheしない。 |
| credential settings / auth / session / sync mutation | browser永続cache対象外。Moodle URL、大学ID/password、大学Cookie、app session tokenをlocalStorage/IndexedDB/Cache Storageへ保存しない。 |
| completion write | 初期はonlineのみ。offline中はread-onlyにし、queueされたwriteが後で他端末の状態を上書きする仕組みをMVPに入れない。 |

offline一覧にはserver側evaluatedAt、端末取得時刻、timeZone、「offline・前回取得」を表示する。「最終取得」と「大学同期成功」を別にする。現行sync_scopesのlast_synced_atはscopeごとのpositive freshnessで、WebClass course-list成功が全group成功を意味しない。MVPはrun結果とsafe取得時刻を表示し、厳密なsource freshnessを出す段階で専用read modelを追加する。raw scope/group IDはBrowserへ出さない。

offline snapshotはUserIdに対応したopaque cache namespace、query、schema version、timezoneに分ける。前利用者のcacheを新利用者へfallbackしない。logout / account切替時は全tabへ通知してmemory・IndexedDB・cacheを削除し、削除完了前に別accountのoffline表示を許さない。401/失効時はcacheを最新認証の代わりに使わない。保存はdefault OFF、shared deviceでは使わない案内にする。

**offlineデータのrevocationには限界がある。** serverのlogout/アカウント失効をoffline端末へ即時配信できず、OS/browserのローカルprofileを読める人への強い秘匿も保証できない。短い保持期限と端末lockを前提にし、実装時にlocal copyの許容範囲を確認する。cache partitionはserver authorizationの代わりではない。browserはstorageをevictし得るため、初回offlineやcache消失時は一覧を保証しない。

通知は別Phaseとする。iOS/iPadOSでは少なくとも16.4でHome Screen Web App向けWeb Pushが導入され、利用者操作に応じたpermission要求が必要。[WebKit Web Push](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/)を参照。Android/desktopもbrowser・permission・省電力等で差があり、background JS常駐・定刻delivery・大学syncの周期実行は保証しない。Push subscriptionのuser管理やserver schedulerが別途必要であり、今回のPWA設計に通知実装を含めない。

## 11. Deployment options

以下は構成比較であり、特定serviceの料金・無料枠・現在のプランを前提にしない。採用時は公式仕様でpersistent volume、長時間process、outbound HTTPS、secret注入、region、backup、価格を改めて検証する。

| 観点 | A. 自宅PC等でself-host | B. 小規模VPS | C. PaaS等 |
| --- | --- | --- | --- |
| Windows PCの常時起動 | そのPCでhostするなら必要。sleep/回線断で利用不可。別の常時稼働hostを用意すればPC依存を外せる。 | 不要。VPSは常時運用する。 | 不要。選んだruntimeが常時起動・jobに対応するか確認。 |
| スマホ外部アクセス | LAN接続だけでは外出先不可。public入口、NAT/CGNAT/firewall、または端末用private network等を設計。入口でauthを省略しない。 | domain/DNS + TLS proxy + firewallで提供。backendやDB portは公開しない。 | HTTPS入口はservice機能を確認。独自domain/session/callback設定を確認。 |
| HTTPS | 自分で証明書/更新/入口を運用。 | 自分でproxy/証明書更新を運用。 | service側機能を利用可能な場合もtrusted proxyを正しく設定。 |
| DB永続化 | local diskだが故障・盗難・誤削除のbackupが必要。 | local persistent disk / volumeを明示。single replicaでSQLiteと相性がよい。 | ephemeral filesystem型では不可。instanceとvolumeの関係・replica制限を確認。 |
| credential保護 | 第三者hostへ預けずに済むがOS侵害やremote入口のリスクは残る。 | operatorが大学credentialを保管する責任を負う。key/DB/backup分離が必要。 | platform operator/secret管理と可視性を確認。自動的に安全になるわけではない。 |
| 運用負担 | 電源、回線、PC更新、可用性、入口の保守。 | OS更新、TLS、monitoring、backup、incident対応。構成が明示的。 | OS作業を減らせる反面、sleep/redeploy/storage/job制約の理解が必要。 |
| コスト | hardware/電気/回線/運用時間。無料とは扱わない。 | server/storage/backup/domain等の継続費。金額未評価。 | runtime/storage/egress/backup等。無料枠に永続性を依存しない。 |
| 複数user | 少人数は可能だが個人回線・hostが障害点。 | user専用DB/handle方式を段階導入しやすい。 | 1 replicaで始め、horizontal scale前にDB/job/session設計の変更が必要。 |

推奨は、**Web-1/2はlocalhost、公開MVPは常時稼働のsingle-host Rust server + 永続volume + TLS proxy**。VPSを第一比較候補とし、同じ条件を満たすPaaSも選択可能。自宅self-hostは本人向け実験・private利用に合理性があるが、他学生向け可用性/incident対応までPC常時起動に依存する構成は再検討する。

localhost PoCの成功は外出先phone利用の証拠にならない。公開前に携帯回線からdomainへ接続、HTTPS、認証、再起動後データ保持、backup restoreを実証する。serverのtimezoneをUTCにしても表示/判定がAsia/Tokyoのままであることも確認する。

## 12. Risks and blockers

### 12.1 Sync semanticsと起動契機

確認済みのsource独立性は **Moodleの成否にかかわらずWebClassも実行する**意味であり、現在はSyncService::run内でMoodle→WebClassと順次処理する。parallel source fetchの保証はない。MoodleのCourse発見は別commit、WebClassはcourse-list freshness / Course作成 / 各group importが別commitで、途中失敗しても既存commitは残る。completionとsource identityを保持し、candidate不在で消さない。

| 契約 | Webでの維持方法 |
| --- | --- |
| partial failure / partial commit | source別reportを返し、Failedでもrequery。全体transaction化・全source rollbackへ変えない。 |
| AuthExpired→reauth→retry once | core sync_webclass_all_with_reauthを呼び、HTTP/UI wrapperは追加のlogin loopを作らない。MFA/未知patternは停止。 |
| session reuse | user runtimeのsessionを再利用。credential revision変更時は次run前にdrop。requestごとのloginをしない。 |
| identity維持 | Moodle MOODLE_SCOPE=default.course-relatedとCourseRelatedを維持。大学profileを変更する場合はtoken rotationと同一視しない。WebClassはgroup_id + contents_idをuser DB内で扱う。 |
| completion保持 | importerはlocal completionを上書きせず、completion APIだけが利用者の状態を変更する。 |

| single-flightの範囲 | 現状で有効か |
| --- | --- |
| 同じApplicationHandleのclone / 同userの複数tab→同runtime | 有効。response喪失後もrun終了まで維持。 |
| 同じuserの別ApplicationHandle | 無効。registryで重複openを禁止する必要。 |
| 別userの独立handle | 独立実行できる。全体egress concurrency上限はWebで別途設定。 |
| 別process / 別server instance | 無効。SQLite writer lockはnetwork/login重複を防ぐsync lockではない。 |
| server restartを跨ぐrun | 無効。running / last_report / sessionはRAM。途中commitはDBに残り得る。 |

| 同期契機候補 | 評価 / 方針 |
| --- | --- |
| pageを開くたび | tab/再描画/復帰で大学通信が増える。初期は一覧queryだけ。将来ONならuserごとのfreshness/cooldownをserverで評価して一度だけadmitする。 |
| login時 | loginは端末ごとに発生し、sync成功をlogin条件にすると大学停止でWebも使えない。login直後はlocal dataを返し、必要時だけ同じcoordinatorへbackground要求する。 |
| server起動時 | 全userのsecret読込・同時loginを誘発し、desktop startup onceと意味が違う。MVPでは無効。restartで勝手に大学loginしない。 |
| 定期schedule | pageを開かなくても更新可能だが、負荷/運用/credential保持責任が増える。MVP後に利用者opt-in、jitter、backoff、上限、per-user lock付きで追加する。 |

初期MVPは手動同期だけとし、終了後一定のcooldown（初期案60秒）をuserごとに適用する。running中の要求は同じrunを返し、network/auth失敗後も無限retryせずbackoffする。将来のautoSyncOnOpenは「初回local表示→serverがstale判定→必要なら同じcoordinator」で、初期stale閾値例15分。認証失敗が続くと自動syncを止め設定確認を促す。閾値は大学負荷と実測で調整する提案値で、既存startup設定を読み替えるものではない。

複数instance化前はDBやdistributed job storeのuser単位leaseとfencing / lock喪失時の停止方針、durable credential generation、run復旧を設計する。単なる期限付きlockが切れた後に旧workerも動き続ける場合の二重loginまで考慮する。initial deploymentはreplica=1でこの未実装機能を要求しない。

### 12.2 Blocker一覧

| リスク / 未確認事項 | Web-1を止めるか | 解消を要求する時点 |
| --- | --- | --- |
| Axum依存取得・Linux build・HTTP Send/Sync統合は未実証 | いいえ。PoCの検証対象。dependency導入後にcompile/testする。 | Web-1完了前 |
| local workerのOS timezone依存 | いいえ。加算clock APIで小さく解決できる。 | Web-1の時刻contract検証、公開前に必須 |
| Web auth / tenant registry / encrypted vaultがない | synthetic loopbackのみならいいえ。実データの公開にはblocker。 | Web-3 + Web-6 |
| Moodle exact allowlist / DNS-to-connect防御 / body上限未実装 | syntheticのみならいいえ。大学sync有効化にはblocker。 | Web-4 |
| hostingから大学SSO成功するか不明 | いいえ。公開版WebClass機能にはblockerになり得る。 | Web-4の利用者主導smoke、Web-6の対象host確認 |
| MFA / challenge / WAF / 固定2025 hostsの年度継続 | いいえ。mockで証明できない。検知して停止する。 | 実sync提供前と継続運用 |
| 3 threads + 2 connections/userの増加 | single userならいいえ。無制限multi-user受付は禁止。 | 他学生の招待開始前に上限とload計測 |
| revision / single-flight / reportがprocess-local | 1 processならいいえ。複数instance提供にはblocker。 | scale-out前 |
| 大学の許容範囲・credential預託方針未確認 | 大学通信なしならいいえ。一般学生への提供にはblocker。 | Web-6の一般提供判定前 |
| offline cacheの漏洩 / logout / shared device | shellだけのPoCならいいえ。offline user data有効化にはblocker。 | Web-5 |
| 永続volume / key recovery / restore未検証 | localhost syntheticならいいえ。公開運用にはblocker。 | Web-6 |
| 学期フィルター未実装 | いいえ。移行と別のproduct backlogとして残す。 | 別途Phase 11Aを依頼されたとき |

大学hostへの到達可否、規約・許可、実credentialの妥当性、実機PWA挙動は今回未検証。本レビューの外部調査はAxum/SQLite/OWASP/Browser公式資料に限定し、大学実serverへアクセスしていない。

## 13. Phase-by-phase migration plan

以下のpathは**将来の変更案**。今回作成した実装fileではない。各Phaseは独立reviewできる小さなPRに分ける。public deployment gateはWeb-6に置き、それまでの未認証開発環境はloopbackかつsynthetic限定。Web-3以前に本番credentialを入力する運用はしない。

### Web-1: Rust HTTP API local PoC

| 項目 | 計画 |
| --- | --- |
| 目的 | 大学通信・実credentialなしでBrowser/HTTP→shared application→SQLite→safe JSONの経路を立証。実装範囲は一覧GETとhealthのみ。 |
| 変更予定file | crates/application/src/runtime.rs、local_worker.rsへexplicit clockの追加口。runtime_tests.rsにそのcontract test。README.md / application READMEに加算APIの説明。root core / frontend / desktop / bridgeは変更しない。 |
| 新規file | crates/web-server/Cargo.toml / Cargo.lock、src/main.rs / lib.rs / state.rs / routes.rs / requests.rs / dto.rs / error.rs / poc.rs、tests/http_poc.rs。最小moduleへの統合は可。 |
| 再利用 | ApplicationHandle::open_with_backend、AssignmentQuery、QueryClock、AssignmentListResponse、SqliteStore::create_course / create_manual_assignment / set_completion。 |
| 主要risk | Send/Sync、worker初期化失敗、OS timezone混入、DTOからsecretが漏れること、誤った外部bindや本番DB選択。 |
| automated tests | router JSON/enum/unknown query、default/filter/sort、Course identity、DateOnly/UTC、synthetic marker非漏洩、clock注入、startup失敗、queue busy mapping、shutdown/reopen。詳細は14節。 |
| manual smoke | 127.0.0.1にのみlisten、Browser/curlでsynthetic JSON、filter違い、終了・再open、外部interface未listenの確認。大学通信なし。 |
| completion criteria | synthetic DBから正しいJSONが返り、worker cleanup、時計、safe error、loopback制約をtestで示す。既存core/application regressionが通る。Web-2や大学syncを同じPRへ入れない。 |

### Web-2: Browser frontend connection（引き続きsynthetic / loopback）

| 項目 | 計画 |
| --- | --- |
| 目的 | 既存UIでHTTP一覧/filter/sortと明示completion PUTを利用する。transportの差を分離する。 |
| 変更予定file | frontend/app.js / settings_view.js / presentation.js / sync.js、application runtime.rs / local_worker.rsへset_completionの加算API、Web routes/DTO/error。frontend/index.htmlはBrowser用entryの最小対応。 |
| 新規file | frontend/api.js / tauri_api.js / http_api.js / web_app.js、Web completion handler、HTTP adapter tests / Browser smoke tests。配信用static route設定。 |
| 再利用 | presentation.js、createListTaskRunner、createSettingsModelの注入構造、core SqliteStore::set_completion。 |
| 主要risk | network response喪失を未commitと誤表示、二重toggle、古いqueryの上書き、Tauri用entryの破壊、timezone表示不一致。 |
| automated tests | 同じPUT複数回でも同状態/updated_at不変、lost response→再query、対立操作の順序、filter維持、HTTP 401/503等のsafe mapping、従来Node testsとTauri DTO互換。 |
| manual smoke | Browserで全filter、長いtitle、完了操作→refresh failure→復旧、複数tab再query。Tauri版で既存invoke経路と設定表示を確認。 |
| completion criteria | synthetic dataで一覧/完了がHTTP経由で一貫し、HTTP toggle endpointがなく、Tauriの既存機能を維持。secret settings / real syncはまだ有効化しない。 |

### Web-3A: Web login / authorization

| 項目 | 計画 |
| --- | --- |
| 目的 | Web自身のログインとowner解決を実装する。single-userでも許可account以外を拒否し、synthetic A/Bの隔離を証明。 |
| 変更予定file | Web state/routes/config、Browser login/session表示、http_api.jsのCSRF/401対応、起動modeをPoCとauthenticatedに分離。 |
| 新規file | crates/web-server/src/auth.rs / session.rs / tenant.rs / control_store.rs / config.rs、独立Web metadata migration、auth/tenant tests。 |
| 再利用 | user runtime内のApplicationHandleと専用SQLite。既存domain/schemaへauth fieldを追加しない。 |
| 主要risk | issuer/subject mapping誤り、proxy header偽造、login CSRF、session固定、global stateへのuser混入、duplicate runtime open。 |
| automated tests | 未認証401、許可外403、AがBのassignment/Course/runへ到達不可、concurrent初回open共有、session rotation/expiry/logout、CSRFとOrigin rejection、production auth bypass拒否。 |
| manual smoke | PCとphone相当の別Browserで同account、許可外account、logout、期限切れ、複数tab。まだsynthetic dataとmock identity/provider test環境を使う。 |
| completion criteria | 全data endpointに共通認証/owner境界があり、same-origin cookie/CSRF設計をtest。公開はまだしない。 |

### Web-3B: Encrypted credential storage

| 項目 | 計画 |
| --- | --- |
| 目的 | Web用SettingsStore / CredentialStoreを追加し、大学secretを安全に保存・削除できるcontractをsynthetic値で立証。 |
| 変更予定file | Web settings handlers / config / control metadata、frontend/index.htmlのWeb保管説明・settings entry、settings.jsのsafe error。 |
| 新規file | crates/web-server/src/credential_store.rs / key_provider.rs / settings_store.rs、crypto roundtrip/rotation/failure tests、key/backup運用草案。 |
| 再利用 | SecretText / WebClassCredentials / CredentialStore / SettingsStore / SettingsStatus、SettingsServiceのvalidationとrevision。OS credential adapterは保持。 |
| 主要risk | 平文DB/log、nonce再利用、ciphertext owner差替え、keyとDBの同時流出、失敗で旧record消失、復号不能を未設定扱い。 |
| automated tests | DB bytesにsynthetic token/ID/passwordがない、誤key/AAD改変失敗、atomic replacement failure、missing delete成功、safe response、save/delete成功だけrevision進行、key rotation/restore。 |
| manual smoke | synthetic値だけでsave/replace/remove/restart、status flags、入力clear、Browser storage/log非漏洩、key欠落時に既存課題readを維持。 |
| completion criteria | saved secret GETなし、暗号化・鍵分離・失敗時保持・再認証要件を確認し、次Phaseでも実secretを無断使用しない。 |

### Web-4: Moodle / WebClass sync integration

| 項目 | 計画 |
| --- | --- |
| 目的 | 認証済みuser runtimeから既存core syncを使い、Web job/statusとserver outbound policyを接続する。 |
| 変更予定file | Web sync handlers/state/status DTO、frontend/http_api.js / sync.js、src/moodle_http.rs / webclass_http.rs / webclass_auth.rsへoptional server policy・bounded body処理を小さく追加、application sync_service.rs / runtime.rsのtrusted policy注入口を必要分だけ加算。既存desktop defaultは維持。 |
| 新規file | crates/web-server/src/sync_jobs.rs / egress.rs、SSRF/DNS/timeout/job ownership tests、approved-origin設定、実host適合性の記録。 |
| 再利用 | sync_moodle_once / sync_webclass_all_with_reauth、SyncReport / should_reload、既存source identity・completion保持・revision・session reuse。SyncBackend差替えはsynthetic testに使用。 |
| 主要risk | SSRF、hosting SSO失敗、無限login、credentials変更とのrace、job切断、partial commit誤表示、全体network枯渇。 |
| automated tests | loopback mockで各source成功/失敗、partial commit、expired→login→retry once、2回目expiry停止、same user single-flight、別user隔離、response切断後のrun継続、Idempotency-Key、cooldown、DNS rebinding/非公開宛先/redirect/body上限、secret-safe logs。 |
| manual smoke | mockのBrowser→202→poll→requeryを先に確認。実大学smokeは別途利用者が明示的に許可・実施し、対象hosting networkでSSO/Moodle成功・失敗時fallbackを確認。失敗ならWebClass提供を止める。 |
| completion criteria | server egress対策、job/revision/partial semanticsが全てtestされ、hosting実接続を「未確認/成功/失敗」と記録。実接続未確認のままWebClass本番対応済みと表示しない。 |

HTTP policyの追加は既存parser/import semanticsの変更と分けてreviewする。任意university URLやunsafe network modeをfrontendへ公開しない。public SyncBackendは現在synthetic test seamと明記されているため、その存在だけを根拠にproduction policy注入が実装済みとはしない。

### Web-5: Phone layout / PWA

| 項目 | 計画 |
| --- | --- |
| 目的 | phoneで一覧・完了を使いやすくし、Home Screen起動と限定offline readを段階導入。 |
| 変更予定file | frontend/styles.css / index.html / presentation.js / app.js / http_api.js、Web static cache/CSP headers。Tauri packageでSW登録をしない。 |
| 新規file | frontend/manifest.webmanifest / service-worker.js / pwa.js / offline_store.js / icons、PWA/update/logout cache tests。 |
| 再利用 | 課題safe DTO、DateOnly表示、Course grouping、error表示。serverのfilter/sort/clockを正本にする。 |
| 主要risk | 他account snapshot混入、期限切れbadgeの過信、cache eviction、更新途中のUI/API不整合、offline write競合。 |
| automated tests | app shell cacheのみ、secret/auth/settings route非cache、owner/timezone/query/version別snapshot、logout全tab purge、update waiting、offline read-only、古いsnapshotの鮮度表示、viewport/accessibility smoke。 |
| manual smoke | iPhone Safari/Home Screen、Android Chrome/Home Screen、Windows/macOS Browserでresponsive/タッチ/zoom/再起動/機内mode/更新。実機未実施なら対象の確認済み表記をしない。 |
| completion criteria | online task確認・completion、Home Screen導線、offline safe snapshotが端末別に検証され、通知/常駐/背景syncの保証をしない。 |

Web-5はsynthetic HTTPS test環境でも進められる。実データを外部phoneから使う環境の公開可否はWeb-6で判定する。

### Web-6: Deployment / operational hardening

| 項目 | 計画 |
| --- | --- |
| 目的 | PCの常時起動に依存せず外出先phoneから安全に利用できる単一user MVPを運用する。 |
| 変更予定file | Web config/readiness/graceful shutdown、README、dependency lock更新、必要なsecurity header / rate / log設定。 |
| 新規file | deploy/のcontainerまたはservice定義・TLS proxy設定例、docs/WEB-OPERATIONS.md、backup/restore/run interruption/credential incident手順、synthetic deployment checks。 |
| 再利用 | single replicaのuser runtime、SQLite v3、encrypted vault、既存core/application/test資産。 |
| 主要risk | ephemeral DB、鍵紛失、公開auth bypass、proxy誤設定、強制終了時job誤表示、SSO egress制約、backupが復元不能、第三者credential預託。 |
| automated tests | production config fail-closed、bind/proxy/header/CSRF、cross-user access、volume再起動保持、backup restore、key欠落/rotation、drain/途中kill復旧、synthetic負荷でqueue/rate上限、secret leak scan。 |
| manual smoke | 携帯回線からHTTPS login→同じ課題/完了をPCと確認、server restart、証明書/monitoring、隔離restore、許可外account拒否、利用者主導大学sync。 |
| completion criteria | HTTPS/auth/isolation/SSRF/永続化/鍵/restore/運用監視がPASS。大学条件・預託方針を確認し、単一userを明示。公開範囲を利用者が承認した時だけdeployする。 |

他学生対応は別のWeb-Multi phaseとし、まずuserごとのA方式・招待制・active runtime上限を導入する。実測がAの限界を示すか、複数instance運用が必要ならB方式/PostgreSQLへ進む。Phase 11A学期、submission state、timetable、WidgetKitをこれらの移行PRへ混在させない。

### 13.1 Regression gates

既存contractの参照として、次のtest実装も読んだ。

| 資産 | 読んだ具体的test / 保持する保証 |
| --- | --- |
| [persistence tests](../src/persistence.rs) | completion_local_update_changes_only_completion_and_timestamp_and_survives_import、imported_assignments_and_local_completion_survive_file_reopen、populated_v2_migrates_all_origins_without_changing_domain_or_observation_and_reopens、v2_rebuild_copy_and_late_ddl_failures_restore_schema_rows_and_version。 |
| [core auth / sync tests](../src/webclass_auth_tests.rs) | full_flow_without_consent_reaches_courses_and_returns_the_same_authenticated_session、untrusted_destinations_stop_before_any_outbound_request、contents_expiry_keeps_commits_and_full_retry_returns_only_final_attempt_counts、second_expiry_returns_sync_category_without_another_login_or_acquisition。localhost mockであり大学本番の証明ではない。 |
| [application runtime_tests](../crates/application/src/runtime_tests.rs) | single_flight_network_wait_keeps_query_toggle_available_and_completion_survives_sync_restart、lost_reply_keeps_single_flight_and_shutdown_drains_before_cleanup、startup_policy_is_once_across_clones_including_off_unconfigured_and_later_settings_changes。 |
| [application sync_tests](../crates/application/src/sync_tests.rs) | source_failures_are_independent_and_committed_data_stays_visible、replacement_and_removal_invalidate_session_and_never_cache_moodle_url、stable_url_rotation_keeps_identity_and_local_completion_on_reimport_and_reopen。 |
| [frontend tests](../tests/frontend.test.mjs)、[settings tests](../tests/settings.test.mjs)、[sync tests](../tests/sync.test.mjs) | CourseId grouping、DateOnly表示、typed filters、saved secret非保持・入力clear、partial failureでもreload、startup ordering、active toggle後にsync refresh。 |
| [macos-bridge tests](../crates/macos-bridge/src/tests.rs)、[sync_tests](../crates/macos-bridge/src/sync_tests.rs) | commit_refresh_clones_shutdown_and_reopen_preserve_application_contract、bridge_partial_commit_url_rotation_completion_revision_and_missing_credentials、bridge_single_flight_is_not_queued_and_local_queries_remain_available。 |
| [Swift concurrency tests](../macos/AssignmentManager/Tests/AssignmentManagerKitTests/ConcurrencyTests.swift)、[sync tests](../macos/AssignmentManager/Tests/AssignmentManagerKitTests/SyncTests.swift) | admitted write非cancel、古いquery破棄、write ack後refresh失敗、local先行startup、partial結果表示、shutdown drain。BridgeSmokeのproxy lifetime/foreign-port smokeも維持。 |

今後の実装PRではroot / applicationのfmt、locked offline clippy all-targets all-features -D warnings、unit/doc testsを実行する。Tauri、bridge、frontendを壊さない確認は少なくとも各Phase完了時とshared public API変更時に行う。Mac固有のSwift/Xcode/Keychain manual smokeはMac実機で行い、Windows上のRust checkで代替したと記載しない。

```text
cargo fmt --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline
cargo test --doc --locked --offline

cargo fmt --manifest-path crates/application/Cargo.toml --check
cargo clippy --manifest-path crates/application/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/application/Cargo.toml --locked --offline
cargo test --doc --manifest-path crates/application/Cargo.toml --locked --offline

cargo test --manifest-path src-tauri/Cargo.toml --locked --offline
cargo build --manifest-path src-tauri/Cargo.toml --locked --offline --features custom-protocol
cargo test --manifest-path crates/macos-bridge/Cargo.toml --locked --offline
node --test tests/frontend.test.mjs tests/settings.test.mjs tests/sync.test.mjs
```

加えてTauri/bridgeそれぞれのfmt/clippy/doc testsを[READMEのChecks](../README.md)どおり実施。Macでは bash macos/BridgeSmoke/build.sh と bash macos/AssignmentManager/build.sh でbindingsを再生成しstrict Swift / Xcode buildを確認する。root移動・生成Swift直接修正・既存desktop UI削除・schema bumpをWeb追加の前提にしない。

このWeb-0は文書のみのためbuild/testは再実行していない。参照file/型/関数、plan整合、git diffの変更範囲を確認し、過去reportのPASSを今回の実測値として転記しない。

## 14. Detailed Web-1 implementation proposal

### 14.1 Scopeと起動の安全性

Web-1で立証する経路は **GET /api/v1/assignments → ApplicationHandle → local worker → synthetic SQLite v3 → Web DTO JSON**。認証なしのPoCは127.0.0.1のみbindし、任意--host / --bind、0.0.0.0、LAN IP、public reverse proxy、tunnelによる公開を提供しない。IPv6を追加する場合も::1に限定する。phoneからの外部アクセスはcompletion criteriaに含めない。

loopbackだけでも悪意あるWeb pageからの呼出しやDNS rebindingを考慮し、Hostを固定loopback host/portへ制限、CORS許可なし、Origin付きcross-origin requestを拒否する。Web-1はGETだけでstate mutation routeを作らない。開発者自身のBrowser/curlによるJSON確認を最小smokeにする。

DBはPoC専用の新規temporary directoryへ作る。継続reopen smokeが必要ならtarget/web-poc配下のmarker付き専用directoryを使用し、desktop保存先や既存の任意DBを選ぶAPIを作らない。初回だけseedし、再起動でseedがcompletion等をresetしない。既存fileの上書きやsilent deleteをしない。

### 14.2 Manifestとmodule案

```text
crates/web-server/
  Cargo.toml               独立binary/lib、pathでapplication依存
  Cargo.lock               このcrateのlocked build
  src/main.rs              loopback bind、lifecycle、Ctrl+C
  src/lib.rs               router生成（integration testから再利用）
  src/state.rs             PocState（handle、server clock、safe metadata）
  src/routes.rs            assignments GET、healthのみ
  src/requests.rs          厳格なWeb query DTO→AssignmentQuery
  src/dto.rs               safe application read model→Web JSON
  src/error.rs             固定code→HTTP status
  src/poc.rs               fixture、memory settings、NoCredentials、DisabledNetworkBackend
  tests/http_poc.rs        synthetic DBを使うrouter/lifecycle tests
```

予定依存はAxum、Tokio（HTTP runtime/net/signal等の必要features）、serde/serde_json、chrono、既存applicationのpath dependency。fixture作成にroot coreをpath参照し、必要ならtempfileを使う。testのrouter ServiceExt等はその時点のofficial APIに合わせる。versionを本書で未検証の数字へ固定せず、導入時に互換性・lock・offline再現を確認する。root Cargo.tomlや既存lockfileを全面書換えない。今回dependencyは追加しない。

### 14.3 処理の順序

1. trusted PoC directoryを新規作成し、SqliteStore::openでv3 DBを生成する。create_course / create_manual_assignment / set_completionでfixtureを作り、seed用Connectionをcloseする。root coreにfixture専用production APIを増やさない。
2. memory SettingsStore（startup=false）、NoCredentials（両sourceともmissing）、DisabledNetworkBackend（呼出されても外部通信しない）を用いApplicationHandle::open_with_backendで1つのruntimeをopenする。これは既存のsynthetic test seamの利用である。
3. shared applicationへquery_assignments_with_clockの加算APIを作る。local workerのList requestはqueryとoptional explicit QueryClockを保持し、既存queryではNone→従来Local::now、WebではSome→そのclockでlist_from_storeを実行する。既存public methodのsignature、queue容量、snapshot構築は変更しない。
4. server clockが一度sampleしたUTC instantを+09:00へ投影してQueryClockを作る。Web handlerはevaluatedAt/timeZoneを同じsampleから作り、queryと一緒に送る。Browserから時計情報を受けない。testでは固定UTC clockを注入する。
5. listen前に初回queryをawaitしてlocal worker初期化成功を確認する。ApplicationHandle::openの成功だけをreadiness成功にしない。失敗なら安全なerrorで終了し、作成済みworkerをshutdownする。
6. routerにPocStateを渡し、GET query DTO→application→Web JSONを実行する。raw SqliteStoreやHTTP clientをhandlerへ渡さず、await中にglobal mutexを持たない。
7. Ctrl+C時にHTTP admissionを閉じ、in-flight handlerとapplication accepted workをdrainし、handle.shutdown().await後に終了する。自動startup sync / settings write / 大学HTTPを呼ばない。

将来の本番AuthenticatedUserを偽装する「user IDをqueryで受ける」近道は作らない。PoCのPocStateと本番tenant registryを型・起動modeで分け、PoC auth bypassを本番routerに残さない。

### 14.4 Synthetic fixtureとtest matrix

fixtureは同名で異なるIDのMoodle/WebClass相当Course、unassigned、DateOnly、UTC DateTime（小数秒含む）、deadlineなし、Completed/Incompleteを含む。token/URLを実在大学から持ち込まず、private submission URL等のsynthetic markerでread modelの非漏洩を検証する。

| 検証 | 期待値 |
| --- | --- |
| GET defaults | Incomplete、全Course/期限、既存core deadline順。response全itemsのorder / totalCount / coursesが期待値と一致。 |
| 全filter / sort | all/completed/unassigned/exact Course ID/overdue/today/within_seven_days、deadline/course/titleをcoreと一致。unknown enum/field/duplicate queryを400。 |
| snapshot | totalCountとCourse候補はfilter前、item/badgeは同じDB snapshot/clock。Course同名mergeなし。 |
| date boundaries | JST日付境界前後、DateOnly当日、nowと同じinstant、168時間端点、7日後DateOnly、completedのbadge除外。server OSがUTCでも固定clock期待値を通す。 |
| DTO | deadline kind維持、UTC Z/小数秒保持、web_class tag、hasSubmissionUrlのみ。submission URL / external key / DB path / secret markerを返さない。 |
| failures | malformed query400、DB初期化失敗はlisten前失敗、queue Busy503、Unavailable503、storage500。bodyにraw input/SQL/causeなし。 |
| concurrency | 複数GETを同じhandleで処理し、request単位openやConnection共有漏れなし。既存runtime single-flight/lifetime testsも通過。 |
| lifecycle | response receiver破棄でもresource破損なし。shutdown後新規要求拒否、drain後reopenしてseedデータ/事前設定completion保持。 |
| network / public bind | DisabledNetworkBackendしかconstructせず、startup/sync routeなし、portのlocal_addrがloopback、non-loopback configを拒否。Host/Origin制限とCORS非許可をtest。 |

queue fullを再現するtestは既存のsynthetic worker制御またはtest-only admission seamを使い、timing任せのflakyなsleep大量投入にしない。変更したshared query seamのtestは既存runtime_testsへ追加し、Web crateだけのmockでapplication経路を省略しない。

### 14.5 実装時の確認手順と終了線

Web-1 PR時の予定コマンド:

```text
cargo fmt --manifest-path crates/web-server/Cargo.toml --check
cargo clippy --manifest-path crates/web-server/Cargo.toml --locked --offline --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/web-server/Cargo.toml --locked --offline
cargo run --manifest-path crates/web-server/Cargo.toml --locked --offline
```

初回dependency fetchが必要なら実装時に行い、以後locked/offlineで再現する。manualでは起動時に示したloopback URLからassignments JSONとfiltersを確認し、OSのlisten一覧でexternal interfaceが開いていないことを確かめる。Ctrl+C→reopenで同じsynthetic DBを読めること、初回だけseedされることを確認する。root/applicationに加え13.1節の既存consumer regressionを実行し、未実施のMac実機確認を明示する。

**Web-1の完了はlocal JSON PoCまで。** frontend接続、completion HTTP実装、Web認証、secret保存、大学sync、PWA、remote phone接続、deploymentへ自動的に進まない。そこで差分をreviewし、次Phaseを別途実施する。

## 15. Final recommendation

root core、Windows Tauri、macOS SwiftUI、UniFFI、Keychainを維持し、Webを追加のconsumerとして実装する。初期は同一originのvanilla frontend + Rust API、user専用ApplicationHandle/SQLite、single server processが合理的。clockと明示completion設定の加算APIでHTTPに必要な差を吸収し、source identity / positive observation / partial commit / completion保持を変更しない。

最も重要な公開前条件は、Webログインとowner検証、user-bound暗号化vault、SSRFを含むserver egress制御、永続volumeとrestore、hostingでの大学SSO適合性の確認である。大学credentialを第三者serverへ預ける責任を明示し、単一ユーザーを理由にこれらを省略しない。

**Ready for Web-1** — 指定baseの実ソースで、shared handleからsafe read modelを取得する経路、synthetic transport seam、SQLite v3、clock projectionが確認できた。必要なのは独立Web adapterと小さなclock注入口であり、全体再設計を先行させるblockerはない。判定対象は「大学通信なし・synthetic DB・loopback限定」のPoC開始であり、インターネット公開準備完了ではない。


## Web-4B1 implementation addendum (2026-10-22)

上記Web-0は当時の調査記録。Web-4B1で[大学HTTP transport policy](WEB-NETWORK-SECURITY.md)と
明示Web constructor、DNS-to-connect検証、bounded body読取を追加した。
[Web-4B1報告](PHASE-WEB-4B1.md)のMac/offline gateはReady for Web-4B2。
Moodle実host/pathは未設定でfail closed、通常Web serverはDisabledNetworkBackendのまま。
実大学接続・Web-4B2実装・公開運用へは進んでいない。
Web-4AのWindows build/smokeは未完了で、同phase最終判定はBLOCKEDを維持する。
