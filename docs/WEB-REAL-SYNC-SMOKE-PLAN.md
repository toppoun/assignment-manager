# Real University Sync: manual smoke plan（未実行）

2026-10-22（Asia/Tokyo）。本手順は実大学接続の許可ではない。
現在の判定は[Web-4B3 Preflight](PHASE-WEB-4B3-PREFLIGHT.md)のとおり **BLOCKED**。
**今回、実大学HTTP/DNS・実credentialは使用していない。**
以下は前提を満たした後、別途の明示承認を受けたoperatorが行う手順である。
Windows Web-4Aのbuild/smoke未完了・BLOCKEDは独立して維持する。

## 0. 実行前に揃えるもの

| Gate | 必須のreview内容 / 未充足なら停止 |
| --- | --- |
| Moodle | 利用者の対象環境のexact hostnameとcalendar export path、HTTPS/443、CourseRelated選択。default.course-relatedの対象profileが既存保存データと同じか確認。token queryは資料へ出さない |
| WebClass | HOPPII / IdP・OpenAM / LMSの現行exact origins、SAML/CSSOのrole別遷移、学年度の適用範囲。2025 hostnameを2026年という理由だけで変更/継続しない |
| 大学条件 | 本人accountの利用権限、第三者backendでの認証/取得可否、許容頻度、MFA/WAF/利用条件。通常Browser用の案内だけでは確定しない |
| Runtime | 専用review済みRust entrypointに固定policy + SecuredCoreSyncBackendを注入。通常binaryはDisabled維持。URL/policy/CA/DNSをenv/CLI/HTTP/browser設定で切り替える仕組みを作らない |
| Storage | 通常Web/native DB・既存Keychainと異なるdirectory/owner/lock/marker。real用DBを明示初期化しsynthetic seedを入れない。PocDatabaseのSYNTHETIC-ONLY環境を転用しない |
| Key custody | Mac Web専用providerの明示init/load、missing/wrong key時停止、process restart保持、廃棄、DBと鍵を独立管理する運用を確認。MemoryKeyProviderの保持をprocess restartの代用にしない |
| Network | 実施hostのegress制限、resolver/DNS-to-connect、標準TLS/SNI/hostname検証、no_proxy、exact port/path/origin、Cookie origin分離。今回のfixture CA/DNSをreal runtimeへ持ち込まない |
| Resource | 180秒協調deadline、512 requests、128 MiB累積body、128 courses/256 groups、4,096 records/batch、各body/Cookie上限を確認。hard cancelではなく真の終了までdrainすることを了承 |
| Logs | access log/APM/HTTP debug/TLS key log/Browser HAR/request captureを無効またはreview済みredaction。URL query、Cookie、大学ID、password、SAML、HTML/JSON本文を収集しない |
| Approval | 使用runtime/policy revision、本人accountの内部alias、許可source、日時/終了時間、回数、停止担当、保持/削除方針を特定した人間の明示承認 |

上記gateが未完了の間は、接続確認目的のDNS lookup、curl、大学URLを開く操作も行わない。
専用runtimeの実接続コマンドは未実装なので、本書には実行可能なreal-connect commandを載せない。

## 1. Credentialの入力 / 記録境界

利用者が専用runtimeのlocal UIへ直接入力する。Codex chatへ貼らず、source/fixture/.env/CLI引数/
clipboard履歴/ログ/スクリーンショットに残さない。Browserの自動password保存や拡張によるcaptureも使わない。
Moodle export URLはquery全体がbearer secret。記録するのはoperatorが別途確認したhostnameとpathだけ。
WebClass ID/passwordは同じVault recordへatomicに保存。Web login/reauth passwordは大学passwordと分離。

同期中のcredential mutationは409を維持する。入力中に同期を並行開始しない。
実credentialの更新は利用者が行い、Codexによるpassword変更/credentialの読出しは行わない。
通常native/Windows desktopの保存credentialを移行元として自動読出ししない。

## 2. 全段階共通の停止条件

