# ai_chat

OpenRouter 経由で AI と会話するための Rust 製ツール集です。

- `ai-discord-bot` … Discord で AI と会話できるボット
- `ai-request-test` … ターミナルで AI と会話する CLI

## ai-discord-bot

### セットアップ

`ai-discord-bot/.env` を作成します。

```env
DISCORD_TOKEN=xxxxxxxx
API_KEY=sk-or-xxxxxxxx

# 以下は任意
# 使用するモデル
AI_MODEL=openai/gpt-5.6-luna
# AI への指示(キャラ付けなど)
SYSTEM_PROMPT=あなたは親切なアシスタントです。
# チャンネルごとに覚えておくメッセージ数
MAX_HISTORY=20
```

※ systemd の `EnvironmentFile` は行末コメントに対応していないため、コメントは必ず別の行に書いてください。

Discord Developer Portal で **Message Content Intent** を有効にしてください。

```sh
cd ai-discord-bot
cargo run --release
```

### 使い方

| コマンド | 説明 |
| --- | --- |
| `!ai <質問>` / `@ボット <質問>` | AI に質問する |
| `!ai reset` | そのチャンネルの会話履歴を消す |
| `!ai model` | 使用中のモデルを表示 |
| `!ai help` | ヘルプを表示 |

会話履歴はチャンネルごとに保持され、ボットを再起動すると消えます。
