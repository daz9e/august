//! The core's operations: one table of everything August can be asked to do, each with the
//! permission it needs. Extensions call them over their protocol, slash commands are thin
//! wrappers around them; whoever calls, the same code runs.

use super::{Gateway, waits};
use crate::extensions::{self, Origin};
use crate::llm::{Message, providers};
use crate::messengers::{Button, Messenger, OutMessage, Thread};
use crate::tools::ToolCtx;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct Op {
    pub name: &'static str,
    /// What an extension must declare (`needs`) to call it; `None`: anyone may.
    pub permission: Option<&'static str>,
    pub about: &'static str,
}

const fn op(name: &'static str, permission: Option<&'static str>, about: &'static str) -> Op {
    Op { name, permission, about }
}

const MESSAGING: Option<&str> = Some("messaging");
const TURNS: Option<&str> = Some("turns");
const SESSIONS: Option<&str> = Some("sessions");

pub const OPS: &[Op] = &[
    op("ops", None, "Every operation with the permission it needs"),
    op("guide", None, "How to write extensions: the guide with the API's types (Markdown)"),
    op("tools", None, "Every agent tool: {name, description, parameters, owner}"),
    op("commands", None, "Every slash command: {name, description, owner}"),
    op("status", None, "{provider, model, workspace, busy} (busy: of `thread`, if given)"),
    op("messengers", MESSAGING, "Every messenger with its capabilities, notes, actions and threads (each with its place: dm|group|channel|thread, parent, title)"),
    op("send", MESSAGING, "Send {thread, message}; returns its id"),
    op("edit", MESSAGING, "Replace a sent message {thread, id, message}"),
    op("delete", MESSAGING, "Delete a sent message {thread, id}"),
    op("react", MESSAGING, "React to a message {thread, id, emoji}"),
    op("open_thread", MESSAGING, "Open a thread titled {title} inside {thread} (where the messenger can `open_thread`); returns the new thread"),
    op("action", MESSAGING, "Run one of the messenger's described {action}s in {thread} with {args} (checked against its schema); returns its result"),
    op("download", MESSAGING, "Save attachment {file} of a message in {thread} (one of `message_in`'s `files`) to {path} (relative: in the workspace); returns {path, size}"),
    op("inbound", None, "Hand in what came to {thread} of a messenger you offer from {user: {id, name}} at {place: {kind: dm|group|channel|thread, parent, title}}: {kind: message|edited|command|press|reaction, ...}"),
    op("listen", MESSAGING, "Listen in {thread} for {buttons, text}; returns a listener id. With {secret} a text taken is deleted from the chat"),
    op("next", MESSAGING, "What {listener} took"),
    op("prompt", MESSAGING, "Hand {thread} a message as if the user sent it"),
    op("turn_start", TURNS, "Start a turn {thread, turn: {text, mode: visible|quiet|fork|fresh, source, parent, ...}}; returns its id"),
    op("turn_wait", TURNS, "A turn's outcome {id, timeout_ms}"),
    op("turn_cancel", TURNS, "Cancel turn {id}"),
    op("turns", TURNS, "Running turns, of {thread} or all"),
    op("stop", TURNS, "Stop everything running in {thread}, like /stop"),
    op("callTool", Some("tools"), "Run an agent tool {thread, name, input}"),
    op("llm", Some("llm"), "One completion without tools: {prompt} or {messages}, {system}, {effort}, {options} (fields of the provider's request body); in {thread}'s conversation's model, counted in its usage"),
    op("model_set", Some("models"), "Switch to {model} of the current provider, or `provider:model` (`provider:` for its default)"),
    op("providers", Some("models"), "Every model provider: {id, label, default_model, extension}"),
    op("models", Some("models"), "The models {provider} offers (default: the active one): [{id, context_window}]"),
    op("accounts", Some("admin"), "Every account extensions sign in to: {id, label, extension, providers, kind: key|login, status: none|connected|expired, who}"),
    op("login", Some("admin"), "Sign in to {account}, asking in {thread}; returns {who}"),
    op("logout", Some("admin"), "Sign out of {account}"),
    op("account_update", None, "Tell how your {account} stands: {status: none|connected|expired, who}"),
    op("secret_get", None, "Your secret {key} (an account's API key is under the account's id)"),
    op("secret_set", None, "Keep your secret {key, value} (null deletes)"),
    op("login_ask", None, "In your sign-in {session}: ask for {label} ({secret}: deleted from the chat); returns the answer"),
    op("login_choose", None, "In your sign-in {session}: {question} with {options} as buttons; returns the one picked"),
    op("login_open", None, "In your sign-in {session}: show {url} to open, with {note}"),
    op("login_progress", None, "In your sign-in {session}: say {text}"),
    op("login_callback", None, "In your sign-in {session}: receive one redirect on localhost {port} (0: any) {path}; returns its address"),
    op("login_wait", None, "In your sign-in {session}: the redirect's query {timeout_ms} (or the address the user pastes)"),
    op("sessions", SESSIONS, "Stored conversations, newest first, of {thread} or all: {id, chat, name, settings, messages, bound}"),
    op("session_new", SESSIONS, "Start a new conversation in {thread} (none: of no chat, addressed as thread {messenger: \"session\", id}), optionally with {name, settings}; returns its id"),
    op("session_update", SESSIONS, "Rename {session} ({name}) or change its {settings}: {model, system, tools}, null deletes"),
    op("session_switch", SESSIONS, "Continue stored {session} in {thread}"),
    op("history", SESSIONS, "The journal of {session} (or {thread}'s, or all): entries after {since} of {kinds}, at most {limit} (default 100)"),
    op("journal_append", None, "Record {type, data} in the journal of {session} or {thread}'s (kind `custom`, caller you)"),
    op("messages", SESSIONS, "The live conversation of {thread} (what the model sees next): {session, messages, tokens, window}"),
    op("messages_set", SESSIONS, "Replace the live conversation of {thread} with {messages} (earlier ones stay searchable)"),
    op("search", SESSIONS, "Full-text search over everything said in any conversation: {query, limit} → [{at, role, text}]"),
    op("extensions", Some("admin"), "Every extension with its state and what it registers"),
    op("extension_enable", Some("admin"), "(Re)start {name} and keep it enabled; returns its status line, fails with its error"),
    op("extension_disable", Some("admin"), "Stop {name} and keep it disabled"),
    op("extensions_reload", Some("admin"), "Restart every extension but the caller"),
    op("extension_logs", Some("admin"), "The last {lines} (default 100) lines of extension {name}'s log: its stderr and the core's notes, also while it is down"),
    op("extension_health", Some("admin"), "Ask extension {name} how it is doing now: {status: ok|degraded|failed|hung, detail}"),
    op("settings", None, "The calling extension's settings, schema defaults filled in"),
    op("settings_set", None, "Set {path, value} in the calling extension's settings (null deletes)"),
    op("config_list", Some("config"), "Every known setting under {prefix} (default all): [{path, description, type, default, value, secret}]; secrets masked"),
    op("config_get", Some("config"), "A setting of any unit at {path} (`august.model`, `extensions.web.settings`); secrets masked"),
    op("config_set", Some("config"), "Change the setting at {path} to {value} (null deletes); `enabled` and `origin` are the user's"),
    op("emit", None, "Run your declared event {event, data} through its handlers; returns the data they leave (an observed event: at once)"),
    op("store_get", None, "The caller's stored value at {key}"),
    op("store_set", None, "Store {key, value} (null deletes)"),
    op("store_list", None, "The caller's stored entries under {prefix}"),
];

