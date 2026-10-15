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
# /model で切り替えられるモデル(カンマ区切り、AI_MODEL は自動で含まれる)
AI_MODELS=openai/gpt-5.6-luna,anthropic/claude-sonnet-5.5
# テスト用: 指定したサーバーにだけスラッシュコマンドを登録する(即時反映)
GUILD_ID=123456789012345678
```

※ systemd の `EnvironmentFile` は行末コメントに対応していないため、コメントは必ず別の行に書いてください。

Discord Developer Portal で **Message Content Intent** を有効にしてください。
スラッシュコマンドが出てこない場合は、OAuth2 の招待URLで `bot` と `applications.commands` の両方にチェックを入れてボットを招待し直してください。

```sh
cd ai-discord-bot
cargo run --release
```

### 使い方

| コマンド | 説明 |
| --- | --- |
| `/ai question:<質問> image:<画像>` | AI に質問する(画像は任意) |
| `/reset` | そのチャンネルの会話履歴を消す |
| `/model [name]` | 使用中のモデルを表示 / そのチャンネルのモデルを切り替え |
| `/help` | ヘルプを表示 |

従来どおり `!ai <質問>` や `@ボット <質問>` でも使えます(`!ai reset` / `!ai model [名前]` / `!ai help` も可)。
メッセージに画像を添付すると、画像の内容も読み取って答えます。画像を読むには画像対応のモデルが必要です。

質問に URL を書くと、そのページを取得して内容を踏まえて答えます(1回の質問につき3件まで)。
例: `!ai https://example.com/news を3行で要約して`

- 画像の URL は画像として読み取ります
- ページ本文は1件あたり8000文字まで AI に渡します
- サーバー内部や LAN 内のアドレス(`127.0.0.1` や `192.168.x.x` など)にはアクセスしません
- ログインが必要なページや、JavaScript で中身を表示するページは読めないことがあります

会話履歴はチャンネルごとに保持され、ボットを再起動すると消えます。
