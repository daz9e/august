//! Telegram: long polling, an allowlist of user ids, one worker per chat.
//!
//! A turn shows up as two messages: a progress message listing what the agent does (folded
//! into an expandable quote when done) and the answer, streamed as it is written. Messages
//! sent while the agent works get a 👀 and are fed into the running turn.

use crate::agent::{self, App, Cancelled};
use crate::channel::{Channel, Decision, Fut, Input};
use crate::commands;
use crate::config::{self, Config};
use crate::markdown;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PIECE: usize = 3500;

// ---------- Bot API ----------

pub struct Api {
    http: reqwest::Client,
    base: String,
    files: String,
}

impl Api {
    pub fn new(token: &str) -> Self {
        Self {
            http: crate::llm::http_client(),
            base: format!("https://api.telegram.org/bot{token}"),
            files: format!("https://api.telegram.org/file/bot{token}"),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let v: Value = self.http.post(format!("{}/{method}", self.base)).json(&params).send().await?.json().await?;
        if v["ok"] != true {
            bail!("telegram {method}: {}", v["description"].as_str().unwrap_or("unknown error"));
        }
        Ok(v["result"].clone())
    }

    /// Sends HTML, falling back to the plain text if Telegram rejects the markup.
    async fn send_html(&self, chat: i64, html: &str, plain: &str, extra: Value) -> Result<i64> {
        let mut params = json!({"chat_id": chat, "text": html, "parse_mode": "HTML", "link_preview_options": {"is_disabled": true}});
        merge(&mut params, &extra);
        let sent = match self.call("sendMessage", params.clone()).await {
            Ok(v) => v,
            Err(_) => {
                params["text"] = plain.chars().take(4096).collect::<String>().into();
                params.as_object_mut().unwrap().remove("parse_mode");
                self.call("sendMessage", params).await?
            }
        };
        sent["message_id"].as_i64().context("no message_id")
    }

    async fn edit_html(&self, chat: i64, id: i64, html: &str, plain: &str, extra: Value) -> Result<()> {
        let mut params = json!({"chat_id": chat, "message_id": id, "text": html, "parse_mode": "HTML", "link_preview_options": {"is_disabled": true}});
        merge(&mut params, &extra);
        match self.call("editMessageText", params.clone()).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("not modified") => Ok(()),
            Err(_) => {
                params["text"] = plain.chars().take(4096).collect::<String>().into();
                params.as_object_mut().unwrap().remove("parse_mode");
                match self.call("editMessageText", params).await {
                    Err(e) if e.to_string().contains("not modified") => Ok(()),
                    r => r.map(drop),
                }
            }
        }
    }

    /// Sends Markdown in message-sized pieces.
    pub async fn send(&self, chat: i64, md: &str) -> Result<()> {
        for piece in markdown::split_markdown(md, PIECE) {
            self.send_html(chat, &markdown::to_html(&piece), &piece, json!({})).await?;
        }
        Ok(())
    }

    async fn delete(&self, chat: i64, id: i64) {
        self.call("deleteMessage", json!({"chat_id": chat, "message_id": id})).await.ok();
    }

    async fn updates(&self, offset: i64, timeout: u64) -> Result<Vec<Value>> {
        let params = json!({"offset": offset, "timeout": timeout, "allowed_updates": ["message", "callback_query"]});
        Ok(self.call("getUpdates", params).await?.as_array().cloned().unwrap_or_default())
    }

