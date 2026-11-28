# macOSでMoodleの定期同期を運用する

LaunchAgentは毎時00分に1回、固定された運用CLIの `sync recurring --apply` を実行します。
実行スクリプトは本番ローカル設定の `webclass: false` と `recurring_sync.webclass` の省略を確認し、
確認した設定を同一内容の一時ファイルとしてCLIに渡します。上限と承認の照合、Notion対象の検証、
排他制御、書き込みの復旧はCLIと既存のStateStoreが担います。
Moodle単独運用の設定は [READMEの定期同期手順](../README.md#macで本番の差分同期を1回実行する) を参照してください。

| Macの状態 | 挙動 |
| --- | --- |
| GUIでログイン中 | 毎時00分に起動します。登録時の即時実行や失敗直後の再起動は設定していません |
| スリープ | スリープ中の予定を復帰時にまとめて1回実行します。復帰時のVPN・キーチェーンの状態で同期の成否が決まります |
| 画面ロック | GUIセッションが続くため実行対象です。ログインキーチェーンへのアクセスが拒否された場合は非対話で停止します |
| ログアウト・電源断 | LaunchAgentは停止し、その間の実行は繰り越しません。次のGUIログイン後の予定から再開します。実行中にログアウトした場合、完了ログが残らないことがあります。未確定の作成は次の実行でStateStoreから照合します |

[Appleの時刻指定ジョブの資料](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/ScheduledJobs.html) ·
[AppleのLaunchAgentの資料](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)

## 登録前の準備

GUIでログインした運用ユーザーで手順を進めます。Xcode Command Line Toolsの `/usr/bin/python3`（3.9以上）を使います。
運用CLIは [固定バイナリの配置手順](../README.md#macの運用用cliと非対話取得) で配置したものを指定します。
同じCLIでMoodle・Notion Reader・Writerのキーチェーン読み取りを非対話で確認しておきます。
VPN接続は普段の方法で維持します。同期が失敗した場合は次の予定で再試行します。

リポジトリのルートから、実際の運用CLIの絶対パスを `CLI` に指定して実行します。
この段階ではリポジトリの `local/` に確認用plistができ、LaunchAgentには登録されません。

```sh
LABEL=io.assignment-manager.moodle-recurring
GUI="gui/$(id -u)"
SERVICE="$GUI/$LABEL"
REPO="$(pwd -P)"
CLI="$HOME/Library/Application Support/AssignmentManagerCLI/bin/<運用版のSHA-256>/assignment-manager"
CONFIG="$REPO/local/notion-production.json"
STATE_DIR="$HOME/Library/Application Support/AssignmentManagerCLI/recurring"
STAGED="$REPO/local/$LABEL.plist"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

umask 077
/usr/bin/codesign --verify "$CLI" &&
/usr/bin/python3 - "$REPO" "$CLI" "$CONFIG" "$STATE_DIR" "$STAGED" <<'PY'
import plistlib
from pathlib import Path
import sys

repo, cli, config, state_dir, destination = sys.argv[1:]
template = Path(repo) / "macos/io.assignment-manager.moodle-recurring.plist"
with template.open("rb") as file:
    data = plistlib.load(file)
for index, arg in enumerate(data["ProgramArguments"]):
    if arg == "__CLI__":
        data["ProgramArguments"][index] = cli
    elif arg == "__CONFIG__":
        data["ProgramArguments"][index] = config
    elif arg == "__STATE_DIR__":
        data["ProgramArguments"][index] = state_dir
    elif arg.startswith("__REPO__/"):
        data["ProgramArguments"][index] = repo + arg[len("__REPO__"):]
Path(destination).write_bytes(plistlib.dumps(data))
PY
/usr/bin/plutil -lint "$STAGED"
```

## 登録・状態確認・停止・再開・削除

登録は生成したplistの確認と承認後に行います。以降の例は上で定義した変数を使用します。

| 操作 | コマンドと確認 |
| --- | --- |
| 初回登録 | `mkdir -p "$HOME/Library/LaunchAgents"`、`install -m 0600 "$STAGED" "$PLIST"`、`launchctl enable "$SERVICE"`、`launchctl bootstrap "$GUI" "$PLIST"` |
| 状態確認 | `launchctl print "$SERVICE"` で稼働状態・直近の終了コードを確認し、`launchctl print-disabled "$GUI"` で有効状態を確認します |
| 一時停止 | 実行中なら完了を待ちます。`launchctl disable "$SERVICE"` の後に `launchctl bootout "$SERVICE"`。停止は再ログイン後も有効です |
| 再開 | `launchctl enable "$SERVICE"`、`launchctl bootstrap "$GUI" "$PLIST"` |
| 削除 | 一時停止したうえで `rm "$PLIST"`。運用CLI、ログ、同期履歴と書き込み承認は保持します |

`launchctl kickstart "$SERVICE"` は本番適用を即時に起動します。予定を待たずに動作確認するときだけ使用します。
LaunchAgentの停止と、`sync recurring --disable-writes` による書き込み承認の取り消しは独立しています。
運用を停止したい場合は登録を停止し、承認も取り消す場合はその後にCLIで取り消します。

## ログと通知

`$STATE_DIR/runs.jsonl` に1実行につき1行を記録します。開始・終了時刻は実行ホストのタイムゾーン付きです。
`moodle_candidates` は取得件数、`created` と `updated` は実際の適用結果です。
取得失敗や未適用は該当する件数が `null`、確認できた0件は `0` です。
`exit_code` はスクリプトの終了コード、`cli_exit_code` はCLIが返したコードです。
スクリプト自体が前提条件やCLI出力を拒否した場合、両者は異なることがあります。
JSONとして検証した固定診断コードと件数を記録し、設定値、認証情報、CLIと通知プログラムの生出力は保存しません。

代表例として `network` は接続・タイムアウト、`authentication` は認証の失効・権限不足・キーチェーンの拒否、
`write_blocked` は承認なし、変更上限、対象の不一致などによる停止です。
`service` と `response` はそれぞれサービス側の障害と応答の異常を表します。
`busy` は排他による見送りです。同じ実行スクリプトが先に起動している場合、後続はログを開かず終了コード4で終了します。

失敗、部分失敗、書き込み停止が合計で3回連続すると、macOSの通知センターに1回送信します。
`busy` は連続回数を変えません。1度成功すると連続回数をリセットし、次の連続障害で再び通知します。
通知の送信自体が失敗した場合はログに `notification: "failed"` を残し、同じ連続障害での再送は抑止します。
`submitted` は通知コマンドが成功したことを表し、画面表示は通知設定や集中モードに従います。
[Appleの通知スクリプト資料](https://developer.apple.com/library/archive/documentation/LanguagesUtilities/Conceptual/MacAutomationScriptingGuide/DisplayNotifications.html)

ログは256 KiBごとにローテーションし、現行と直近3世代を保持します。スクリプトがログを作成するディレクトリの権限は `0700` です。
確認には `tail -n 20 "$STATE_DIR/runs.jsonl"` を使います。ログがない状態で終了コードが `1` の場合は、
`launchctl print "$SERVICE"` と、運用ユーザーによる `STATE_DIR` の所有・書き込み権限を確認してください。

## 運用CLIの更新と切り戻し

Phase 2B以降の運用では、対象LaunchAgentの無効・未登録状態を維持し、運用CLIを手動で実行します。

1. 新版を別の固定パスに配置し、運用ユーザーでキーチェーンの非対話読み取りを再確認します。
2. 新版の絶対パスを指定し、承認済みの手動プレビューと適用を行います。
3. 切り戻すときは、同じ本番設定で旧版の絶対パスを指定します。

どちらの場合も同じ `STATE_DIR` と本番設定を使います。対象・ポリシー・スキーマに変更があればCLIが再承認を要求します。

## 模擬検証とLinux移行

模擬CLIと模擬通知を使うテストは外部サービスやLaunchAgent登録を伴いません。

```sh
/usr/bin/python3 -B -m unittest discover -s tests -p test_recurring_runner.py
/usr/bin/plutil -lint macos/io.assignment-manager.moodle-recurring.plist
/bin/sh -n macos/notify-recurring-sync.sh
```

実行スクリプトは1回の同期で終了し、スケジュールはplistが管理します。
Linuxの認証情報管理とLinux向け運用CLIが整った後は、同じ実行スクリプトをsystemdの `Type=oneshot` サービスから起動し、
対応するtimerで起動時刻を設定できます。通知コマンドは任意なので、Linux側の通知先に差し替えられます。
