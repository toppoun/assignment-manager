# assignment-manager-macos-bridge

Phase 10AのUniFFI 0.32.2 transport adapter。
共有ApplicationHandleをopaque MacApplicationへ包み、safe DTO/typed errorsへ投影します。
Swift向けにshared application/coreの公開contractを変更しません。

詳細なarchitecture、公式仕様の確認、再現手順、generated file方針、Swift smokeは
[macos/BridgeSmoke](../../macos/BridgeSmoke/README.md)を参照してください。

settings/credential adaptersはprivate PoC専用です。Settings/Keychain/Sync APIはexportしません。
`examples/prepare-smoke.rs`は一時DB専用helperで、production libraryの公開APIではありません。
