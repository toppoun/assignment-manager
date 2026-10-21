# Web Credential Vault (Web-3B)

対象はWindows local開発、単一Web account、合成credential、127.0.0.1:3000。
大学HTTP、実同期、公開運用を含まない。

## Responsibility / ownership

```text
Browser web_settings.js → authenticated routes → TenantRuntime.for_user(UserId)
  → ApplicationHandle → SettingsWorker → shared SettingsService
  → WebVault(owner, SharedControl, KeyProvider) → control SQLite v2
                                           ↘ Windows Credential Manager (key only)
```

Settings validation、Moodle identity、WebClass revisionは既存applicationに置く。
WebVaultはplatform CredentialStore adapterであり、HTTP handlerからSQLを直接実行しない。
shared control connectionをMutexで直列化し、blocking操作をsettings worker / auth blocking poolで行う。
大学IDは暗号化payload内だけにあり、UserId、DB key、OS metadataに使用しない。
MoodleとWebClassはowner/record-kindごとに1行。WebClass ID/passwordは同じpayloadでatomicに置換する。

## Cryptography / maintenance decision

RustCrypto [AES-GCM 0.11.1 API](https://docs.rs/aes-gcm/0.11.1/aes_gcm/)と
[upstream changelog](https://docs.rs/crate/aes-gcm/0.11.1/source/CHANGELOG.md)、
[maintenance repository](https://github.com/RustCrypto/AEADs)を確認して採用した。
0.11系のAPI/依存と本repositoryのRust 1.97.1でのbuildを確認し、web-serverのlockだけを更新した。
`aes`, `alloc`, `zeroize` featuresを有効にする。独自cipher/hash/MACは実装していない。
公式のconstant-time CPU条件を踏まえ、今回の検証対象はWindows x86_64 MSVC。

| Field | Format v1 |
| --- | --- |
| version | 1 byte: 1 |
| key ID | 16 random bytes、公開metadata |
| nonce | OS CSPRNG 12 bytes |
| ciphertext | AES-256-GCM encrypted payload |
| authentication tag | 16 bytes、ciphertext末尾（libraryが生成・検証） |

復号前にversion/key ID/最小・最大長を検証し、tag検証失敗ではplaintextを返さない。
暗号化・復号bufferとraw keyはZeroizingで所有し、cipher内部のkey stateにもlibraryのzeroize機能を使う。
SecretText、VaultKey、入力型、WebClassCredentialsにDebug/Clone/Serializeを追加していない。
HTTP/JSON/OS/Browser/allocator内部の全secret copyの完全消去を保証するものではない。

AADは次の順のbinaryで、曖昧な文字列結合を避ける。

1. 固定domain `assignment-manager.web.vault\0`
2. format version（1 byte）
3. key ID（16 bytes）
4. internal UserIdのbyte長（8-byte big endian）
5. internal UserId bytes
6. record-kindの固定ASCII（moodle / webclass / binding）

logical record identityは(owner, kind)である。別owner・別kindへのciphertext丸ごと差し替えはtag不一致となる。
version/key ID改ざん・未知version/IDも拒否する。同じowner/kindの旧recordへのrollback攻撃を検出する
monotonic外部counterはない。DBを書換えられるlocal OS攻撃者やbackupのrollbackを完全に防ぐ設計ではない。

Moodle payloadはURLのUTF-8。WebClass payloadはID byte長（4-byte big endian）、ID UTF-8、password UTF-8。
復号後も構造・長さ・shared validationを確認し、partial/invalid payloadを「未設定」にしない。
Moodle scopeはdefault.course-related、export kindはCourseRelatedのまま、URLをidentityに使用しない。

## Nonce lifecycle

nonceはrecord writeごとにCSPRNGで生成する。`vault_nonces(key_id, nonce)`にunique予約してから暗号化し、
record置換と同じIMMEDIATE transactionでcommitする。衝突時は最大4回再生成し、失敗は旧recordを保持する。
履歴はcredential置換・削除でも消さず、restartでも保持する。keyあたり100万nonceを上限とし、超過はfail closed。
暗号化に成功してもtransactionが失敗したciphertextは公開/acknowledgeしない。

key-check recordにも同じnonce履歴を使う。CSPRNG故障・nonce衝突・書込失敗は成功へ偽装しない。
DB snapshotのrollbackでnonce履歴も戻るため、将来のbackup/restoreでは鍵/nonce継続性の手順が必要。
現在はrestore CLIや復元保証を提供せず、単純copyでの公開運用を承認しない。

## KeyProvider / initialization

KeyProvider.loadは既存VaultKeyを取得するだけで、欠落時に生成しない。
VaultKey.from_partsにより、将来のoperator secret mount/KMS adapterへ置換可能。
Linux/VPSのproduction providerは未実装で、non-Windows executableはfail closed。
MemoryKeyProviderは明示注入する合成test seamで、executableのfallbackにはしない。

Windowsは既存desktopと同じmaintainedなkeyring-core / windows-native-keyring-store APIを使い、
[Local persistence](https://docs.rs/windows-native-keyring-store/1.1.0/windows_native_keyring_store/)を指定する。
serviceは`org.assignmentmanager.web.vault.keys.v1`、accountはcanonical installation control pathのSHA-256。
大学IDやUserIdはOS credential metadataへ入れない。Tauriの`io.assignment-manager.desktop`、
macOS nativeのKeychain serviceとは独立している。OS entryはkey format/version + key ID + 32-byte keyだけ。
Moodle/WebClassの実secretをOS entryへ直接保存しない。

`init-vault-key`だけが次を行う。

1. PoC directoryとcontrol DBを検証し、server/bootstrap共通のexclusive process lockを取得。
2. account存在を要求。binding/credential/nonceがあれば既存鍵をloadして検証するだけ。
3. 完全に未初期化の場合のみ、OS entryがmissingならCSPRNGで新規鍵を生成。
4. 既存OS entryは上書きせず再利用。破損entryは拒否。
5. control DBにowner束縛した暗号化key-check recordとnonceをtransactionで保存。

DBとOS storeを跨ぐdistributed transactionはない。OS保存後にDB commitが失敗した場合は孤立鍵が残り得る。
再度の**明示init**ではその既存鍵を再利用し、同じentryを再生成・上書きしない。
通常起動はbindingを必須とし、空Vaultでも鍵ID/実鍵一致を検証する。
鍵消失・wrong key・unknown key ID・corrupt bindingは停止し、credentialをsilent resetしない。
鍵をCLI arguments、env、source、DB、log、frontendへ出す経路はない。

鍵rotationは今回**metadata対応のみ**。key IDとformat versionはあるが、複数active key、re-encryption、
rotation CLI、旧鍵廃棄、鍵backup/restore手順は未実装。自動rotationはしない。
path由来installation identityはdirectory移動で変わる。restore/移設は将来の明示管理手順で扱う。
鍵とciphertextを同じbackupへ入れてはいけない。

## Persistence / migration / failure

assignment DBは既存schema v3。control DBだけをtransactionでv1→v2へ移行する。

| Added field/table | Responsibility |
| --- | --- |
| sessions.reauthenticated_at | session-bound recent proof、NULL default |
| vault_binding | singleton owner + encrypted known key-check payload |
| web_credentials | owner FK、kind CHECK、(owner,kind) primary key、envelope BLOB |
| vault_nonces | key ID/nonce length CHECK、(key ID,nonce) primary key |

accounts/verifier、sessions、login_attemptsをmigrationで削除・再作成しない。
server restart時のsession失効は既存AuthService policyとしてmigration後に実施する。
future version、partial schema、SQLite quick_check / foreign_key_check失敗は拒否する。
foreign nonempty v0やcorrupt recordをresetしない。control connectionはsynchronous=FULLを明示する。

replace/deleteは旧recordを復号・構造検証してから同じtransactionで変更する。
missingだけが正常な未設定で、missing deleteは冪等。corrupt recordは置換も削除もエラーにする。
commitに成功して初めてSettingsWorkerが応答し、HTTPは204を返す。失敗時は旧recordとnonce履歴を保持する。
WebClassのrevision更新は既存SettingsServiceが成功後だけ行う。実network sessionは一度も確立しない。
corrupt recordの修復は将来の明示的なoperator手順であり、現UIからは行えない。

## Deployment boundary

保存時のHTTPS/host/userinfo validationはSSRF対策ではない。保存を受理したURLは通信許可済みではない。
DisabledNetworkBackend、startup sync OFF、sync endpointなし、大学HTTP client非構築を維持する。
Web-4では大学別host/path/port allowlist、DNS/接続先制御、HTTP response body size上限、outbound policyが必要。
production HTTPS/Secure Cookie、reverse proxy trust、production key provider/ACL、鍵とDBを分離したbackup/restore、
大学認証の実検証とcredential取扱方針も公開前の必須作業。今回のVault完成は公開許可ではない。