    /// Downloads an attachment into the workspace inbox.
    async fn download(&self, file_id: &str, name: &str) -> Result<PathBuf> {
        let f = self.call("getFile", json!({"file_id": file_id})).await?;
        let remote = f["file_path"].as_str().context("file is too big to download (Telegram's limit is 20 MB)")?;
        let bytes = self.http.get(format!("{}/{remote}", self.files)).send().await?.error_for_status()?.bytes().await?;
        let dir = config::workspace().join("inbox");
        std::fs::create_dir_all(&dir)?;
        let name = if name.is_empty() { remote } else { name };
        let path = dir.join(format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), safe_name(name)));
        std::fs::write(&path, &bytes)?;
        Ok(path)
    }

    async fn upload(&self, chat: i64, path: &Path, caption: &str) -> Result<()> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let photo = matches!(crate::llm::image_type(path), Some("image/jpeg" | "image/png" | "image/webp"));
        let (method, field) = if photo { ("sendPhoto", "photo") } else { ("sendDocument", "document") };
        let part = reqwest::multipart::Part::bytes(std::fs::read(path)?).file_name(name);
        let form = reqwest::multipart::Form::new().text("chat_id", chat.to_string()).text("caption", caption.to_string()).part(field, part);
        let v: Value = self.http.post(format!("{}/{method}", self.base)).multipart(form).send().await?.json().await?;
        if v["ok"] != true {
            bail!("telegram {method}: {}", v["description"].as_str().unwrap_or("unknown error"));
        }
        Ok(())
    }
}

/// The last path component of a sender-chosen file name, so it can't point outside the inbox.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    if base.is_empty() || base == "." || base == ".." { "file".into() } else { base.into() }
}

fn merge(dst: &mut Value, extra: &Value) {
    if let (Some(d), Some(e)) = (dst.as_object_mut(), extra.as_object()) {
        d.extend(e.clone());
    }
}

fn display_name(from: &Value) -> String {
    match from["username"].as_str() {
        Some(u) => format!("@{u}"),
        None => from["first_name"].as_str().unwrap_or("unknown").to_string(),
    }
}

/// Pairing for `august login`: checks the token, then waits for the owner's first private
/// message. Returns the sender's (id, name).
pub async fn pair(token: &str, on_ready: impl FnOnce(&str)) -> Result<(i64, String)> {
    let api = Api::new(token);
    let me = api.call("getMe", json!({})).await.context("Telegram rejected this bot token")?;
    on_ready(me["username"].as_str().unwrap_or_default());
    let mut offset = 0;
    loop {
        for u in api.updates(offset, 30).await? {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let m = &u["message"];
            if m["chat"]["type"] == "private"
                && let Some(id) = m["from"]["id"].as_i64()
            {
                // Acknowledge it so `serve` doesn't answer the pairing message.
                api.updates(offset, 0).await.ok();
                return Ok((id, display_name(&m["from"])));
            }
        }
    }
}

/// The bot's @username, to show in `august status`.
pub async fn username(token: &str) -> Result<String> {
    let me = Api::new(token).call("getMe", json!({})).await?;
    Ok(me["username"].as_str().unwrap_or_default().to_string())
}

// ---------- the bot ----------

pub struct Bot {
    pub api: Api,
    approvals: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
    chats: Mutex<HashMap<i64, Arc<ChatState>>>,
}

enum Job {
    Turn(Input),
    Command(String, String),
}

#[derive(Default)]
struct Inbox {
    busy: bool,
    pending: Vec<Input>,
}

struct ChatState {
    inbox: Mutex<Inbox>,
    cancel: Mutex<CancellationToken>,
    tx: mpsc::UnboundedSender<Job>,
}

impl Bot {
    pub fn new(token: &str) -> Arc<Self> {
        Arc::new(Self { api: Api::new(token), approvals: Default::default(), chats: Default::default() })
    }

    fn chat(self: &Arc<Self>, app: &Arc<App>, chat: i64) -> Arc<ChatState> {
        self.chats
            .lock()
            .unwrap()
            .entry(chat)
            .or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                let state = Arc::new(ChatState { inbox: Default::default(), cancel: Default::default(), tx });
                tokio::spawn(worker(app.clone(), self.clone(), chat, state.clone(), rx));
                state
            })
            .clone()
    }

    /// A channel to a chat for unattended runs (cron): approvals and files, no live output.
    pub fn quiet(self: &Arc<Self>, chat: i64) -> Live {
        Live::new(self.clone(), chat, None, false)
    }
}