次のいずれかで新規同期受付を止める。失敗を繰り返して原因を推測しない。

| 検知 | 停止後の扱い |
| --- | --- |
| 未確認origin / path / port、想定外redirect | 固定category/stageのみ記録。新originをresponseから自動追加しない。policyを広げるには別review |
| TLS chain / hostname / SNI / certificate失敗 | 検証無効化、自己署名警告迂回、任意CA追加で再試行しない |
| DNS/SSRF制限違反、private/metadata/mixed answer | DNS guard/no_proxy/egress制限を緩めない。OS DNSへのfallbackやIP literal化をしない |
| login / MFA / consent / WAFの予期しない動作 | 自動操作を止める。CAPTCHA/MFA/規約画面を迂回しない。本人と大学側条件の確認へ戻る |
| body/time/request/course/group/batch/Cookie limit | 安全なcode/件数/所要時間だけを記録。上限引上げ・retry増加・圧縮許可は別review |
| Cookie origin separation不整合 / sibling共有要求 | IdP/SP/LMSのJarを共有しない。実Cookie値を調査ログへ出さない |
| secretのlog/APM/画面証跡流出 | 新規受付停止、capture停止、該当資料をアクセス制限。本人によるtoken/session/credential失効と事故対応方針へ。secretをchatへ転記しない |
| 大学利用条件・利用account権限との不整合 | 即停止。本人/大学側で可否が解決し新しい明示承認を得るまで再開しない |
| DB/key/binding不整合、wrong/missing key | reset/上書き/平文fallbackをせず停止。元DB/鍵を保全し、復旧を別review |
| run結果不明 / worker喪失 / 強制終了 | 成功や未保存と断定しない。commit済み一覧を確認し、旧run/idempotencyを成功として復元しない |

停止はadmission停止→HTTP drain→job/Settings drain→application shutdown→auth/control close。
180秒は協調的期限なので処理中parser/SQLite/OS provider/DNSを強制cancelできる保証はない。
期限後にworkerをdetachして新しいjobを許可しない。
外部supervisorが強制終了した場合はpartial commitと結果不明を記録する。

## 3. 検証順序と各段階のgate

各段階を1回ずつ確認してから次へ進む。共通停止条件は全段階に適用する。
Web UIがfinishedでもsourceごとのsuccessを確認する。部分失敗でもshouldReloadはtrueになり得る。