pub fn find(name: &str) -> Option<&'static Op> {
    OPS.iter().find(|o| o.name == name)
}

pub fn thread(params: &Value) -> Result<Thread> {
    let t = &params["thread"];
    match (t["messenger"].as_str(), t["id"].as_str()) {
        (Some(m), Some(id)) => Ok(Thread::new(m, id)),
        _ => bail!("this needs a thread ({{messenger, id}}); the call has none"),
    }
}

pub(super) fn message(params: &Value) -> Result<OutMessage> {
    let m = &params["message"];
    let text = m.as_str().or(m["text"].as_str()).ok_or_else(|| anyhow!("missing `message.text`"))?;
    let button = |b: &Value| match (b["id"].as_str(), b["label"].as_str()) {
        (Some(id), Some(label)) => Ok(Button { id: id.into(), label: label.into() }),
        _ => Err(anyhow!("a button needs `id` and `label`")),
    };
    // Rows of buttons, or one flat list as a single row.
    let list = m["buttons"].as_array().cloned().unwrap_or_default();
    let buttons = if list.iter().all(Value::is_array) {
        list.iter().map(|row| row.as_array().into_iter().flatten().map(button).collect()).collect::<Result<_>>()?
    } else {
        vec![list.iter().map(button).collect::<Result<_>>()?]
    };
    let files = m["files"].as_array().into_iter().flatten().filter_map(Value::as_str).map(std::path::PathBuf::from).collect();
    let reply_to = m["reply_to"].as_str().map(String::from);
    Ok(OutMessage { text: text.into(), buttons, files, reply_to })
}

