#!/usr/bin/env bash
# ai-discord-bot を最新にしてサービスを再起動する
#
# 使い方:
#   ./update.sh            今のブランチを最新にして再起動
#   ./update.sh main       main ブランチに切り替えて再起動
#   ./update.sh --release  リリースビルドで再起動(service の ExecStart も release を指すこと)
set -euo pipefail

SERVICE="ai-discord-bot"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOT_DIR="$REPO_DIR/ai-discord-bot"

BRANCH=""
BUILD_ARGS=()
for arg in "$@"; do
    case "$arg" in
        --release) BUILD_ARGS+=(--release) ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) BRANCH="$arg" ;;
    esac
done

cd "$REPO_DIR"

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "⚠️  コミットされていない変更があります。退避するかコミットしてから実行してください:"
    git status --short --untracked-files=no
    exit 1
fi

echo "==> コードを取得"
git fetch origin
if [ -n "$BRANCH" ]; then
    git checkout "$BRANCH"
fi
BEFORE="$(git rev-parse HEAD)"
git pull --ff-only origin "$(git rev-parse --abbrev-ref HEAD)"
AFTER="$(git rev-parse HEAD)"

if [ "$BEFORE" = "$AFTER" ]; then
    echo "    すでに最新です ($(git log -1 --format='%h %s'))"
else
    echo "    更新内容:"
    git log --format='    %h %s' "$BEFORE..$AFTER"
fi

echo "==> ビルド"
cd "$BOT_DIR"
# ビルドに失敗したら再起動せずに終わる(動いているボットはそのまま)
cargo build ${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}

echo "==> 再起動"
systemctl daemon-reload
systemctl restart "$SERVICE"
sleep 3

if systemctl is-active --quiet "$SERVICE"; then
    echo "✅ $SERVICE は起動しています"
else
    echo "❌ $SERVICE が起動していません"
fi
echo "---- 最近のログ ----"
journalctl -u "$SERVICE" -n 15 --no-pager