pub async fn serve(app: Arc<App>, bot: Arc<Bot>) -> Result<()> {
    let me = bot.api.call("getMe", json!({})).await.context("Telegram rejected the bot token, run `august login`")?;
    agent::log(&format!("telegram: running as @{}", me["username"].as_str().unwrap_or_default()));
    let list: Vec<Value> = commands::LIST.iter().map(|(n, d)| json!({"command": n, "description": d})).collect();
    bot.api.call("setMyCommands", json!({"commands": list})).await.ok();

    let mut offset = 0;
    loop {
        let updates = match bot.api.updates(offset, 30).await {
            Ok(u) => u,
            Err(e) => {
                agent::log(&format!("telegram: {e:#}; retrying in 3s"));
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        let allowed = Config::load().map(|c| c.telegram.allowed).unwrap_or_default();
        for u in updates {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            if !u["callback_query"].is_null() {
                on_button(&app, &bot, &u["callback_query"], &allowed).await;
                continue;
            }
            let m = &u["message"];
            let (Some(chat), Some(user)) = (m["chat"]["id"].as_i64(), m["from"]["id"].as_i64()) else { continue };
            if !allowed.contains(&user) {
                agent::log(&format!("telegram: rejected {} ({user})", display_name(&m["from"])));
                if m["chat"]["type"] == "private" {
                    let text = format!("Sorry, this is a private assistant. Your Telegram user id is {user}.");
                    bot.api.call("sendMessage", json!({"chat_id": chat, "text": text})).await.ok();
                }
                continue;
            }
            let state = bot.chat(&app, chat);
            let text = m["text"].as_str().unwrap_or_default();
            match commands::parse(text) {
                Some(("stop", _)) => {
                    let busy = state.inbox.lock().unwrap().busy;
                    if busy {
                        state.cancel.lock().unwrap().cancel();
                    } else {
                        bot.api.send(chat, "Nothing is running.").await.ok();
                    }
                }
                // These change the conversation, so they wait for the running turn.
                Some((name @ ("new" | "reset" | "compact"), args)) => {
                    let _ = state.tx.send(Job::Command(name.into(), args.into()));
                }
                Some(("resume", _)) => {
                    let list = agent::archived(&format!("tg-{chat}"), 10);
                    if list.is_empty() {
                        bot.api.send(chat, "No earlier conversations yet.").await.ok();
                    } else {
                        let rows: Vec<Vec<Value>> = list.iter().map(|a| vec![json!({"text": a.label, "callback_data": format!("res:{}", a.file)})]).collect();
                        let markup = json!({"reply_markup": {"inline_keyboard": rows}});
                        bot.api.send_html(chat, "<b>Go back to a conversation</b>", "Go back to a conversation", markup).await.ok();
                    }
                }
                Some((kind @ ("model" | "effort"), "")) => {
                    let (app, bot) = (app.clone(), bot.clone());
                    let kind: &'static str = if kind == "model" { "model" } else { "effort" };
                    tokio::spawn(async move { send_picker(&app, &bot, chat, kind).await });
                }
                Some((name, args)) => {
                    let (app, bot, name, args) = (app.clone(), bot.clone(), name.to_string(), args.to_string());
                    tokio::spawn(async move { reply_command(&app, &bot, chat, &name, &args).await });
                }
                None => {
                    let (bot, m) = (bot.clone(), m.clone());
                    tokio::spawn(async move {
                        match read_message(&bot, &m).await {
                            Ok(Some(input)) => enqueue(&bot, &state, chat, m["message_id"].as_i64(), input).await,
                            Ok(None) => {}
                            Err(e) => {
                                bot.api.send(chat, &format!("⚠️ Could not read that message: {e:#}")).await.ok();
                            }
                        }
                    });
                }
            }
        }
    }
}

async fn reply_command(app: &Arc<App>, bot: &Bot, chat: i64, name: &str, args: &str) {
    if name == "update" && args.is_empty() {
        bot.api.send(chat, "Checking for updates…").await.ok();
    }
    let text = match commands::run(app, &format!("tg-{chat}"), name, args).await {
        Ok(t) => t,
        Err(e) => format!("⚠️ {e:#}"),
    };
    bot.api.send(chat, &text).await.ok();
}

/// Starts a turn, or queues the message for the running one.
async fn enqueue(bot: &Bot, state: &ChatState, chat: i64, message_id: Option<i64>, input: Input) {
    let queued = {
        let mut inbox = state.inbox.lock().unwrap();
        if inbox.busy {
            inbox.pending.push(input);
            true
        } else {
            inbox.busy = true;
            let _ = state.tx.send(Job::Turn(input));
            false
        }
    };
    if queued && let Some(id) = message_id {
        let reaction = json!({"chat_id": chat, "message_id": id, "reaction": [{"type": "emoji", "emoji": "👀"}]});
        bot.api.call("setMessageReaction", reaction).await.ok();
    }
}

/// Turns a Telegram message into input: text, downloaded files, transcribed voice.
async fn read_message(bot: &Bot, m: &Value) -> Result<Option<Input>> {
    let mut input = Input::text(m["text"].as_str().or(m["caption"].as_str()).unwrap_or_default());
    let note = |input: &mut Input, line: String| {
        input.text = if input.text.is_empty() { line } else { format!("{}\n{line}", input.text) };
    };
    if let Some(sizes) = m["photo"].as_array() {
        let largest = sizes.last().context("empty photo")?;
        let path = bot.api.download(largest["file_id"].as_str().unwrap_or_default(), "photo.jpg").await?;
        note(&mut input, format!("[photo saved to {}]", path.display()));
        input.images.push(path);
    }
    if !m["document"].is_null() {
        let d = &m["document"];
        let path = bot.api.download(d["file_id"].as_str().unwrap_or_default(), d["file_name"].as_str().unwrap_or("file")).await?;
        note(&mut input, format!("[file saved to {}]", path.display()));
        if crate::llm::image_type(&path).is_some() {
            input.images.push(path);
        }
    }
    for kind in ["voice", "audio", "video", "video_note"] {
        let f = &m[kind];
        if f.is_null() {
            continue;
        }
        let name = f["file_name"].as_str().unwrap_or(if kind == "voice" { "voice.ogg" } else { "" });
        let path = bot.api.download(f["file_id"].as_str().unwrap_or_default(), name).await?;
        if !matches!(kind, "voice" | "audio") {
            note(&mut input, format!("[{kind} saved to {}]", path.display()));
            continue;
        }
        match transcribe(&bot.api.http, &path).await {
            Ok(Some(text)) => {
                let echo = format!("🎤 <i>{}</i>", markdown::escape(&text));
                let reply = json!({"reply_parameters": {"message_id": m["message_id"], "allow_sending_without_reply": true}, "disable_notification": true});
                bot.api.send_html(m["chat"]["id"].as_i64().unwrap_or_default(), &echo, &text, reply).await.ok();
                note(&mut input, format!("[voice message, transcribed]: {text}"));
            }
            Ok(None) => note(
                &mut input,
                format!("[{kind} message saved to {}; no transcription is configured, so transcribe it yourself if you can, otherwise ask the user to type]", path.display()),
            ),
            Err(e) => note(&mut input, format!("[{kind} message saved to {}; transcription failed: {e:#}]", path.display())),
        }
    }
    if let Some(emoji) = m["sticker"]["emoji"].as_str() {
        note(&mut input, format!("[sticker {emoji}]"));
    }
    if !m["location"].is_null() {
        note(&mut input, format!("[location {}, {}]", m["location"]["latitude"], m["location"]["longitude"]));
    }
    Ok((!input.text.is_empty() || !input.images.is_empty()).then_some(input))
}

/// Speech to text with the OpenAI API; `None` when no key is configured.
async fn transcribe(http: &reqwest::Client, path: &Path) -> Result<Option<String>> {
    let cfg = Config::load()?;
    let Some(key) = cfg.secrets.openai_key else { return Ok(None) };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "audio.ogg".into());
    let name = if name.ends_with(".oga") { name.replace(".oga", ".ogg") } else { name };
    let form = reqwest::multipart::Form::new()
        .text("model", cfg.voice.model)
        .part("file", reqwest::multipart::Part::bytes(std::fs::read(path)?).file_name(name));
    let resp = http.post("https://api.openai.com/v1/audio/transcriptions").bearer_auth(key).multipart(form).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", v["error"]["message"].as_str().unwrap_or_default());
    }
    Ok(Some(v["text"].as_str().unwrap_or_default().trim().to_string()))
}