impl Gateway {
    /// Runs operation `name` for extension `ext`, whose permissions its host has checked;
    /// with `as_user` (a slash command, say) it acts for the user.
    pub(crate) async fn op(self: &Arc<Self>, ext: &str, name: &str, p: &Value) -> Result<Value> {
        // As hooks and the journal show who did something.
        let caller = if p["as_user"] == true { "user".to_string() } else { format!("ext:{ext}") };
        // `"thread": "home"` is the user's home thread.
        let home;
        let p = if p["thread"] == "home" {
            let t = self.home()?;
            home = Value::Object(p.as_object().cloned().unwrap_or_default().into_iter().chain([("thread".into(), json!({"messenger": t.messenger, "id": t.id}))]).collect());
            &home
        } else {
            p
        };
        let arg = |k: &str| p[k].as_str().ok_or_else(|| anyhow!("missing string `{k}`"));
        let ms = |k: &str, default: u64| Duration::from_millis(p[k].as_u64().unwrap_or(default));
        Ok(match name {
            "ops" => Value::Array(OPS.iter().map(|o| json!({"name": o.name, "permission": o.permission, "about": o.about})).collect()),
            "guide" => json!(extensions::guide()),
            "tools" => json!(self.tool_list()),
            "commands" => Value::Array(self.command_list().into_iter().map(|(c, owner)| json!({"name": c.name, "description": c.description, "owner": owner})).collect()),
            "status" => {
                let busy = thread(p).ok().map(|t| self.turns.busy(&t));
                let m = self.model.read().unwrap().clone();
                json!({"provider": m.0, "model": m.1, "workspace": self.workspace, "busy": busy})
            }
            "messengers" => self.messengers().await,
            "download" => {
                let t = thread(p)?;
                let file: crate::messengers::Attachment = serde_json::from_value(p["file"].clone()).map_err(|e| anyhow!("bad `file`: {e}"))?;
                let path = self.workspace.join(arg("path")?);
                let bytes = self.messenger(&t)?.download(&file).await?;
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::write(&path, &bytes).await?;
                json!({"path": path, "size": bytes.len()})
            }
            "send" | "edit" | "delete" | "react" => {
                let t = thread(p)?;
                let m = self.messenger(&t)?;
                match name {
                    "send" => json!(self.send_in_order(&t, message(p)?, ext).await?),
                    "edit" => m.edit(&t.id, arg("id")?, &message(p)?).await.map(|_| Value::Null)?,
                    "delete" => m.delete(&t.id, arg("id")?).await.map(|_| Value::Null)?,
                    _ => m.react(&t.id, arg("id")?, p["emoji"].as_str().unwrap_or("")).await.map(|_| Value::Null)?,
                }
            }
            "open_thread" => {
                let t = thread(p)?;
                let id = self.messenger(&t)?.open_thread(&t.id, arg("title")?).await?;
                json!({"messenger": t.messenger, "id": id})
            }
            "action" => {
                let t = thread(p)?;
                let m = self.messenger(&t)?;
                let name = arg("action")?;
                let d = m.describe();
                let spec = d.actions.iter().find(|a| a.name == name).ok_or_else(|| anyhow!("{} has no action `{name}`", d.name))?;
                let args = if p["args"].is_null() { json!({}) } else { p["args"].clone() };
                crate::extensions::check(&spec.input_schema, &args).map_err(|e| anyhow!("`{name}` arguments: {e}"))?;
                m.action(&t.id, name, args).await?
            }
            "inbound" => {
                let t = thread(p)?;
                anyhow::ensure!(self.ext.messenger_owner(&t.messenger).await.as_deref() == Some(ext), "`{}` is not a messenger you offer", t.messenger);
                let place = if p["place"].is_null() { Default::default() } else { serde_json::from_value(p["place"].clone()).map_err(|e| anyhow!("bad `place`: {e}"))? };
                let ev = crate::messengers::Inbound { thread: t, place, user: serde_json::from_value(p["user"].clone())?, kind: serde_json::from_value(p.clone())? };
                let gw = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = gw.handle(ev).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                Value::Null
            }
            "listen" => {
                let buttons: Vec<String> = serde_json::from_value(p["buttons"].clone()).unwrap_or_default();
                let accept = waits::Accept { buttons, text: p["text"] == true, secret: p["secret"] == true };
                json!(self.listen(thread(p)?, accept, ms("ttl_ms", 600_000)))
            }
            "next" => self.next(p["listener"].as_u64().ok_or_else(|| anyhow!("missing `listener`"))?, ms("timeout_ms", 300_000)).await?,
            "prompt" => {
                let t = thread(p)?;
                let m = self.messenger(&t)?;
                let source = p["source"].as_str().map(String::from).unwrap_or_else(|| caller.clone());
                let deliver = p["deliver"].as_str().unwrap_or("steer");
                anyhow::ensure!(["steer", "followUp", "nextTurn"].contains(&deliver), "`deliver` is steer, followUp or nextTurn");
                let message = json!({"text": arg("text")?, "source": source, "deliver": deliver});
                let gw = self.clone();
                // Not awaited: the caller may be inside a turn of that very thread.
                tokio::spawn(async move {
                    if let Err(e) = gw.deliver(m, t, message).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                Value::Null
            }
            "turn_start" => {
                let mut req: super::turns::TurnRequest = serde_json::from_value(p["turn"].clone())?;
                req.thread = Some(thread(p)?);
                req.source.get_or_insert(caller.clone());
                json!(self.start_turn(req)?)
            }
            "turn_wait" => self.wait_turn(p["id"].as_u64().ok_or_else(|| anyhow!("missing `id`"))?, ms("timeout_ms", 3_600_000)).await?,
            "turn_cancel" => json!(self.turns.cancel(p["id"].as_u64().unwrap_or(0))),
            "turns" => self.turns.list(thread(p).ok().as_ref()),
            "stop" => json!({"cancelled": self.stop(&thread(p)?).await}),
            "callTool" => {
                let (output, is_error) = self.call_tool(self.origin(p)?, arg("name")?, &p["input"], &caller).await?;
                json!({"output": output, "isError": is_error})
            }
            "llm" => {
                let system = p["system"].as_str().filter(|s| !s.is_empty()).unwrap_or("You are a helpful assistant.");
                let messages = match p["messages"].as_array() {
                    Some(list) => list.iter().map(Message::from_json).collect::<Option<Vec<_>>>().ok_or_else(|| anyhow!("malformed `messages`"))?,
                    None => vec![Message::user_text(arg("prompt")?)],
                };
                // In a thread: its conversation's model, and `llm_result` counts the call there.
                let session = match thread(p) {
                    Ok(t) => self.db.current_session(&t.key())?,
                    Err(_) => None,
                };
                let model = match &session {
                    Some(s) => self.db.session_settings(s)?["model"].as_str().map(String::from),
                    None => None,
                };
                let provider = match model {
                    Some(m) => providers::build_spec(&m)?,
                    None => self.provider.read().unwrap().clone(),
                };
                let effort = p["effort"].as_str();
                let provider = match effort.is_some() || p["options"].is_object() {
                    true => provider.tuned(effort, &p["options"]).unwrap_or(provider),
                    false => provider,
                };
                let c = provider.complete(&crate::util::new_uuid(), system, &messages, &[]).await?;
                if self.ext.listens("llm_result") {
                    let (ext, data, origin) = (self.ext.clone(), crate::agent::llm_result(None, session.as_deref(), &c), self.origin(p).unwrap_or_default());
                    tokio::spawn(async move { ext.emit("llm_result", data, &origin).await });
                }
                json!(c.message.text())
            }
            "model_set" => {
                let (provider, model) = self.switch_model(arg("model")?).await?;
                json!({"provider": provider, "model": model})
            }
            "providers" => Value::Array(
                self.ext
                    .providers()
                    .into_iter()
                    .map(|(ext, p)| json!({"id": p.id, "label": p.label, "default_model": p.default_model, "extension": ext}))
                    .collect(),
            ),
            "models" => {
                let id = match p["provider"].as_str() {
                    Some(id) => id.to_string(),
                    None => self.model.read().unwrap().0.clone(),
                };
                json!(crate::llm::remote::models(&id).await?.iter().map(|m| m.to_json()).collect::<Vec<_>>())
            }
            "accounts" => self.accounts()?,
            "login" => self.login(arg("account")?, thread(p)?).await?,
            "logout" => {
                self.logout(arg("account")?).await?;
                Value::Null
            }
            "account_update" => {
                self.account_update(ext, arg("account")?, arg("status")?, p["who"].as_str()).await?;
                Value::Null
            }
            "secret_get" => json!(crate::config::secret(ext, arg("key")?)?),
            "secret_set" => {
                crate::config::set_secret(ext, arg("key")?, p["value"].as_str())?;
                Value::Null
            }
            "login_ask" | "login_choose" | "login_open" | "login_progress" | "login_callback" | "login_wait" => self.login_step(ext, name, p).await?,
            "sessions" => json!(self.db.sessions(thread(p).ok().map(|t| t.key()).as_deref())?),
            // A conversation of no chat, addressed as thread `{messenger: "session", id}`.
            "session_new" if thread(p).is_err() => {
                let id = self.db.new_detached_session()?;
                self.db.update_session(&id, p["name"].as_str(), &p["settings"])?;
                json!(id)
            }
            "session_new" | "session_switch" => {
                let t = thread(p)?;
                let event = if name == "session_new" { "session_before_new" } else { "session_before_switch" };
                if self.ext.listens(event) {
                    let current = self.db.current_session(&t.key())?;
                    let data = json!({"session": current, "to": p["session"], "by": caller});
                    let data = self.ext.emit(event, data, &Origin::thread(t.clone())).await;
                    match &data["block"] {
                        Value::String(why) if !why.is_empty() => bail!("blocked by an extension: {why}"),
                        Value::Bool(true) => bail!("blocked by an extension"),
                        _ => {}
                    }
                }
                let switch = name == "session_switch";
                let id = if switch {
                    let id = arg("session")?;
                    anyhow::ensure!(self.db.session(id)?.is_some(), "no session `{id}`");
                    id.to_string()
                } else {
                    let id = self.db.new_session(&t.key())?;
                    self.db.update_session(&id, p["name"].as_str(), &p["settings"])?;
                    id
                };
                let chat = self.chat(&t).await?;
                let apply = {
                    let (gw, t, id) = (self.clone(), t.clone(), id.clone());
                    async move {
                        let mut agent = chat.agent.lock().await;
                        gw.waits.cancel(&t, "new");
                        if switch { agent.switch(&id) } else { Ok(agent.begin(id)) }
                    }
                };
                // Asked from the thread's own reply, which holds the conversation: the change
                // waits for that turn to end instead of cancelling it.
                if p["from_turn"].as_u64().is_some_and(|turn| self.turns.is_reply_of(turn, &t)) {
                    tokio::spawn(async move {
                        if let Err(e) = apply.await {
                            eprintln!("session change: {e:#}");
                        }
                    });
                } else {
                    self.turns.cancel_thread(&t);
                    apply.await?;
                }
                json!(id)
            }
            "history" => {
                let session = match (p["session"].as_str(), thread(p)) {
                    (Some(s), _) => Some(s.to_string()),
                    (None, Ok(t)) => Some(self.db.current_session(&t.key())?.ok_or_else(|| anyhow!("no conversation in that thread yet"))?),
                    (None, Err(_)) => None,
                };
                let kinds: Vec<String> = serde_json::from_value(p["kinds"].clone()).unwrap_or_default();
                let limit = p["limit"].as_u64().unwrap_or(100) as usize;
                json!(self.db.history(session.as_deref(), &kinds, p["since"].as_i64().unwrap_or(0), limit)?)
            }
            "journal_append" => {
                let mut e = crate::db::Entry::new("custom", json!({"type": arg("type")?, "data": p["data"]}));
                e.session = match (p["session"].as_str(), thread(p)) {
                    (Some(s), _) => Some(s.to_string()),
                    (None, Ok(t)) => self.db.current_session(&t.key())?,
                    (None, Err(_)) => None,
                };
                e.caller = Some(caller);
                json!(self.db.journal(&e)?)
            }
            "session_update" => {
                self.db.update_session(arg("session")?, p["name"].as_str(), &p["settings"])?;
                Value::Null
            }
            "messages" => {
                let chat = self.chat(&thread(p)?).await?;
                let agent = chat.agent.lock().await;
                let (messages, tokens, window) = agent.conversation();
                let messages: Vec<Value> = messages.iter().map(Message::to_json).collect();
                json!({"session": agent.session(), "messages": messages, "tokens": tokens, "window": window})
            }
            "messages_set" => {
                let list = p["messages"].as_array().ok_or_else(|| anyhow!("missing `messages`"))?;
                let messages = list.iter().map(Message::from_json).collect::<Option<Vec<_>>>().ok_or_else(|| anyhow!("malformed `messages`"))?;
                self.chat(&thread(p)?).await?.agent.lock().await.set_history(messages)?;
                Value::Null
            }
            "search" => {
                let limit = p["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
                Value::Array(self.db.search(arg("query")?, limit)?.into_iter().map(|h| json!({"at": h.at, "role": h.role, "text": h.text})).collect())
            }
            "extensions" => self.ext.list(),
            "extension_enable" => {
                let name = arg("name")?;
                // Whoever starts a new extension first installed it: the user, or the agent
                // through an extension's tool. The core says so; the extension can't.
                if self.ext.dir().join(name).is_dir() && crate::config::unit("extensions", name)?["origin"].is_null() {
                    crate::config::set(&format!("extensions.{name}.origin"), json!(if caller == "user" { "user" } else { "agent" }))?;
                }
                let line = self.ext.load(name).await?;
                self.publish_commands().await;
                json!(line)
            }
            "extension_disable" => {
                self.ext.disable(arg("name")?).await?;
                self.publish_commands().await;
                Value::Null
            }
            "extension_logs" => {
                let name = arg("name")?;
                anyhow::ensure!(extensions::valid_name(name), "bad extension name `{name}`");
                json!(extensions::log_tail(name, p["lines"].as_u64().unwrap_or(100) as usize))
            }
            "extension_health" => self.ext.check_health(arg("name")?).await?,
            "extensions_reload" => {
                self.ext.reload_except(Some(ext)).await;
                self.publish_commands().await;
                self.ext.list()
            }
            "settings" => {
                extensions::with_defaults(&self.ext.settings_schema(ext), &crate::config::unit("extensions", ext)?["settings"])
            }
            "settings_set" => {
                let path = format!("extensions.{}.settings.{}", ext, arg("path")?);
                self.set_config(&path, p["value"].clone()).await?;
                Value::Null
            }
            "config_list" => Value::Array(self.config_list(p["prefix"].as_str().unwrap_or(""))?),
            "config_get" => {
                let (path, value) = (arg("path")?, crate::config::get(arg("path")?)?);
                let secret = self.secret_fields(path);
                match crate::config::is_secret_path(path, &secret) && !value.is_null() {
                    true => json!(crate::config::MASK),
                    false => crate::config::masked(&value, &secret),
                }
            }
            "config_set" => {
                let path = arg("path")?;
                let (kind, _, inner) = crate::config::split_path(path)?;
                let field = inner.first().map(String::as_str);
                if caller != "user" && kind == "extensions" && matches!(field, None | Some("enabled" | "origin")) {
                    bail!("only the user turns extensions on and off");
                }
                self.set_config(path, p["value"].clone()).await?;
                Value::Null
            }
            "emit" => {
                let turn = p["from_turn"].as_u64().and_then(|id| self.turns.tag(id));
                let origin = extensions::Origin { thread: thread(p).ok(), turn, depth: p["depth"].as_u64().unwrap_or(0) as u32 };
                self.ext.emit_own(ext, arg("event")?, p["data"].clone(), origin).await?
            }
            "store_get" => match self.db.kv_get(ext, arg("key")?)? {
                Some(v) => serde_json::from_str(&v).unwrap_or(Value::Null),
                None => Value::Null,
            },
            "store_set" => {
                let value = (!p["value"].is_null()).then(|| p["value"].to_string());
                self.db.kv_set(ext, arg("key")?, value.as_deref()).map(|_| Value::Null)?
            }
            "store_list" => {
                let rows = self.db.kv_list(ext, p["prefix"].as_str().unwrap_or(""))?;
                Value::Array(rows.into_iter().map(|(k, v)| json!({"key": k, "value": serde_json::from_str::<Value>(&v).unwrap_or(Value::Null)})).collect())
            }
            other => bail!("unknown operation `{other}`"),
        })
    }

    /// The home thread: the one set with `/home`, else the one the user wrote in last.
    pub(super) fn home(&self) -> Result<Thread> {
        if let Some((m, id)) = crate::config::app()?.home.as_deref().and_then(|h| h.split_once(':')) {
            return Ok(Thread::new(m, id));
        }
        let seen = self.activity.lock().unwrap();
        let last = seen.iter().max_by_key(|(_, at)| **at).map(|(t, _)| t.clone());
        last.ok_or_else(|| anyhow!("there is no home thread yet: send /home in the thread that should be it"))
    }

    /// Secret field names under `path`: those an extension's schema marks.
    fn secret_fields(&self, path: &str) -> Vec<String> {
        match crate::config::split_path(path) {
            Ok((kind, id, _)) if kind == "extensions" => extensions::secret_fields(&self.ext.settings_schema(&id)),
            _ => Vec::new(),
        }
    }

    /// Every setting a schema describes or a file holds: August's own, then each extension's.
    fn config_list(&self, prefix: &str) -> Result<Vec<Value>> {
        let mut units = vec![("august".to_string(), crate::config::app_schema(), crate::config::unit("august", "")?)];
        for e in self.ext.list().as_array().into_iter().flatten() {
            let name = e["name"].as_str().unwrap_or_default();
            units.push((format!("extensions.{name}.settings"), self.ext.settings_schema(name), crate::config::get(&format!("extensions.{name}.settings"))?));
        }
        let mut out = Vec::new();
        for (unit, schema, values) in units.into_iter().filter(|(u, ..)| u.starts_with(prefix) || prefix.starts_with(u.as_str())) {
            let props = schema["properties"].as_object().cloned().unwrap_or_default();
            let mut keys: Vec<String> = props.keys().cloned().collect();
            keys.extend(values.as_object().into_iter().flatten().map(|(k, _)| k.clone()).filter(|k| !props.contains_key(k)));
            let secret = extensions::secret_fields(&schema);
            for key in keys {
                let (path, spec, value) = (format!("{unit}.{key}"), &props.get(&key).cloned().unwrap_or_default(), &values[&key]);
                if !path.starts_with(prefix) {
                    continue;
                }
                let hidden = crate::config::is_secret_path(&path, &secret) && !value.is_null();
                let value = if hidden { json!(crate::config::MASK) } else { crate::config::masked(value, &secret) };
                out.push(json!({"path": path, "description": spec["description"], "type": spec["type"], "default": spec["default"], "value": value, "secret": hidden || spec["secret"] == true}));
            }
        }
        Ok(out)
    }

    /// Changes a setting and tells extensions (`config_changed {path}`).
    pub(super) async fn set_config(&self, path: &str, value: Value) -> Result<()> {
        crate::config::set(path, value)?;
        if self.ext.listens("config_changed") {
            let ext = self.ext.clone();
            let data = json!({"path": path});
            tokio::spawn(async move { ext.emit("config_changed", data, &Origin::default()).await });
        }
        Ok(())
    }

    pub(super) fn messenger(&self, thread: &Thread) -> Result<Arc<dyn Messenger>> {
        self.channel(&thread.messenger).ok_or_else(|| anyhow!("messenger `{}` is not running", thread.messenger))
    }

    /// Every agent tool, with the extension that offers it (`august` for built-in ones).
    fn tool_list(&self) -> Vec<Value> {
        let owners = self.ext.tool_owners();
        let mut out: Vec<Value> = self
            .tools()
            .specs()
            .into_iter()
            .map(|t| {
                let owner = owners.get(&t.name).map_or("august", String::as_str);
                json!({"name": t.name, "description": t.description, "parameters": t.input_schema, "owner": owner})
            })
            .collect();
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out
    }

    async fn messengers(&self) -> Value {
        let seen = self.activity.lock().unwrap().clone();
        let active = seen.iter().max_by_key(|(_, at)| **at).map(|(t, _)| t.clone());
        let mut all = self.all_channels();
        all.sort_by_key(|m| m.id().to_string());
        let mut out = Vec::new();
        for m in all {
            let d = m.describe();
            let mut ids = m.threads().await;
            for t in seen.keys().filter(|t| t.messenger == d.id) {
                if !ids.contains(&t.id) {
                    ids.push(t.id.clone());
                }
            }
            let places = self.places.lock().unwrap().clone();
            let threads: Vec<Value> = ids
                .into_iter()
                .map(|id| {
                    let thread = Thread::new(&d.id, &id);
                    let place = places.get(&thread).cloned().unwrap_or_default();
                    json!({"id": id, "active": active.as_ref() == Some(&thread), "last_seen": seen.get(&thread), "place": place})
                })
                .collect();
            out.push(json!({"id": d.id, "name": d.name, "capabilities": d.capabilities, "notes": d.notes, "actions": d.actions, "threads": threads}));
        }
        Value::Array(out)
    }

    fn listen(self: &Arc<Self>, thread: Thread, accept: waits::Accept, ttl: Duration) -> u64 {
        let (id, rx) = self.waits.add(thread, accept);
        self.listeners.lock().unwrap().insert(id, rx);
        // A listener nobody collects mustn't keep taking the user's messages.
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Some(gw) = me.upgrade()
                && gw.listeners.lock().unwrap().remove(&id).is_some()
            {
                gw.waits.remove(id);
            }
        });
        id
    }

    async fn next(&self, listener: u64, timeout: Duration) -> Result<Value> {
        let rx = self.listeners.lock().unwrap().remove(&listener);
        let rx = rx.ok_or_else(|| anyhow!("no listener #{listener} (it gave its event, or its time ran out)"))?;
        let reply = tokio::time::timeout(timeout, rx).await.ok().and_then(Result::ok);
        self.waits.remove(listener);
        Ok(match reply {
            Some(waits::Reply::Press(button)) => json!({"press": button}),
            Some(waits::Reply::Text(text)) => json!({"text": text}),
            Some(waits::Reply::Cancelled(why)) => json!({"cancelled": why}),
            None => json!({"timeout": true}),
        })
    }

    /// Cancels waits and every turn of the thread; returns how many turns were running.
    pub(super) async fn stop(&self, thread: &Thread) -> usize {
        self.waits.cancel(thread, "stop");
        // Extensions hear it first, so a loop of theirs doesn't start the next turn.
        let turns: Vec<Value> = self.turns.list(Some(thread)).as_array().into_iter().flatten().map(|t| t["id"].clone()).collect();
        self.ext.emit("stop", json!({"turns": turns}), &Origin::thread(thread.clone())).await;
        // Every turn of the thread: the reply, quiet ones, sub-agents.
        self.turns.cancel_thread(thread)
    }

    /// The thread of a call, and the turn it is made from (`from_turn`, which the SDKs send).
    fn origin(&self, p: &Value) -> Result<extensions::Origin> {
        let turn = p["from_turn"].as_u64().and_then(|id| self.turns.tag(id));
        Ok(extensions::Origin { thread: Some(thread(p)?), turn, ..Default::default() })
    }

    async fn call_tool(&self, origin: extensions::Origin, name: &str, input: &Value, caller: &str) -> Result<(String, bool)> {
        let thread = origin.thread.clone().expect("a call's origin has its thread");
        self.messenger(&thread)?;
        let ctx = ToolCtx {
            db: self.db.clone(),
            origin,
            inbox: None,
            caller: caller.into(),
        };
        Ok(self.tools().call(None, name, input, &ctx).await)
    }

    /// Switches to `model` of the active provider, or to `provider:model` (`provider:` for
    /// its default), and persists it.
    async fn switch_model(&self, model: &str) -> Result<(String, String)> {
        let mut model = model.to_string();
        if self.ext.listens("model_select") {
            let previous = self.model.read().unwrap().1.clone();
            let data = self.ext.emit("model_select", json!({"model": model, "previous": previous}), &Origin::default()).await;
            match &data["block"] {
                Value::String(why) if !why.is_empty() => bail!("blocked by an extension: {why}"),
                Value::Bool(true) => bail!("blocked by an extension"),
                _ => {}
            }
            model = data["model"].as_str().map(String::from).unwrap_or(model);
        }
        let ids: Vec<String> = self.ext.providers().into_iter().map(|(_, p)| p.id).collect();
        let mut sel = providers::parse_spec(&model, &ids)?;
        anyhow::ensure!(!sel.provider.is_empty(), "no model provider is chosen yet: sign in to one with /login");
        anyhow::ensure!(ids.contains(&sel.provider), "no provider `{}` (have: {})", sel.provider, ids.join(", "));
        if sel.model.is_empty() {
            sel.model = crate::llm::remote::default_model(&sel.provider).await?;
        }
        let (provider_id, model) = (sel.provider.clone(), sel.model.clone());
        let previous = self.model.read().unwrap().1.clone();
        crate::agent::SessionStore::journal(&*self.db, &crate::db::Entry::new("model_change", json!({"model": model, "previous": previous})));
        *self.provider.write().unwrap() = providers::build(sel);
        *self.model.write().unwrap() = (provider_id.clone(), model.clone());
        let mut cfg = crate::config::app()?;
        cfg.provider = Some(provider_id.clone());
        cfg.model = Some(model.clone());
        crate::config::save_app(&cfg)?;
        Ok((provider_id, model))
    }
}
