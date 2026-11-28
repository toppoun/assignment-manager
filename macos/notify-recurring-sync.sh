#!/bin/sh
set -eu

case "$1" in
    network) description='通信障害' ;;
    authentication) description='認証・権限エラー' ;;
    write_blocked) description='書き込み停止' ;;
    service) description='サービス側の一時障害' ;;
    response) description='応答の異常' ;;
    *) description='定期同期の失敗' ;;
esac

exec /usr/bin/osascript - "$description" "$2" <<'APPLESCRIPT'
on run argv
    display notification ((item 1 of argv) & "（連続" & (item 2 of argv) & "回）") with title "課題同期"
end run
APPLESCRIPT
