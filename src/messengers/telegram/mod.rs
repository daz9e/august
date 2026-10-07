//! Telegram vendor: long-polling Bot API channel with Markdown → HTML rendering,
//! inline-button actions, an owner allowlist and a guided `connect` flow.

mod api;
mod markdown;

use super::{
    Attachment, Button, Messenger, MessengerDef, Thread, CommandSpec, Inbound, InboundKind, Limits, User,
    parse_command,
};
use crate::messengers::bus::Bus;
use crate::config;
use anyhow::{Context, Result, bail};
use api::Api;
use async_trait::async_trait;
use dialoguer::{Confirm, Password, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const ID: &str = "telegram";
/// Telegram allows 4096 chars after HTML conversion; Markdown source is kept well below.
const MAX_LEN: usize = 3000;
/// Bot API limits: bots download files up to 20 MB and upload up to 50 MB
/// (photos up to 10 MB).
const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
const MAX_UPLOAD: u64 = 50 * 1024 * 1024;
const MAX_PHOTO: u64 = 10 * 1024 * 1024;
const MAX_CAPTION: usize = 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub token: String,
    /// Telegram user ids allowed to talk to the bot. Empty = nobody.
    #[serde(default)]
    pub allowed: Vec<i64>,
}

type Channels = BTreeMap<String, Value>;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Saved config with `TELEGRAM_BOT_TOKEN` / `TELEGRAM_ALLOWED_USERS` overrides.
pub fn load_config() -> Result<Option<Config>> {
    let mut all: Channels = config::load(config::CHANNELS)?;
    let mut cfg: Option<Config> = all
        .remove(ID)
        .map(serde_json::from_value)
        .transpose()
        .context("parse telegram section of channels.json")?;
    if let Some(token) = env("TELEGRAM_BOT_TOKEN") {
        cfg.get_or_insert_with(Config::default).token = token;
    }
    if let (Some(list), Some(c)) = (env("TELEGRAM_ALLOWED_USERS"), cfg.as_mut()) {
        c.allowed = list.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    }
    Ok(cfg.filter(|c| !c.token.is_empty()))
}

fn save_config(cfg: &Config) -> Result<()> {
    let mut all: Channels = config::load(config::CHANNELS)?;
    all.insert(ID.into(), serde_json::to_value(cfg)?);
    config::save(config::CHANNELS, &all)
}

// ---------------------------------------------------------------- inbound parsing

struct Bot {
    id: i64,
    username: String,
}

#[derive(Debug)]
enum Parsed {
    Ignore,
    /// Sender is not on the allowlist: `(chat id, user id)`.
    Deny(i64, i64),
    Event(Inbound),
}

fn display_name(from: &Value) -> String {
    let first = from["first_name"].as_str().unwrap_or("");
    match from["username"].as_str() {
        Some(u) if first.is_empty() => format!("@{u}"),
        Some(u) => format!("{first} (@{u})"),
        None => first.to_string(),
    }
}