/// Buttons to switch the model or reasoning effort.
async fn send_picker(app: &App, bot: &Bot, chat: i64, kind: &str) {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => return drop(bot.api.send(chat, &format!("⚠️ {e:#}")).await),
    };
    let items: Vec<String> = if kind == "model" {
        match app.llm.models(&cfg).await {
            Ok(m) => m,
            Err(e) => return drop(bot.api.send(chat, &format!("⚠️ Could not list models: {e:#}")).await),
        }
    } else {
        crate::llm::efforts(&cfg.model.provider).iter().map(|s| s.to_string()).collect()
    };
    let current = if kind == "model" { &cfg.model.name } else { &cfg.model.effort };
    let button = |i: &String| {
        let label = if i == current { format!("✓ {i}") } else { i.clone() };
        json!({"text": label, "callback_data": format!("set:{kind}:{i}")})
    };
    // Callback data is capped at 64 bytes.
    let fits: Vec<&String> = items.iter().filter(|i| i.len() + kind.len() + 5 <= 64).take(40).collect();
    let rows: Vec<Vec<Value>> = if kind == "model" { fits.into_iter().map(|i| vec![button(i)]).collect() } else { vec![fits.into_iter().map(button).collect()] };
    let title = if kind == "model" { "Choose a model" } else { "Choose reasoning effort" };
    let html = format!("<b>{title}</b>\nNow: <code>{}</code>", markdown::escape(current));
    bot.api.send_html(chat, &html, title, json!({"reply_markup": {"inline_keyboard": rows}})).await.ok();
}

