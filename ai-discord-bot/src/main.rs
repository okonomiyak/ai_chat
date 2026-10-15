use base64::Engine;
use serenity::all::{
    Attachment, Command, CommandInteraction, CommandOptionType, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, EditInteractionResponse, GuildId, Interaction,
    ResolvedValue,
};
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
const MAX_IMAGE_BYTES: u32 = 10 * 1024 * 1024;
const MAX_MODEL_CHOICES: usize = 25;
const DEFAULT_IMAGE_PROMPT: &str = "この画像について説明してください。";

const HELP_TEXT: &str = "\
**使い方**
`/ai <質問> [画像]` または `!ai <質問>` / ボットにメンションして質問
画像を添付して質問すると、画像の内容も読み取ります
`/reset` … このチャンネルの会話履歴を消す
`/model [モデル]` … 使用中のモデルを表示・このチャンネルのモデルを切り替え
`/help` … このヘルプを表示";

struct Config {
    api_key: String,
    model: String,
    models: Vec<String>,
    system_prompt: Option<String>,
    max_history: usize,
    guild_id: Option<GuildId>,
}

#[derive(Default)]
struct ChannelState {
    history: Vec<serde_json::Value>,
    model: Option<String>,
}

struct Handler {
    config: Config,
    http_client: reqwest::Client,
    channels: Arc<Mutex<HashMap<ChannelId, ChannelState>>>,
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

    async fn current_model(&self, channel_id: ChannelId) -> String {
        self.channels
            .lock()
            .await
            .get(&channel_id)
            .and_then(|s| s.model.clone())
            .unwrap_or_else(|| self.config.model.clone())
    }