fn parse_update(u: &Value, bot: &Bot, allowed: &[i64]) -> Parsed {
    let chat_of = |id: i64| Thread {
        messenger: ID.into(),
        id: id.to_string(),
    };

    if let Some(cb) = u.get("callback_query") {
        let (Some(user), Some(chat), Some(id)) = (
            cb["from"]["id"].as_i64(),
            cb["message"]["chat"]["id"].as_i64(),
            cb["id"].as_str(),
        ) else {
            return Parsed::Ignore;
        };
        if !allowed.contains(&user) {
            return Parsed::Deny(chat, user);
        }
        return Parsed::Event(Inbound {
            thread: chat_of(chat),
            user: User {
                id: user.to_string(),
                name: display_name(&cb["from"]),
            },
            kind: InboundKind::Action {
                id: id.to_string(),
                data: cb["data"].as_str().unwrap_or("").to_string(),
            },
        });
    }

    let m = &u["message"];
    let (Some(chat), Some(user)) = (m["chat"]["id"].as_i64(), m["from"]["id"].as_i64()) else {
        return Parsed::Ignore;
    };
    let files = attachments(m);
    let text = m["text"].as_str().or(m["caption"].as_str()).unwrap_or("");
    if text.is_empty() && files.is_empty() {
        return Parsed::Ignore; // stickers, service messages, ...
    }
    if m["from"]["is_bot"].as_bool() == Some(true) {
        return Parsed::Ignore;
    }

    let private = m["chat"]["type"] == "private";
    let mention = format!("@{}", bot.username.to_ascii_lowercase());
    let mentioned = text.to_ascii_lowercase().contains(&mention);
    let replied_to_bot = m["reply_to_message"]["from"]["id"].as_i64() == Some(bot.id);
    let command = files.is_empty().then(|| parse_command(text, Some(&bot.username))).flatten();
    // In groups the bot only reacts when addressed.
    if !private && !(mentioned || replied_to_bot || command.is_some()) {
        return Parsed::Ignore;
    }
    if !allowed.contains(&user) {
        return Parsed::Deny(chat, user);
    }

    let kind = match command {
        Some((name, args)) => InboundKind::Command { name, args },
        None => {
            let cleaned = if mentioned {
                let lower = text.to_ascii_lowercase();
                let at = lower.find(&mention).unwrap_or(0);
                format!("{}{}", &text[..at], &text[at + mention.len()..])
            } else {
                text.to_string()
            };
            InboundKind::Message { text: cleaned.trim().to_string(), files }
        }
    };
    Parsed::Event(Inbound {
        thread: chat_of(chat),
        user: User {
            id: user.to_string(),
            name: display_name(&m["from"]),
        },
        kind,
    })
}

/// Files attached to a message: the largest size of a photo, or a document,
/// video, audio, voice note or animation.
fn attachments(m: &Value) -> Vec<Attachment> {
    let file = |f: &Value, mime: Option<&str>| {
        Some(Attachment {
            id: f["file_id"].as_str()?.to_string(),
            name: f["file_name"].as_str().map(str::to_string),
            mime: f["mime_type"].as_str().or(mime).map(str::to_string),
            size: f["file_size"].as_u64(),
        })
    };
    let mut out = Vec::new();
    if let Some(sizes) = m["photo"].as_array() {
        let largest = sizes.iter().max_by_key(|p| p["width"].as_u64().unwrap_or(0) * p["height"].as_u64().unwrap_or(0));
        out.extend(largest.and_then(|p| file(p, Some("image/jpeg"))));
    }
    for key in ["document", "video", "audio", "voice", "animation"] {
        if m[key].is_object() {
            out.extend(file(&m[key], None));
        }
    }
    out
}

// ---------------------------------------------------------------- channel

pub struct TelegramChannel {
    api: Api,
    allowed: Vec<i64>,
}

impl TelegramChannel {
    pub fn new(cfg: &Config) -> Self {
        Self {
            api: Api::new(&cfg.token),
            allowed: cfg.allowed.clone(),
        }
    }

    #[cfg(test)]
    fn with_api(api: Api, allowed: Vec<i64>) -> Self {
        Self { api, allowed }
    }
}

fn chat_id(chat: &str) -> Value {
    chat.parse::<i64>().map(Value::from).unwrap_or_else(|_| chat.into())
}

fn markup(buttons: &[Button]) -> Option<Value> {
    (!buttons.is_empty()).then(|| {
        let row: Vec<Value> = buttons
            .iter()
            .map(|b| json!({"text": b.label, "callback_data": b.data}))
            .collect();
        json!({"inline_keyboard": [row]})
    })
}

fn plain(md: &str) -> String {
    md.chars().take(4096).collect()
}

#[async_trait]
impl Messenger for TelegramChannel {
    fn id(&self) -> &str {
        ID
    }

    fn limits(&self) -> Limits {
        Limits {
            max_len: MAX_LEN,
            // Telegram allows about one edit per second per chat.
            edit_interval: Duration::from_millis(1100),
        }
    }