async fn on_button(app: &App, bot: &Bot, q: &Value, allowed: &[i64]) {
    let id = q["id"].as_str().unwrap_or_default();
    let data = q["data"].as_str().unwrap_or_default();
    let chat = q["message"]["chat"]["id"].as_i64().unwrap_or_default();
    let message = q["message"]["message_id"].as_i64().unwrap_or_default();
    if !q["from"]["id"].as_i64().is_some_and(|u| allowed.contains(&u)) {
        bot.api.call("answerCallbackQuery", json!({"callback_query_id": id, "text": "Not allowed"})).await.ok();
        return;
    }
    if let Some((kind, value)) = data.strip_prefix("set:").and_then(|d| d.split_once(':')) {
        let res = Config::load().and_then(|mut cfg| {
            if kind == "model" {
                cfg.model.name = value.into();
            } else {
                cfg.model.effort = value.into();
            }
            cfg.save()
        });
        let text = match res {
            Ok(()) => format!("{}: <code>{}</code> ✓", if kind == "model" { "Model" } else { "Effort" }, markdown::escape(value)),
            Err(e) => format!("⚠️ {}", markdown::escape(&format!("{e:#}"))),
        };
        bot.api.call("answerCallbackQuery", json!({"callback_query_id": id})).await.ok();
        bot.api.edit_html(chat, message, &text, value, json!({})).await.ok();
        return;
    }
    if let Some(file) = data.strip_prefix("res:") {
        let busy = bot.chats.lock().unwrap().get(&chat).is_some_and(|c| c.inbox.lock().unwrap().busy);
        if busy {
            bot.api.call("answerCallbackQuery", json!({"callback_query_id": id, "text": "Wait for the current reply to finish"})).await.ok();
            return;
        }
        let session = format!("tg-{chat}");
        let text = match agent::restore(&session, file) {
            Ok(()) => {
                app.reset(&session);
                let n = agent::Session::load(&session).map(|s| s.messages.len()).unwrap_or(0);
                format!("↩️ Back in that conversation ({n} messages). Carry on.")
            }
            Err(e) => format!("⚠️ {}", markdown::escape(&format!("{e:#}"))),
        };
        bot.api.call("answerCallbackQuery", json!({"callback_query_id": id})).await.ok();
        bot.api.edit_html(chat, message, &text, &text, json!({})).await.ok();
        return;
    }
    let Some((key, choice)) = data.strip_prefix("ok:").and_then(|d| d.split_once(':')) else { return };
    let sender = bot.approvals.lock().unwrap().remove(key);
    let (decision, label) = match choice {
        "y" => (Decision::Yes, "✅ Allowed"),
        "a" => (Decision::Always, "✅ Allowed for the rest of this conversation"),
        _ => (Decision::No, "⛔ Denied"),
    };
    let label = match sender {
        Some(tx) => {
            let _ = tx.send(decision);
            label
        }
        None => "⌛ This request has expired",
    };
    bot.api.call("answerCallbackQuery", json!({"callback_query_id": id})).await.ok();
    let original = q["message"]["text"].as_str().unwrap_or_default();
    let command = original.split_once('\n').map(|(_, rest)| rest).unwrap_or(original);
    let html = format!("<pre>{}</pre>\n{label}", markdown::escape(command));
    bot.api.edit_html(chat, message, &html, &format!("{command}\n{label}"), json!({})).await.ok();
}

