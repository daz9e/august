//! `telegram`: August in Telegram, as a bot. Long polling, Markdown → HTML, inline buttons
//! in rows, edits for streaming, uploads as photos or documents, reactions both ways, an
//! allowlist of user ids (others are turned away) and groups only when addressed.
//!
//! Connecting is the account `telegram` (`/login telegram`, `august connect telegram`): the
//! bot token from @BotFather, then the owner pairs by messaging the bot. The token is a
//! secret, the allowed user ids a setting (`allowed`); `TELEGRAM_BOT_TOKEN` and
//! `TELEGRAM_ALLOWED_USERS` (comma-separated) stand in for them. `TELEGRAM_API_BASE` points
//! at a self-hosted Bot API server (or a test double).

mod api;
mod markdown;

use anyhow::{Context, Result, anyhow, bail};
use api::Api;
use async_trait::async_trait;
use august_ext::messenger::{
    Attachment, Button, Capabilities, CommandSpec, Description, FileKind, InboundKind, Messenger, OutMessage, User,
    parse_command,
};
use august_ext::{August, Login, Signed, Thread};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

const ID: &str = "telegram";
/// Telegram allows 4096 chars after HTML conversion; Markdown source is kept well below.
const MAX_LEN: usize = 3000;
/// Bot API limits: bots download files up to 20 MB and upload up to 50 MB
/// (photos up to 10 MB).
const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
const MAX_UPLOAD: u64 = 50 * 1024 * 1024;
const MAX_PHOTO: u64 = 10 * 1024 * 1024;
const MAX_CAPTION: usize = 1024;
/// How long pairing waits for the owner's message.
const PAIR_WAIT: Duration = Duration::from_secs(180);

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------- inbound parsing

struct Bot {
    id: i64,
    username: String,
}

/// What came in to chat `chat` from `user`.
#[derive(Debug)]
struct Event {
    chat: i64,
    user: User,
    kind: InboundKind,
}

#[derive(Debug)]
enum Parsed {
    Ignore,
    /// Sender is not on the allowlist: `(chat id, user id, their name)`.
    Deny(i64, i64, String),
    Event(Event),
    /// A button press, with the callback id to confirm it.
    Press(Event, String),
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
    if let Some(cb) = u.get("callback_query") {
        let (Some(user), Some(chat), Some(id)) = (
            cb["from"]["id"].as_i64(),
            cb["message"]["chat"]["id"].as_i64(),
            cb["id"].as_str(),
        ) else {
            return Parsed::Ignore;
        };
        if !allowed.contains(&user) {
            return Parsed::Deny(chat, user, display_name(&cb["from"]));
        }
        let press = Event {
            chat,
            user: User {
                id: user.to_string(),
                name: display_name(&cb["from"]),
            },
            kind: InboundKind::Press { button: cb["data"].as_str().unwrap_or("").to_string() },
        };
        return Parsed::Press(press, id.to_string());
    }

