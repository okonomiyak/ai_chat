use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::model::id::ChannelId;
use serenity::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const PREFIX: &str = "!ai";
const DISCORD_MAX_LEN: usize = 2000;
const DEFAULT_MODEL: &str = "openai/gpt-5.6-luna";
const DEFAULT_MAX_HISTORY: usize = 20;

const HELP_TEXT: &str = "\
**使い方**
`!ai <質問>` または ボットにメンションして質問
`!ai reset` … このチャンネルの会話履歴を消す
`!ai model` … 使用中のモデルを表示
`!ai help` … このヘルプを表示";

struct Config {
    api_key: String,
    model: String,
    system_prompt: Option<String>,
    max_history: usize,
}

struct Handler {
    config: Config,
    http_client: reqwest::Client,
    histories: Arc<Mutex<HashMap<ChannelId, Vec<serde_json::Value>>>>,
}

impl Handler {
    // 質問文を取り出す。ボット宛てでなければ None
    fn extract_input(&self, ctx: &Context, msg: &Message) -> Option<String> {
        let content = msg.content.trim();
        if content == PREFIX {
            return Some(String::new());
        }
        if let Some(rest) = content.strip_prefix(&format!("{} ", PREFIX)) {
            return Some(rest.trim().to_string());
        }
        let bot_id = ctx.cache.current_user().id;
        if msg.mentions.iter().any(|u| u.id == bot_id) {
            let mention1 = format!("<@{}>", bot_id);
            let mention2 = format!("<@!{}>", bot_id);
            return Some(
                content
                    .replace(&mention1, "")
                    .replace(&mention2, "")
                    .trim()
                    .to_string(),
            );
        }
        None
    }

    async fn ask_ai(&self, messages: &[serde_json::Value]) -> Result<String, String> {
        let mut all_messages = Vec::new();
        if let Some(prompt) = &self.config.system_prompt {
            all_messages.push(serde_json::json!({"role": "system", "content": prompt}));
        }
        all_messages.extend_from_slice(messages);

        let body = serde_json::json!({
            "model": self.config.model,
            "messages": all_messages
        });

        let res = self
            .http_client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("通信に失敗しました: {}", e))?;

        let status = res.status();
        let json: serde_json::Value = res
            .json()
            .await
            .map_err(|e| format!("レスポンスの解析に失敗しました: {}", e))?;

        if !status.is_success() {
            let detail = json["error"]["message"].as_str().unwrap_or("不明なエラー");
            return Err(format!("APIエラー ({}): {}", status, detail));
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "AIの返答が空でした".to_string())
    }
}

// Discordの文字数制限に収まるよう分割する(なるべく改行で区切る)
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max_len {
        let limit = rest
            .char_indices()
            .nth(max_len)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let cut = match rest[..limit].rfind('\n') {
            Some(i) if i > 0 => i,
            _ => limit,
        };
        chunks.push(rest[..cut].to_string());
        rest = rest[cut..].trim_start_matches('\n');
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

async fn reply(ctx: &Context, msg: &Message, text: &str) {
    for (i, chunk) in split_message(text, DISCORD_MAX_LEN).iter().enumerate() {
        let result = if i == 0 {
            msg.reply(&ctx.http, chunk).await.map(|_| ())
        } else {
            msg.channel_id.say(&ctx.http, chunk).await.map(|_| ())
        };
        if let Err(e) = result {
            println!("送信失敗: {}", e);
            return;
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("{} is connected! (model: {})", ready.user.name, self.config.model);
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let Some(question) = self.extract_input(&ctx, &msg) else {
            return;
        };

        match question.as_str() {
            "" | "help" => {
                reply(&ctx, &msg, HELP_TEXT).await;
                return;
            }
            "reset" => {
                self.histories.lock().await.remove(&msg.channel_id);
                reply(&ctx, &msg, "会話履歴をリセットしました").await;
                return;
            }
            "model" => {
                reply(&ctx, &msg, &format!("使用中のモデル: `{}`", self.config.model)).await;
                return;
            }
            _ => {}
        }

        println!("質問開始!:{}", question);
        let user_message = serde_json::json!({"role": "user", "content": question});

        // 通信中にロックを握り続けないよう、履歴のコピーを作ってすぐ解放する
        let mut messages = self
            .histories
            .lock()
            .await
            .get(&msg.channel_id)
            .cloned()
            .unwrap_or_default();
        messages.push(user_message.clone());

        // 返答が来るまで「入力中...」を表示する(drop で止まる)
        let typing = msg.channel_id.start_typing(&ctx.http);
        let result = self.ask_ai(&messages).await;
        drop(typing);

        match result {
            Ok(content) => {
                {
                    let mut histories = self.histories.lock().await;
                    let history = histories.entry(msg.channel_id).or_default();
                    history.push(user_message);
                    history.push(serde_json::json!({"role": "assistant", "content": content}));
                    if history.len() > self.config.max_history {
                        let excess = history.len() - self.config.max_history;
                        history.drain(..excess);
                    }
                }
                reply(&ctx, &msg, &content).await;
            }
            Err(e) => {
                println!("{}", e);
                reply(&ctx, &msg, &format!("⚠️ {}", e)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let token = std::env::var("DISCORD_TOKEN").expect("NO DISCORD TOKEN");
    let config = Config {
        api_key: std::env::var("API_KEY").expect("NO API KEY"),
        model: std::env::var("AI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        system_prompt: std::env::var("SYSTEM_PROMPT").ok().filter(|s| !s.is_empty()),
        max_history: std::env::var("MAX_HISTORY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_HISTORY),
    };

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let handler = Handler {
        config,
        http_client: reqwest::Client::new(),
        histories: Arc::new(Mutex::new(HashMap::new())),
    };
    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .await?;

    client.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_is_not_split() {
        assert_eq!(split_message("こんにちは", 2000), vec!["こんにちは"]);
    }

    #[test]
    fn long_message_is_split_within_limit() {
        let text = "あ".repeat(4500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 2000));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_prefers_newlines() {
        let text = format!("{}\n{}", "a".repeat(10), "b".repeat(10));
        assert_eq!(split_message(&text, 15), vec!["a".repeat(10), "b".repeat(10)]);
    }
}