async fn worker(app: Arc<App>, bot: Arc<Bot>, chat: i64, state: Arc<ChatState>, mut rx: mpsc::UnboundedReceiver<Job>) {
    let session = format!("tg-{chat}");
    while let Some(job) = rx.recv().await {
        match job {
            Job::Command(name, args) => reply_command(&app, &bot, chat, &name, &args).await,
            Job::Turn(mut input) => loop {
                run_turn(&app, &bot, chat, &session, &state, input).await;
                // Messages that arrived after the turn's last step start the next one.
                let next = {
                    let mut inbox = state.inbox.lock().unwrap();
                    if inbox.pending.is_empty() {
                        inbox.busy = false;
                        None
                    } else {
                        Some(Input::merge(std::mem::take(&mut inbox.pending)))
                    }
                };
                match next {
                    Some(i) => input = i,
                    None => break,
                }
            },
        }
    }
}

async fn run_turn(app: &Arc<App>, bot: &Arc<Bot>, chat: i64, session: &str, state: &Arc<ChatState>, input: Input) {
    let cancel = CancellationToken::new();
    *state.cancel.lock().unwrap() = cancel.clone();
    let live = Arc::new(Live::new(bot.clone(), chat, Some(state.clone()), true));
    let ticker = tokio::spawn(live.clone().tick());
    let res = agent::turn(app, session, input, &cancel, &*live).await;
    ticker.abort();
    if let Err(e) = &res
        && e.downcast_ref::<Cancelled>().is_none()
    {
        agent::log(&format!("{session}: {e:#}"));
    }
    live.finish(res).await;
}

// ---------- a turn's live messages ----------

#[derive(Default)]
struct View {
    /// Progress lines, already HTML.
    steps: Vec<String>,
    /// Text of the current step, streamed into the answer message.
    answer: String,
    progress_id: Option<i64>,
    answer_id: Option<i64>,
    /// Bumped when the answer text moves into the progress list.
    generation: u64,
    delete: Vec<i64>,
    dirty: bool,
    drawn: Option<Instant>,
}

pub struct Live {
    bot: Arc<Bot>,
    chat: i64,
    state: Option<Arc<ChatState>>,
    stream: bool,
    started: Instant,
    view: Mutex<View>,
    flushing: tokio::sync::Mutex<()>,
}

fn step_html(summary: &str) -> String {
    let (name, arg) = summary.split_once(": ").unwrap_or((summary, ""));
    let arg: String = arg.chars().take(120).collect();
    format!("▸ <b>{}</b> <code>{}</code>", markdown::escape(name), markdown::escape(&arg))
}

impl Live {
    fn new(bot: Arc<Bot>, chat: i64, state: Option<Arc<ChatState>>, stream: bool) -> Self {
        Self { bot, chat, state, stream, started: Instant::now(), view: Default::default(), flushing: Default::default() }
    }

    /// Redraws about once a second (Telegram's edit rate) and keeps "typing…" up.
    async fn tick(self: Arc<Self>) {
        let mut n = 0u32;
        loop {
            if n.is_multiple_of(4) {
                self.bot.api.call("sendChatAction", json!({"chat_id": self.chat, "action": "typing"})).await.ok();
            }
            self.flush(None).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            n += 1;
        }
    }