    let r = &u["message_reaction"];
    if let (Some(chat), Some(message)) = (r["chat"]["id"].as_i64(), r["message_id"].as_i64()) {
        let user = r["user"]["id"].as_i64().unwrap_or(0);
        if !allowed.contains(&user) {
            return Parsed::Ignore;
        }
        let emoji = r["new_reaction"].as_array().into_iter().flatten().find_map(|e| e["emoji"].as_str()).unwrap_or("");
        return Parsed::Event(Event {
            chat,
            user: User { id: user.to_string(), name: display_name(&r["user"]) },
            kind: InboundKind::Reaction { message: message.to_string(), emoji: emoji.to_string() },
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
        return Parsed::Deny(chat, user, display_name(&m["from"]));
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
            InboundKind::Message { id: m["message_id"].to_string(), text: cleaned.trim().to_string(), files }
        }
    };
    Parsed::Event(Event {
        chat,
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
    let file = |f: &Value, mime: Option<&str>, kind: FileKind| {
        Some(Attachment {
            id: f["file_id"].as_str()?.to_string(),
            kind,
            name: f["file_name"].as_str().map(str::to_string),
            mime: f["mime_type"].as_str().or(mime).map(str::to_string),
            size: f["file_size"].as_u64(),
        })
    };
    let mut out = Vec::new();
    if let Some(sizes) = m["photo"].as_array() {
        let largest = sizes.iter().max_by_key(|p| p["width"].as_u64().unwrap_or(0) * p["height"].as_u64().unwrap_or(0));
        out.extend(largest.and_then(|p| file(p, Some("image/jpeg"), FileKind::Image)));
    }
    let kinds = [
        ("document", FileKind::Document),
        ("video", FileKind::Video),
        ("audio", FileKind::Audio),
        ("voice", FileKind::Voice),
        ("animation", FileKind::Video),
    ];
    for (key, kind) in kinds {
        if m[key].is_object() {
            out.extend(file(&m[key], None, kind));
        }
    }
    // A document that is an image or audio is that, for whoever reads the message.
    for a in &mut out {
        let mime = a.mime.as_deref().unwrap_or("");
        if a.kind == FileKind::Document && mime.starts_with("image/") {
            a.kind = FileKind::Image;
        } else if a.kind == FileKind::Document && mime.starts_with("audio/") {
            a.kind = FileKind::Audio;
        }
    }
    out
}

// ---------------------------------------------------------------- messenger

/// The bot as connected now: its API and who may talk to it.
#[derive(Default)]
struct Telegram {
    api: RwLock<Option<Api>>,
    allowed: RwLock<Vec<i64>>,
    /// "typing…" kept up in these chats while August works.
    typing: Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>,
    /// The long-polling loop.
    poller: Mutex<Option<tokio::task::AbortHandle>>,
    /// How polling went last: when it last got an answer, and the error since, if any.
    polled: Mutex<Option<(std::time::Instant, Option<String>)>>,
    /// While pairing: where a private message from someone not yet allowed goes.
    pairing: Mutex<Option<mpsc::UnboundedSender<(i64, String)>>>,
}

impl Telegram {
    fn api(&self) -> Result<Api> {
        self.api.read().unwrap().clone().ok_or_else(|| anyhow!("Telegram is not connected: /login telegram"))
    }

    /// Uploads one file (a photo where it can be), with an optional caption; its message id.
    async fn upload_file(&self, chat: &str, path: &std::path::Path, caption: &str) -> Result<String> {
        let api = self.api()?;
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
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let photo = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp");
        if photo && size <= MAX_PHOTO {
            match api.upload("sendPhoto", params.clone(), "photo", &name, &bytes).await {
                Ok(sent) => return Ok(sent["message_id"].to_string()),
                // e.g. unusual dimensions: still deliver it, as a file
                Err(e) => eprintln!("sendPhoto failed, sending as a document: {e:#}"),
            }
        }
        let sent = api.upload("sendDocument", params, "document", &name, &bytes).await?;
        Ok(sent["message_id"].to_string())
    }

    /// A message with files: a short text without buttons is the caption of a single file;
    /// otherwise the text (with its buttons) goes first. Returns the first message's id.
    async fn send_files(&self, chat: &str, message: &OutMessage) -> Result<String> {
        let caption_fits = message.files.len() == 1 && message.buttons.is_empty() && message.text.chars().count() <= MAX_CAPTION;
        let mut first = None;
        let caption = if caption_fits || message.text.trim().is_empty() {
            message.text.clone()
        } else {
            let text = OutMessage { files: Vec::new(), ..message.clone() };
            first = Some(self.send(chat, &text).await?);
            String::new()
        };
        for (i, path) in message.files.iter().enumerate() {
            let id = self.upload_file(chat, path, if i == 0 { &caption } else { "" }).await?;
            first.get_or_insert(id);
        }
        Ok(first.unwrap_or_default())
    }
}

fn chat_id(chat: &str) -> Value {
    chat.parse::<i64>().map(Value::from).unwrap_or_else(|_| chat.into())
}

fn markup(rows: &[Vec<Button>]) -> Option<Value> {
    (rows.iter().any(|r| !r.is_empty())).then(|| {
        let rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| row.iter().map(|b| json!({"text": b.label, "callback_data": b.id})).collect())
            .collect();
        json!({"inline_keyboard": rows})
    })
}

fn plain(md: &str) -> String {
    md.chars().take(4096).collect()
}

fn description() -> Description {
    Description {
        id: ID.into(),
        name: "Telegram".into(),
        capabilities: Capabilities {
            markdown: true,
            max_len: MAX_LEN,
            buttons: 8,
            edit: true,
            // Telegram allows about one edit per second per chat.
            edit_interval_ms: 1100,
            files_in: true,
            files_out: true,
            images: true,
            audio_in: true,
            commands: true,
            presence: true,
            delete: true,
            reactions: true,
            reply: true,
            threads: true,
        },
        extra: json!({
            "groups": "the bot answers in groups only when mentioned, replied to or given a command",
            "max_download_mb": MAX_DOWNLOAD / 1024 / 1024,
            "max_upload_mb": MAX_UPLOAD / 1024 / 1024,
        }),
    }
}

#[async_trait]
impl Messenger for Telegram {
    async fn threads(&self) -> Vec<String> {
        // A private chat with a user has the user's id.
        self.allowed.read().unwrap().iter().map(|u| u.to_string()).collect()
    }

    async fn send(&self, chat: &str, message: &OutMessage) -> Result<String> {
        if !message.files.is_empty() {
            return self.send_files(chat, message).await;
        }
        let api = self.api()?;
        let (markdown, buttons) = (message.text.as_str(), &message.buttons);
        let mut params = json!({
            "chat_id": chat_id(chat),
            "text": markdown::to_html(markdown),
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(to) = message.reply_to.as_deref().and_then(|id| id.parse::<i64>().ok()) {
            params["reply_parameters"] = json!({"message_id": to, "allow_sending_without_reply": true});
        }
        if let Some(m) = markup(buttons) {
            params["reply_markup"] = m;
        }
        let html_too_long = params["text"].as_str().is_some_and(|t| t.chars().count() > 4096);
        let res = if html_too_long {
            Err(anyhow!("can't parse entities: too long"))
        } else {
            api.call("sendMessage", params.clone()).await
        };
        let sent = match res {
            Err(e) if api::is_parse_error(&e) => {
                params["text"] = plain(markdown).into();
                params.as_object_mut().unwrap().remove("parse_mode");
                api.call("sendMessage", params).await?
            }
            other => other?,
        };
        Ok(sent["message_id"].as_i64().context("no message_id")?.to_string())
    }

    async fn edit(&self, chat: &str, id: &str, message: &OutMessage) -> Result<()> {
        let api = self.api()?;
        let (markdown, buttons) = (message.text.as_str(), &message.buttons);
        let mut params = json!({
            "chat_id": chat_id(chat),
            "message_id": id.parse::<i64>().context("bad message id")?,
            "text": markdown::to_html(markdown),
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            // An empty keyboard removes the buttons.
            "reply_markup": markup(buttons).unwrap_or_else(|| json!({"inline_keyboard": []})),
        });
        let html_too_long = params["text"].as_str().is_some_and(|t| t.chars().count() > 4096);
        let res = if html_too_long {
            Err(anyhow!("can't parse entities: too long"))
        } else {
            api.call("editMessageText", params.clone()).await
        };
        match res {
            Err(e) if api::is_parse_error(&e) => {
                params["text"] = plain(markdown).into();
                params.as_object_mut().unwrap().remove("parse_mode");
                match api.call("editMessageText", params).await {
                    Err(e) if api::is_not_modified(&e) => Ok(()),
                    other => other.map(|_| ()),
                }
            }
            Err(e) if api::is_not_modified(&e) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    async fn presence(&self, chat: &str, busy: bool) {
        let mut typing = self.typing.lock().unwrap();
        if let Some(old) = typing.remove(chat) {
            old.abort();
        }
        let Ok(api) = self.api() else { return };
        if busy {
            // Telegram shows "typing…" for about five seconds per call.
            let id = chat_id(chat);
            let task = tokio::spawn(async move {
                loop {
                    api.call("sendChatAction", json!({"chat_id": id, "action": "typing"})).await.ok();
                    tokio::time::sleep(Duration::from_secs(4)).await;
                }
            });
            typing.insert(chat.to_string(), task.abort_handle());
        }
    }

    async fn delete(&self, chat: &str, id: &str) -> Result<()> {
        let message = id.parse::<i64>().context("bad message id")?;
        self.api()?.call("deleteMessage", json!({"chat_id": chat_id(chat), "message_id": message})).await.map(drop)
    }

    async fn react(&self, chat: &str, id: &str, emoji: &str) -> Result<()> {
        let message = id.parse::<i64>().context("bad message id")?;
        let reaction = if emoji.is_empty() { json!([]) } else { json!([{"type": "emoji", "emoji": emoji}]) };
        let params = json!({"chat_id": chat_id(chat), "message_id": message, "reaction": reaction});
        self.api()?.call("setMessageReaction", params).await.map(drop)
    }

    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()> {
        let list: Vec<Value> = commands.iter().map(|c| json!({"command": c.name, "description": c.description})).collect();
        self.api()?.call("setMyCommands", json!({"commands": list})).await.map(drop)
    }

    async fn download(&self, file: &Attachment) -> Result<Vec<u8>> {
        if file.size.is_some_and(|s| s > MAX_DOWNLOAD) {
            bail!("file is larger than the 20 MB Telegram lets bots download");
        }
        self.api()?.download(&file.id).await
    }
}

// ---------------------------------------------------------------- connection

/// The saved token (or `TELEGRAM_BOT_TOKEN`).
async fn token(august: &August) -> Result<Option<String>> {
    Ok(match env("TELEGRAM_BOT_TOKEN") {
        Some(t) => Some(t),
        None => august.secret("token").await?,
    })
}

/// The allowed user ids: `TELEGRAM_ALLOWED_USERS`, else the setting.
async fn allowed(august: &August) -> Result<Vec<i64>> {
    if let Some(list) = env("TELEGRAM_ALLOWED_USERS") {
        return Ok(list.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    }
    Ok(serde_json::from_value(august.settings().await?["allowed"].clone()).unwrap_or_default())
}

/// Connects with `token`: checks it, offers the messenger and starts long polling (in place
/// of a previous connection). Returns the bot's username.
async fn connect(august: &August, tg: &Arc<Telegram>, token: &str) -> Result<String> {
    let api = Api::new(token);
    let me = api.get_me().await.context("Telegram rejected the bot token")?;
    let bot = Bot { id: me["id"].as_i64().unwrap_or(0), username: me["username"].as_str().unwrap_or("").to_string() };
    // Long polling and webhooks are mutually exclusive.
    api.call("deleteWebhook", json!({})).await.ok();
    *tg.allowed.write().unwrap() = allowed(august).await?;
    if tg.allowed.read().unwrap().is_empty() {
        eprintln!("no allowed users, nobody can talk to the bot (/login telegram pairs one)");
    }
    *tg.api.write().unwrap() = Some(api.clone());
    let username = bot.username.clone();
    *tg.polled.lock().unwrap() = Some((std::time::Instant::now(), None));
    let task = tokio::spawn(poll(august.clone(), tg.clone(), api, bot));
    if let Some(old) = tg.poller.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
    august.register_messenger(description(), tg.clone());
    eprintln!("connected as @{username}");
    Ok(username)
}

async fn poll(august: August, tg: Arc<Telegram>, api: Api, bot: Bot) {
    let mut offset = 0;
    loop {
        let updates = match api.get_updates(offset, 30).await {
            Ok(u) => {
                *tg.polled.lock().unwrap() = Some((std::time::Instant::now(), None));
                u
            }
            Err(e) => {
                if let Some((_, error)) = tg.polled.lock().unwrap().as_mut() {
                    *error = Some(format!("{e:#}"));
                }
                eprintln!("{e:#}; retrying in 3s");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for u in updates {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let allowed = tg.allowed.read().unwrap().clone();
            let (event, callback) = match parse_update(&u, &bot, &allowed) {
                Parsed::Ignore => continue,
                Parsed::Event(e) => (e, None),
                Parsed::Press(e, callback) => (e, Some(callback)),
                Parsed::Deny(chat, user, name) => {
                    // Someone pairing messages the bot privately.
                    let pairing = tg.pairing.lock().unwrap().clone();
                    if let Some(tx) = pairing.filter(|_| u["message"]["chat"]["type"] == "private") {
                        tx.send((user, name)).ok();
                        continue;
                    }
                    eprintln!("rejected user {user}");
                    let text = format!(
                        "⛔ You are not authorized to use this bot.\nYour Telegram user id: {user}\n\
                         The owner can add you with /login telegram."
                    );
                    api.call("sendMessage", json!({"chat_id": chat, "text": text})).await.ok();
                    continue;
                }
            };
            if let Some(callback) = callback {
                // Stops the button's spinner.
                api.call("answerCallbackQuery", json!({"callback_query_id": callback})).await.ok();
            }
            eprintln!("{} ({}): {:?}", event.user.name, event.user.id, event.kind);
            let thread = Thread { messenger: ID.into(), id: event.chat.to_string() };
            if let Err(e) = august.inbound(&thread, &event.user, &event.kind).await {
                eprintln!("could not hand in a message: {e:#}");
            }
        }
    }
}

/// `/login telegram`: the bot token (kept, or a new one), then pairing: the person to allow
/// messages the bot and is confirmed.
async fn login(august: &August, tg: &Arc<Telegram>, steps: Login) -> Result<Signed> {
    let saved = token(august).await?;
    let keep = match &saved {
        Some(_) => steps.choose("A bot token is already saved. Keep it?", &["Keep", "New token"]).await? == "Keep",
        None => false,
    };
    let token = match saved.filter(|_| keep) {
        Some(t) => t,
        None => {
            steps.progress("1. Open @BotFather in Telegram and send /newbot\n2. Pick a name and a username, then copy the token it gives you").await?;
            steps.ask("Bot token", true).await?.trim().to_string()
        }
    };
    let username = connect(august, tg, &token).await?;
    august.set_secret("token", Some(&token)).await?;
    let mut allowed = tg.allowed.read().unwrap().clone();
    if keep && !allowed.is_empty() && steps.choose("Allow one more person to talk to August?", &["Yes", "No"]).await? != "Yes" {
        return Ok(Signed { who: format!("@{username}"), expires_at: None });
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    *tg.pairing.lock().unwrap() = Some(tx);
    steps.progress(&format!("Now open https://t.me/{username} and send it any message (waiting up to 3 min)…")).await?;
    let paired = tokio::time::timeout(PAIR_WAIT, async {
        while let Some((id, name)) = rx.recv().await {
            if steps.choose(&format!("Allow {name} (id {id}) to control August?"), &["Allow", "Skip"]).await? == "Allow" {
                return Ok(Some(id));
            }
            steps.progress("Skipped, waiting for another message…").await?;
        }
        Ok::<_, anyhow::Error>(None)
    })
    .await;
    tg.pairing.lock().unwrap().take();
    let user = paired.ok().transpose()?.flatten().ok_or_else(|| anyhow!("no message came; run /login telegram again"))?;
    if !allowed.contains(&user) {
        allowed.push(user);
    }
    august.call("config_set", json!({"path": "extensions.telegram.settings.allowed", "value": allowed})).await?;
    *tg.allowed.write().unwrap() = allowed;
    tg.api()?.call("sendMessage", json!({"chat_id": user, "text": "✅ August is connected. Message me here."})).await.ok();
    Ok(Signed { who: format!("@{username}"), expires_at: None })
}

/// The poller stopped while connected: broken inside. No answers from Telegram for two long
/// polls: the network or Telegram, which a restart won't fix.
fn health(tg: &Telegram) -> Value {
    if tg.api.read().unwrap().is_none() {
        return json!({"status": "ok", "detail": "not connected"});
    }
    if tg.poller.lock().unwrap().as_ref().is_some_and(|p| p.is_finished()) {
        return json!({"status": "failed", "detail": "the polling loop stopped"});
    }
    match &*tg.polled.lock().unwrap() {
        Some((at, Some(error))) if at.elapsed() > Duration::from_secs(70) => {
            json!({"status": "degraded", "detail": format!("no answer from Telegram for {} s: {error}", at.elapsed().as_secs())})
        }
        _ => json!({"status": "ok"}),
    }
}

/// `/logout telegram`: forgets the token and stops the bot (allowed users stay).
async fn logout(august: &August, tg: &Telegram) -> Result<()> {
    august.set_secret("token", None).await?;
    if let Some(task) = tg.poller.lock().unwrap().take() {
        task.abort();
    }
    tg.api.write().unwrap().take();
    august.unregister_messenger(ID);
    Ok(())
}

#[tokio::main]
async fn main() {
    let august = August::new();
    august.needs(&["config"]);
    august.settings_schema(json!({"properties": {
        "allowed": {"type": "array", "items": {"type": "integer"}, "default": [], "description": "Telegram user ids allowed to talk to the bot (/login telegram pairs one)"},
    }}));
    let tg = Arc::new(Telegram::default());
    let t = tg.clone();
    august.health(move || {
        let t = t.clone();
        async move { Ok(health(&t)) }
    });
    let (a, t) = (august.clone(), tg.clone());
    let (b, u) = (august.clone(), tg.clone());
    august.register_login_account(
        ID,
        "Telegram (bot)",
        &[],
        move |_, steps| {
            let (a, t) = (a.clone(), t.clone());
            async move { login(&a, &t, steps).await }
        },
        move |_| {
            let (a, t) = (b.clone(), u.clone());
            async move { logout(&a, &t).await }
        },
    );
    let (a, t) = (august.clone(), tg.clone());
    tokio::spawn(async move {
        match token(&a).await {
            Ok(Some(token)) => match connect(&a, &t, &token).await {
                // August may not list this extension yet: tell it once it does.
                Ok(username) => {
                    for _ in 0..30 {
                        if a.account_update(ID, "connected", Some(&format!("@{username}"))).await.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                Err(e) => eprintln!("{e:#}"),
            },
            Ok(None) => eprintln!("not connected: /login telegram"),
            Err(e) => eprintln!("{e:#}"),
        }
    });
    august.run().await;
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
        assert!(matches!(parse_update(&msg("private", "hi", 8), &bot(), &[7]), Parsed::Deny(5, 8, _)));
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
            "message": {"chat": {"id": 5}}, "data": "k1.0"}});
        let Parsed::Press(e, callback) = parse_update(&u, &bot(), &[7]) else { panic!() };
        assert!(matches!(e.kind, InboundKind::Press { ref button } if button == "k1.0"));
        assert_eq!(callback, "cb1");
        assert!(matches!(parse_update(&u, &bot(), &[1]), Parsed::Deny(5, 7, _)));
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
        let ch = Telegram { api: RwLock::new(Some(Api::with_host(&format!("http://{addr}"), "T"))), ..Default::default() };
        let id = ch.send("5", &OutMessage::text("**hi**")).await.unwrap();
        assert_eq!(id, "42");
        let seen = seen.lock().unwrap();
        assert!(seen[0].contains("parse_mode") && seen[0].contains("<b>hi</b>"));
        assert!(!seen[1].contains("parse_mode") && seen[1].contains("**hi**"));
    }
}