    // 画像をダウンロードして data URL にする(Discord の URL は期限切れになるため)
    async fn fetch_image(&self, attachment: &Attachment) -> Result<String, String> {
        if attachment.size > MAX_IMAGE_BYTES {
            return Err(format!(
                "画像が大きすぎます(上限 {}MB)",
                MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
        let bytes = self
            .http_client
            .get(&attachment.url)
            .send()
            .await
            .and_then(|res| res.error_for_status())
            .map_err(|e| format!("画像の取得に失敗しました: {}", e))?
            .bytes()
            .await
            .map_err(|e| format!("画像の取得に失敗しました: {}", e))?;
        let content_type = attachment.content_type.as_deref().unwrap_or("image/png");
        Ok(format!(
            "data:{};base64,{}",
            content_type,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        ))
    }

    async fn fetch_images(&self, attachments: &[&Attachment]) -> Result<Vec<String>, String> {
        let mut images = Vec::new();
        for attachment in attachments {
            images.push(self.fetch_image(attachment).await?);
        }
        Ok(images)
    }

    async fn ask_ai(&self, model: &str, messages: &[serde_json::Value]) -> Result<String, String> {
        let mut all_messages = Vec::new();
        if let Some(prompt) = &self.config.system_prompt {
            all_messages.push(serde_json::json!({"role": "system", "content": prompt}));
        }
        all_messages.extend_from_slice(messages);

        let body = serde_json::json!({
            "model": model,
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

    // 履歴付きで AI に質問し、成功したら履歴に追加する
    async fn chat(
        &self,
        channel_id: ChannelId,
        question: &str,
        images: &[String],
    ) -> Result<String, String> {
        let user_message =
            serde_json::json!({"role": "user", "content": build_user_content(question, images)});

        // 通信中にロックを握り続けないよう、履歴のコピーを作ってすぐ解放する
        let (mut messages, model) = {
            let channels = self.channels.lock().await;
            let state = channels.get(&channel_id);
            (
                state.map(|s| s.history.clone()).unwrap_or_default(),
                state
                    .and_then(|s| s.model.clone())
                    .unwrap_or_else(|| self.config.model.clone()),
            )
        };
        messages.push(user_message.clone());

        let content = self.ask_ai(&model, &messages).await?;

        let mut channels = self.channels.lock().await;
        let history = &mut channels.entry(channel_id).or_default().history;
        history.push(user_message);
        history.push(serde_json::json!({"role": "assistant", "content": content}));
        trim_history(history, self.config.max_history);
        Ok(content)
    }

    async fn reset(&self, channel_id: ChannelId) {
        if let Some(state) = self.channels.lock().await.get_mut(&channel_id) {
            state.history.clear();
        }
    }

    // /model と !ai model の共通処理
    async fn select_model(&self, channel_id: ChannelId, name: Option<&str>) -> String {
        let Some(name) = name else {
            let current = self.current_model(channel_id).await;
            let list = self
                .config
                .models
                .iter()
                .map(|m| format!("`{}`", m))
                .collect::<Vec<_>>()
                .join(", ");
            return format!("使用中のモデル: `{}`\n選べるモデル: {}", current, list);
        };
        if !self.config.models.iter().any(|m| m == name) {
            return format!("`{}` は選べるモデルにありません", name);
        }
        self.channels.lock().await.entry(channel_id).or_default().model = Some(name.to_string());
        format!("このチャンネルのモデルを `{}` に切り替えました", name)
    }

    fn commands(&self) -> Vec<CreateCommand> {
        let mut model_option = CreateCommandOption::new(
            CommandOptionType::String,
            "name",
            "切り替えるモデル(省略すると現在のモデルを表示)",
        );
        for model in self.config.models.iter().take(MAX_MODEL_CHOICES) {
            model_option = model_option.add_string_choice(model, model);
        }

        vec![
            CreateCommand::new("ai")
                .description("AIに質問する")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "question", "質問内容")
                        .required(false),
                )
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Attachment,
                    "image",
                    "AIに見せる画像",
                )),
            CreateCommand::new("reset").description("このチャンネルの会話履歴を消す"),
            CreateCommand::new("model")
                .description("使用中のモデルを表示・切り替え")
                .add_option(model_option),
            CreateCommand::new("help").description("使い方を表示"),
        ]
    }

    async fn handle_command(&self, ctx: &Context, command: &CommandInteraction) {
        let channel_id = command.channel_id;
        match command.data.name.as_str() {
            "ai" => self.handle_ai_command(ctx, command).await,
            "reset" => {
                self.reset(channel_id).await;
                respond(ctx, command, "会話履歴をリセットしました", false).await;
            }
            "model" => {
                let name = command.data.options().into_iter().find_map(|o| match o.value {
                    ResolvedValue::String(s) if o.name == "name" => Some(s.to_string()),
                    _ => None,
                });
                let text = self.select_model(channel_id, name.as_deref()).await;
                respond(ctx, command, &text, false).await;
            }
            "help" => respond(ctx, command, HELP_TEXT, true).await,
            _ => {}
        }
    }

    async fn handle_ai_command(&self, ctx: &Context, command: &CommandInteraction) {
        let mut question = String::new();
        let mut image: Option<Attachment> = None;
        for option in command.data.options() {
            match option.value {
                ResolvedValue::String(s) if option.name == "question" => question = s.to_string(),
                ResolvedValue::Attachment(a) if option.name == "image" => image = Some(a.clone()),
                _ => {}
            }
        }
        if question.is_empty() && image.is_none() {
            respond(ctx, command, "質問か画像を指定してください", true).await;
            return;
        }
        if let Some(a) = &image {
            if !is_image(a) {
                respond(ctx, command, "画像ファイルを添付してください", true).await;
                return;
            }
        }

        // 3秒以内に応答しないと失敗扱いになるので、先に「考え中...」を出す
        let defer = CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new());
        if let Err(e) = command.create_response(&ctx.http, defer).await {
            println!("応答失敗: {}", e);
            return;
        }

        println!("質問開始!:{}", question);
        let result = async {
            let images = match &image {
                Some(a) => self.fetch_images(&[a]).await?,
                None => Vec::new(),
            };
            let prompt = if question.is_empty() { DEFAULT_IMAGE_PROMPT } else { &question };
            self.chat(command.channel_id, prompt, &images).await
        }
        .await;

        // スラッシュコマンドは質問文がチャンネルに残らないので引用して見せる
        let mut header = format!("> {}", truncate(&question, 200));
        if image.is_some() {
            header.push_str(" 🖼️");
        }
        let text = match result {
            Ok(content) => format!("{}\n\n{}", header, content),
            Err(e) => {
                println!("{}", e);
                format!("{}\n\n⚠️ {}", header, e)
            }
        };

        for (i, chunk) in split_message(&text, DISCORD_MAX_LEN).iter().enumerate() {
            let result = if i == 0 {
                command
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(chunk))
                    .await
                    .map(|_| ())
            } else {
                command
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new().content(chunk),
                    )
                    .await
                    .map(|_| ())
            };
            if let Err(e) = result {
                println!("送信失敗: {}", e);
                return;
            }
        }
    }

    async fn handle_message(&self, ctx: &Context, msg: &Message, question: String) {
        let attachments: Vec<&Attachment> = msg.attachments.iter().filter(|a| is_image(a)).collect();

        if attachments.is_empty() {
            match question.as_str() {
                "" | "help" => return reply(ctx, msg, HELP_TEXT).await,
                "reset" => {
                    self.reset(msg.channel_id).await;
                    return reply(ctx, msg, "会話履歴をリセットしました").await;
                }
                "model" => {
                    let text = self.select_model(msg.channel_id, None).await;
                    return reply(ctx, msg, &text).await;
                }
                _ => {}
            }
            if let Some(name) = question.strip_prefix("model ") {
                let text = self.select_model(msg.channel_id, Some(name.trim())).await;
                return reply(ctx, msg, &text).await;
            }
        }

        println!("質問開始!:{}", question);
        // 返答が来るまで「入力中...」を表示する(drop で止まる)
        let typing = msg.channel_id.start_typing(&ctx.http);
        let result = async {
            let images = self.fetch_images(&attachments).await?;
            let prompt = if question.is_empty() { DEFAULT_IMAGE_PROMPT } else { &question };
            self.chat(msg.channel_id, prompt, &images).await
        }
        .await;
        drop(typing);

        match result {
            Ok(content) => reply(ctx, msg, &content).await,
            Err(e) => {
                println!("{}", e);
                reply(ctx, msg, &format!("⚠️ {}", e)).await;
            }
        }
    }
}