| 順序 | 手動操作 | 期待する証拠 | この段階の停止条件 |
| --- | --- | --- | --- |
| 1 Moodle単独 | WebClass未設定をboolean statusで確認。本人が承認済みCourseRelated exportを保存し、1回同期 | Moodle success/counts、WebClass not_configured、一覧更新。source identityがdefault.course-relatedに属することをsecretなしの検証で確認 | exact host/path/export契約不一致、redirect/TLS/DNS/limit、誤った対象profile。export kindをqueryから推測して続行しない |
| 2 WebClass単独 | drain後Moodle設定を削除して未設定確認。本人が合成環境とは別の隔離Vaultへ大学credentialを入力し1回同期 | Moodle not_configured、SSO/consent/SAML/CSSO/courses検証成功、WebClass success、一覧更新 | 未確認IdP/LMS/role遷移、MFA/WAF/challenge、Cookie共有要求、認証失敗。反復loginしない |
| 3 両source | 本人がMoodleを再設定し両boolean=trueを確認、1回同期 | 両source success。先行sourceのcommitを後行sourceの失敗で消さない。件数はAPI reportと一覧で突合 | 一方失敗ならここで停止しpartial保存の事実だけ記録。両source成功と報告しない |
| 4 再同期 | 同じcredential/policyのまま、状態確認後に明示再同期 | stable identity、duplicate増加なし、通常はcreated0。cached WebClass session reuse（safe countで確認可能な場合）。大学側更新があればupdatedは許容 | 不明なduplicate/scope変更、毎回SSO、複数loginやreauth loop、結果不明。別key連打しない |
| 5 完了保持 | 既知課題1件をUIでcompletedにしcommit応答確認、再同期 | 同じIDのcompletionを保持、filter/sortを維持して一覧再取得。大学側状態でlocal completionを上書きしない | 完了が戻る、ID変更、一覧と保存結果が不一致。以後のwriteを止め保全 |
| 6 credential変更後session更新 | active jobなしを確認。本人が正当な更新済みcredentialをlocal UIで保存（同じ値の再保存はrevision試験として別記録） | persist成功後revision更新、旧Jar破棄、次同期はfresh SSO。新credential失敗でも旧sessionを再利用しない | 保存409/busy、保存失敗、旧session再利用、実認証失敗/MFA。大学passwordを故意に誤らせて試験しない |
| 7 部分失敗・再取得 | 実大学で障害を起こさない。自然な一方失敗が発生した場合だけ停止・一覧のpartial commitを確認。原因解消と再試行範囲の確認後、承認範囲内で1回再取得 | 成功sourceのcommit/completion保持、失敗sourceは固定code、再取得でduplicateなし。AuthExpiredが自然発生した場合だけreauth once | 未解決障害、2回目AuthExpired、rate/limit、予期しないlogin。自然失敗がなければ実環境は未観測としsynthetic証拠を参照 |
| 8 server restart | admission停止→drain→全close、同じ隔離DBと承認済み永続providerでprocessを終了/起動。本人がWeb再login | 旧Cookie401、旧run404/lastRun=null、課題/completion/暗号化credential保持、大学sessionはmemoryなので新SSO、再同期duplicateなし | key/binding/DB不整合、key消失、旧sessionが有効、データ消失。新鍵で初期化し直さない |

Sourceの除去/再保存は大学側のcredentialを変更する操作ではない。
server restartとrun履歴/idempotency保持を混同しない。履歴はmemoryだけで、restart前runは復元しない。
step 7の危険な障害注入、2回expiry、HTTP切断、Settings409、shutdown途中のdrainは
[synthetic自動tests](PHASE-WEB-4B3-PREFLIGHT.md#6-required-scenario-evidence--security-boundaries)を使用する。

## 4. 安全な観測項目 / 結果票

記録してよいもの: JST開始/終了、runtime/policy revision、stage番号、source status、固定failure code/
AuthStep/category、created/updated/unchanged、total件数、completion保持の可否、合否/停止理由。
対象課題やaccountは実IDやtitleではなく、検証者が持つ内部aliasを使う。

HTTP request数/body bytes/course/group/Cookie数等は現在のprivate budgetからUIへ公開していない。
実smokeで測定が必要なら、secretを持たないbounded aggregateだけを出す計測を先に別reviewする。
計測のためにURL/headers/body/reqwest error chainをdumpしない。
利用者のBrowser devtools/network HARやログのuploadは求めない。

結果票の項目:

```text
実施JST / runtime・policy revision / 承認記録への参照（secretなし）
段階 / source status・counts / 一覧total / completion保持 / session再利用または再loginの判定方法
所要時間 / 固定code・stage / PASS・FAIL・未観測 / 停止理由
DB/鍵の保持・廃棄確認 / Windows未完了gate / 次段階への承認範囲
```

許可originや上限の変更が必要なら、secretなしの根拠をreviewしruntimeを作り直す。
失敗時の自動緩和は禁止。既存処理内のAuthExpired reauth once以外の自動再試行を追加しない。

## 5. 終了 / 再開

正常時も新規受付停止→全drain→runtime破棄。Web browser session、cached大学session/Cookieは破棄。
DBとWeb鍵の保持/削除は事前に合意した方針で別々に扱う。正常終了が鍵のbackup/restore成功を意味しない。
key rotation/re-encryptionや移設は本smokeの操作に含めない。
失敗時は停止条件とcommitの確定範囲を記録し、原因解決・policy/runtimeの再review・必要な新承認後に再開する。

**実接続の明示承認があるまで、この手順を実行しない。**