    async fn run(&self, bus: Bus<Inbound>) -> Result<()> {
        let me = self.api.get_me().await.context("bad Telegram token?")?;
        let bot = Bot {
            id: me["id"].as_i64().unwrap_or(0),
            username: me["username"].as_str().unwrap_or("").to_string(),
        };
        // Long polling and webhooks are mutually exclusive.
        self.api.call("deleteWebhook", json!({})).await.ok();
        eprintln!("telegram: connected as @{}", bot.username);

        let mut offset = 0;
        loop {
            let updates = match self.api.get_updates(offset, 30).await {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("telegram: {e:#}; retrying in 3s");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };
            for u in updates {
                offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                match parse_update(&u, &bot, &self.allowed) {
                    Parsed::Ignore => {}
                    Parsed::Event(e) => {
                        eprintln!("telegram · {} ({}): {:?}", e.user.name, e.user.id, e.kind);
                        bus.publish(e)
                    }
                    Parsed::Deny(chat, user) => {
                        eprintln!("telegram: rejected user {user}");
                        let text = format!(
                            "⛔ You are not authorized to use this bot.\nYour Telegram user id: {user}\n\
                             The owner can add you with `august connect telegram`."
                        );
                        self.api
                            .call("sendMessage", json!({"chat_id": chat, "text": text}))
                            .await
                            .ok();
                    }
                }
            }
        }
    }