fn is_image(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("image/"))
}

// 画像があれば OpenAI 互換のマルチモーダル形式にする
fn build_user_content(text: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::json!(text);
    }
    let mut parts = vec![serde_json::json!({"type": "text", "text": text})];
    for url in images {
        parts.push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
    }
    serde_json::Value::Array(parts)
}

// 古い履歴を捨てる。先頭が AI の返答にならないよう user から始める
fn trim_history(history: &mut Vec<serde_json::Value>, max: usize) {
    if history.len() > max {
        history.drain(..history.len() - max);
    }
    while history.first().is_some_and(|m| m["role"] != "user") {
        history.remove(0);
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars).collect();
    s.push('…');
    s
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

async fn respond(ctx: &Context, command: &CommandInteraction, text: &str, ephemeral: bool) {
    let message = CreateInteractionResponseMessage::new()
        .content(truncate(text, DISCORD_MAX_LEN - 1))
        .ephemeral(ephemeral);
    if let Err(e) = command
        .create_response(&ctx.http, CreateInteractionResponse::Message(message))
        .await
    {
        println!("応答失敗: {}", e);
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected! (model: {})", ready.user.name, self.config.model);

        // GUILD_ID があればそのサーバーにだけ登録する(即時反映されるのでテスト向け)
        let result = match self.config.guild_id {
            Some(guild_id) => guild_id.set_commands(&ctx.http, self.commands()).await,
            None => Command::set_global_commands(&ctx.http, self.commands()).await,
        };
        match result {
            Ok(commands) => println!("スラッシュコマンドを{}個登録しました", commands.len()),
            Err(e) => println!("スラッシュコマンドの登録に失敗しました: {}", e),
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            self.handle_command(&ctx, &command).await;
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        if let Some(question) = self.extract_input(&ctx, &msg) {
            self.handle_message(&ctx, &msg, question).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let token = std::env::var("DISCORD_TOKEN").expect("NO DISCORD TOKEN");

    let model = std::env::var("AI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let mut models = vec![model.clone()];
    for m in std::env::var("AI_MODELS").unwrap_or_default().split(',') {
        let m = m.trim();
        if !m.is_empty() && !models.iter().any(|x| x == m) {
            models.push(m.to_string());
        }
    }

    let config = Config {
        api_key: std::env::var("API_KEY").expect("NO API KEY"),
        model,
        models,
        system_prompt: std::env::var("SYSTEM_PROMPT").ok().filter(|s| !s.is_empty()),
        max_history: std::env::var("MAX_HISTORY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_HISTORY),
        guild_id: std::env::var("GUILD_ID")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .map(GuildId::new),
    };

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let handler = Handler {
        config,
        http_client: reqwest::Client::new(),
        channels: Arc::new(Mutex::new(HashMap::new())),
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

    #[test]
    fn text_only_content_is_plain_string() {
        assert_eq!(build_user_content("hi", &[]), serde_json::json!("hi"));
    }

    #[test]
    fn image_content_uses_parts() {
        let content = build_user_content("これは？", &["data:image/png;base64,AAA".to_string()]);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "これは？");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAA");
    }

    #[test]
    fn trim_history_keeps_user_first() {
        let mut history: Vec<_> = (0..6)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                serde_json::json!({"role": role, "content": i})
            })
            .collect();
        trim_history(&mut history, 3);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("あいうえお", 3), "あいう…");
        assert_eq!(truncate("あい", 3), "あい");
    }
}