    fn header(&self, done: Option<&str>) -> String {
        let secs = self.started.elapsed().as_secs();
        match done {
            None => format!("⏳ <b>Working</b> · {secs}s"),
            Some(status) => format!("{status} · {secs}s"),
        }
    }

    /// Pushes the current view to Telegram. `done` folds the progress list under that status.
    async fn flush(&self, done: Option<&str>) {
        let _guard = self.flushing.lock().await;
        let (steps, answer, progress_id, answer_id, generation, delete) = {
            let mut v = self.view.lock().unwrap();
            // The progress header shows elapsed time, so it is also redrawn every few seconds.
            let stale = !v.steps.is_empty() && v.drawn.is_none_or(|t| t.elapsed() >= Duration::from_secs(3));
            if !v.dirty && done.is_none() && !stale {
                return;
            }
            v.dirty = false;
            v.drawn = Some(Instant::now());
            (v.steps.clone(), v.answer.clone(), v.progress_id, v.answer_id, v.generation, std::mem::take(&mut v.delete))
        };
        let api = &self.bot.api;
        for id in delete {
            api.delete(self.chat, id).await;
        }
        if !steps.is_empty() {
            let html = match done {
                None => {
                    let hidden = steps.len().saturating_sub(12);
                    let mut lines: Vec<String> = steps.iter().skip(hidden).cloned().collect();
                    if hidden > 0 {
                        lines.insert(0, format!("<i>… {hidden} earlier</i>"));
                    }
                    format!("{}\n{}", self.header(None), lines.join("\n"))
                }
                Some(status) => {
                    let mut body = steps.join("\n");
                    while body.chars().count() > PIECE && body.contains('\n') {
                        body = body.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default();
                    }
                    format!("{}\n<blockquote expandable>{body}</blockquote>", self.header(Some(status)))
                }
            };
            let plain = "Working…";
            match progress_id {
                Some(id) => {
                    api.edit_html(self.chat, id, &html, plain, json!({})).await.ok();
                }
                None => {
                    if let Ok(id) = api.send_html(self.chat, &html, plain, json!({"disable_notification": true})).await {
                        self.view.lock().unwrap().progress_id = Some(id);
                    }
                }
            }
        }
        if done.is_none() && self.stream && !answer.trim().is_empty() {
            let preview: String = if answer.chars().count() > PIECE {
                answer.chars().take(PIECE).collect::<String>() + " …"
            } else {
                answer.clone()
            };
            let html = markdown::to_html(&preview);
            match answer_id {
                Some(id) => {
                    api.edit_html(self.chat, id, &html, &preview, json!({})).await.ok();
                }
                None => {
                    if let Ok(id) = api.send_html(self.chat, &html, &preview, json!({})).await {
                        let mut v = self.view.lock().unwrap();
                        if v.generation == generation {
                            v.answer_id = Some(id);
                        } else {
                            v.delete.push(id); // the text became a progress line meanwhile
                            v.dirty = true;
                        }
                    }
                }
            }
        }
    }

    async fn finish(&self, res: Result<String>) {
        let status = match &res {
            Ok(_) => {
                let n = self.view.lock().unwrap().steps.len();
                format!("✓ {n} step{}", if n == 1 { "" } else { "s" })
            }
            Err(e) if e.downcast_ref::<Cancelled>().is_some() => "◼ Stopped".into(),
            Err(_) => "✗ Failed".into(),
        };
        self.flush(Some(&status)).await;
        let _guard = self.flushing.lock().await;
        let (answer_id, partial) = {
            let mut v = self.view.lock().unwrap();
            (v.answer_id.take(), std::mem::take(&mut v.answer))
        };
        let api = &self.bot.api;
        let text = match res {
            Ok(reply) => reply,
            Err(e) if e.downcast_ref::<Cancelled>().is_some() => {
                if partial.trim().is_empty() { "◼ Stopped.".into() } else { format!("{partial}\n\n◼ _stopped_") }
            }
            Err(e) => {
                let msg: String = format!("{e:#}").chars().take(1000).collect();
                if let Some(id) = answer_id {
                    api.delete(self.chat, id).await;
                }
                api.send(self.chat, &format!("⚠️ {msg}")).await.ok();
                return;
            }
        };
        if text.trim().is_empty() {
            if let Some(id) = answer_id {
                api.delete(self.chat, id).await;
            }
            return;
        }
        let pieces = markdown::split_markdown(&text, PIECE);
        match answer_id {
            Some(id) if pieces.len() == 1 => {
                api.edit_html(self.chat, id, &markdown::to_html(&pieces[0]), &pieces[0], json!({})).await.ok();
            }
            _ => {
                if let Some(id) = answer_id {
                    api.delete(self.chat, id).await;
                }
                for p in pieces {
                    api.send_html(self.chat, &markdown::to_html(&p), &p, json!({})).await.ok();
                }
            }
        }
    }
}