    async fn send(&self, chat: &str, markdown: &str, buttons: &[Button]) -> Result<String> {
        let mut params = json!({
            "chat_id": chat_id(chat),
            "text": markdown::to_html(markdown),
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(m) = markup(buttons) {
            params["reply_markup"] = m;
        }
        let html_too_long = params["text"].as_str().is_some_and(|t| t.chars().count() > 4096);
        let res = if html_too_long {
            Err(anyhow::anyhow!("can't parse entities: too long"))
        } else {
            self.api.call("sendMessage", params.clone()).await
        };
        let sent = match res {
            Err(e) if api::is_parse_error(&e) => {
                params["text"] = plain(markdown).into();
                params.as_object_mut().unwrap().remove("parse_mode");
                self.api.call("sendMessage", params).await?
            }
            other => other?,
        };
        Ok(sent["message_id"].as_i64().context("no message_id")?.to_string())
    }

    async fn edit(
        &self,
        chat: &str,
        message: &str,
        markdown: &str,
        buttons: &[Button],
    ) -> Result<()> {
        let mut params = json!({
            "chat_id": chat_id(chat),
            "message_id": message.parse::<i64>().context("bad message id")?,
            "text": markdown::to_html(markdown),
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            // An empty keyboard removes the buttons.
            "reply_markup": markup(buttons).unwrap_or_else(|| json!({"inline_keyboard": []})),
        });
        let html_too_long = params["text"].as_str().is_some_and(|t| t.chars().count() > 4096);
        let res = if html_too_long {
            Err(anyhow::anyhow!("can't parse entities: too long"))
        } else {
            self.api.call("editMessageText", params.clone()).await
        };
        match res {
            Err(e) if api::is_parse_error(&e) => {
                params["text"] = plain(markdown).into();
                params.as_object_mut().unwrap().remove("parse_mode");
                match self.api.call("editMessageText", params).await {
                    Err(e) if api::is_not_modified(&e) => Ok(()),
                    other => other.map(|_| ()),
                }
            }
            Err(e) if api::is_not_modified(&e) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    async fn typing(&self, chat: &str) -> Result<()> {
        self.api
            .call("sendChatAction", json!({"chat_id": chat_id(chat), "action": "typing"}))
            .await
            .map(|_| ())
    }

    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()> {
        let list: Vec<Value> = commands
            .iter()
            .map(|c| json!({"command": c.name, "description": c.description}))
            .collect();
        self.api
            .call("setMyCommands", json!({"commands": list}))
            .await
            .map(|_| ())
    }

    async fn ack_action(&self, action_id: &str) -> Result<()> {
        self.api
            .call("answerCallbackQuery", json!({"callback_query_id": action_id}))
            .await
            .map(|_| ())
    }

    async fn download(&self, file: &Attachment) -> Result<Vec<u8>> {
        if file.size.is_some_and(|s| s > MAX_DOWNLOAD) {
            bail!("file is larger than the 20 MB Telegram lets bots download");
        }
        self.api.download(&file.id).await
    }

    async fn send_file(&self, chat: &str, path: &std::path::Path, caption: &str) -> Result<()> {
        let bytes = tokio::fs::read(path).await.with_context(|| format!("read {}", path.display()))?;
        let size = bytes.len() as u64;
        if size > MAX_UPLOAD {
            bail!("file is larger than the 50 MB Telegram lets bots upload");
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let mut params = json!({"chat_id": chat_id(chat)});
        if !caption.is_empty() {
            params["caption"] = caption.chars().take(MAX_CAPTION).collect::<String>().into();
        }
        let photo = matches!(crate::util::mime_for(&name), "image/jpeg" | "image/png" | "image/webp");
        if photo && size <= MAX_PHOTO {
            match self.api.upload("sendPhoto", params.clone(), "photo", &name, &bytes).await {
                Ok(_) => return Ok(()),
                // e.g. unusual dimensions: still deliver it, as a file
                Err(e) => eprintln!("telegram: sendPhoto failed, sending as a document: {e:#}"),
            }
        }
        self.api.upload("sendDocument", params, "document", &name, &bytes).await.map(|_| ())
    }
}

// ---------------------------------------------------------------- vendor / setup

pub struct TelegramDef;

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

#[async_trait]
impl MessengerDef for TelegramDef {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        "Telegram (bot)"
    }

    fn is_configured(&self) -> Result<bool> {
        Ok(load_config()?.is_some())
    }

    fn build(&self) -> Result<Option<Arc<dyn Messenger>>> {
        let Some(cfg) = load_config()? else {
            return Ok(None);
        };
        if cfg.allowed.is_empty() {
            eprintln!("telegram: no allowed users, nobody can talk to the bot (run `august connect telegram`)");
        }
        Ok(Some(Arc::new(TelegramChannel::new(&cfg))))
    }

    async fn setup(&self) -> Result<()> {
        let existing = load_config()?;
        let mut cfg = existing.clone().unwrap_or_default();

        let reuse = match &existing {
            Some(_) => Confirm::with_theme(&theme())
                .with_prompt("A bot token is already saved. Reuse it?")
                .default(true)
                .interact()?,
            None => false,
        };
        if !reuse {
            println!(
                "\n1. Open @BotFather in Telegram and send /newbot\n\
                 2. Pick a name and a username, then copy the token it gives you\n"
            );
            cfg.token = Password::with_theme(&theme())
                .with_prompt("Bot token")
                .interact()?
                .trim()
                .to_string();
        }

        let api = Api::new(&cfg.token);
        let me = api.get_me().await.context("Telegram rejected the token")?;
        let username = me["username"].as_str().unwrap_or("?").to_string();
        println!("bot: @{username}");

        let user = pair(&api, &username).await?;
        if !cfg.allowed.contains(&user.0) {
            cfg.allowed.push(user.0);
        }
        save_config(&cfg)?;
        api.call(
            "sendMessage",
            json!({"chat_id": user.0, "text": "✅ August is connected. Run `august serve` on your machine, then message me here."}),
        )
        .await
        .ok();
        println!(
            "saved to {}\nallowed users: {:?}\nstart the bot with: august serve",
            config::home().join(config::CHANNELS).display(),
            cfg.allowed
        );
        Ok(())
    }
}

/// Waits for a private message to the bot and returns its sender after confirmation.
async fn pair(api: &Api, username: &str) -> Result<(i64, String)> {
    // Skip anything sent before setup started.
    let mut offset = api
        .get_updates(-1, 0)
        .await?
        .last()
        .and_then(|u| u["update_id"].as_i64())
        .map_or(0, |id| id + 1);

    println!("\nNow open https://t.me/{username} and send it any message (waiting up to 3 min)…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while tokio::time::Instant::now() < deadline {
        for u in api.get_updates(offset, 20).await? {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let m = &u["message"];
            if m["chat"]["type"] != "private" || m["from"]["is_bot"].as_bool() == Some(true) {
                continue;
            }
            let Some(id) = m["from"]["id"].as_i64() else { continue };
            let name = display_name(&m["from"]);
            let ok = Confirm::with_theme(&theme())
                .with_prompt(format!("Allow {name} (id {id}) to control August?"))
                .default(true)
                .interact()?;
            api.get_updates(offset, 0).await.ok(); // acknowledge
            if ok {
                return Ok((id, name));
            }
            println!("skipped, waiting for another message…");
        }
    }
    bail!("no message received; run `august connect telegram` again")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Bot {
        Bot { id: 99, username: "AugustBot".into() }
    }

    fn msg(chat_type: &str, text: &str, user: i64) -> Value {
        json!({"update_id": 1, "message": {
            "chat": {"id": 5, "type": chat_type}, "from": {"id": user, "first_name": "Ann", "username": "ann"},
            "text": text}})
    }

    #[test]
    fn private_text_and_command() {
        let Parsed::Event(e) = parse_update(&msg("private", "hi", 7), &bot(), &[7]) else { panic!() };
        assert!(matches!(e.kind, InboundKind::Message { ref text, .. } if text == "hi"));
        assert_eq!(e.user.name, "Ann (@ann)");
        let Parsed::Event(e) = parse_update(&msg("private", "/model x", 7), &bot(), &[7]) else { panic!() };
        assert!(matches!(e.kind, InboundKind::Command { ref name, ref args } if name == "model" && args == "x"));
    }

    #[test]
    fn allowlist_is_enforced() {
        assert!(matches!(parse_update(&msg("private", "hi", 8), &bot(), &[7]), Parsed::Deny(5, 8)));
        assert!(matches!(parse_update(&msg("private", "hi", 8), &bot(), &[]), Parsed::Deny(..)));
    }

    #[test]
    fn groups_need_a_mention() {
        assert!(matches!(parse_update(&msg("supergroup", "hi all", 7), &bot(), &[7]), Parsed::Ignore));
        let Parsed::Event(e) = parse_update(&msg("supergroup", "@augustbot hi", 7), &bot(), &[7]) else { panic!() };
        assert!(matches!(e.kind, InboundKind::Message { ref text, .. } if text == "hi"));
        assert!(matches!(parse_update(&msg("group", "/new@OtherBot", 7), &bot(), &[7]), Parsed::Ignore));
    }

    #[test]
    fn callbacks() {
        let u = json!({"update_id": 2, "callback_query": {"id": "cb1", "from": {"id": 7, "first_name": "A"},
            "message": {"chat": {"id": 5}}, "data": "ap:1:y"}});
        let Parsed::Event(e) = parse_update(&u, &bot(), &[7]) else { panic!() };
        assert!(matches!(e.kind, InboundKind::Action { ref id, ref data } if id == "cb1" && data == "ap:1:y"));
        assert!(matches!(parse_update(&u, &bot(), &[1]), Parsed::Deny(5, 7)));
    }

    /// End to end against a fake Bot API server.
    #[tokio::test]
    async fn send_falls_back_to_plain_text_on_parse_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for n in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let len = s.read(&mut buf).await.unwrap();
                seen2.lock().unwrap().push(String::from_utf8_lossy(&buf[..len]).to_string());
                let body = if n == 0 {
                    r#"{"ok":false,"description":"Bad Request: can't parse entities: nope"}"#
                } else {
                    r#"{"ok":true,"result":{"message_id":42}}"#
                };
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                s.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        let ch = TelegramChannel::with_api(Api::with_host(&format!("http://{addr}"), "T"), vec![]);
        let id = ch.send("5", "**hi**", &[]).await.unwrap();
        assert_eq!(id, "42");
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("parse_mode") && seen[0].contains("<b>hi</b>"));
        assert!(!seen[1].contains("parse_mode") && seen[1].contains("**hi**"));
    }
}