impl Channel for Live {
    fn text(&self, delta: &str) {
        let mut v = self.view.lock().unwrap();
        v.answer.push_str(delta);
        v.dirty = true;
    }

    fn tool(&self, summary: &str) {
        let mut v = self.view.lock().unwrap();
        // Text before a tool call is commentary: it moves into the progress list.
        let said = std::mem::take(&mut v.answer);
        let said = said.trim();
        if !said.is_empty() {
            let short: String = said.chars().take(300).collect();
            let ellipsis = if said.chars().count() > 300 { "…" } else { "" };
            v.steps.push(format!("<i>{}{ellipsis}</i>", markdown::escape(&short)));
        }
        if let Some(id) = v.answer_id.take() {
            v.delete.push(id);
        }
        v.generation += 1;
        v.steps.push(step_html(summary));
        v.dirty = true;
    }

    fn tool_done(&self, _output: &str, is_error: bool) {
        if is_error {
            let mut v = self.view.lock().unwrap();
            if let Some(last) = v.steps.last_mut() {
                last.push_str(" ✗");
            }
            v.dirty = true;
        }
    }

    fn steer(&self) -> Vec<Input> {
        match &self.state {
            Some(s) => std::mem::take(&mut s.inbox.lock().unwrap().pending),
            None => Vec::new(),
        }
    }

    fn approve<'a>(&'a self, command: &'a str, reason: &'a str) -> Fut<'a, Decision> {
        Box::pin(async move {
            let mut buf = [0u8; 6];
            getrandom::fill(&mut buf).expect("os rng");
            let key: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            let (tx, rx) = oneshot::channel();
            self.bot.approvals.lock().unwrap().insert(key.clone(), tx);
            let shown: String = command.chars().take(1500).collect();
            let html = format!("🔐 <b>Allow this command?</b> <i>({})</i>\n<pre>{}</pre>", markdown::escape(reason), markdown::escape(&shown));
            let buttons = json!({"reply_markup": {"inline_keyboard": [[
                {"text": "Allow", "callback_data": format!("ok:{key}:y")},
                {"text": "Always", "callback_data": format!("ok:{key}:a")},
                {"text": "Deny", "callback_data": format!("ok:{key}:n")},
            ]]}});
            let plain = format!("Allow this command? ({reason})\n{shown}");
            let Ok(message) = self.bot.api.send_html(self.chat, &html, &plain, buttons).await else {
                self.bot.approvals.lock().unwrap().remove(&key);
                return Decision::No;
            };
            match tokio::time::timeout(Duration::from_secs(900), rx).await {
                Ok(Ok(d)) => d,
                _ => {
                    self.bot.approvals.lock().unwrap().remove(&key);
                    let html = format!("<pre>{}</pre>\n⌛ No answer, denied", markdown::escape(&shown));
                    self.bot.api.edit_html(self.chat, message, &html, &shown, json!({})).await.ok();
                    Decision::No
                }
            }
        })
    }

    fn send_file<'a>(&'a self, path: &'a Path, caption: &'a str) -> Fut<'a, Result<String>> {
        Box::pin(async move {
            self.bot.api.upload(self.chat, path, caption).await?;
            Ok(format!("sent {} to the user", path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_names_stay_in_the_inbox() {
        use super::safe_name;
        assert_eq!(safe_name("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(safe_name("..\\..\\x.txt"), "x.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("photos/file_12.jpg"), "file_12.jpg");
        assert_eq!(safe_name("report.pdf"), "report.pdf");
    }
}
