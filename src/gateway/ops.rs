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
    op("tools", None, "Every agent tool: {name, description, parameters, owner}"),
    op("commands", None, "Every slash command: {name, description, owner}"),
    op("status", None, "{provider, model, workspace, busy} (busy: of `thread`, if given)"),
    op("messengers", MESSAGING, "Every messenger with its capabilities and threads"),
    op("send", MESSAGING, "Send {thread, message}; returns its id"),
    op("edit", MESSAGING, "Replace a sent message {thread, id, message}"),
    op("delete", MESSAGING, "Delete a sent message {thread, id}"),
    op("react", MESSAGING, "React to a message {thread, id, emoji}"),
    op("listen", MESSAGING, "Listen in {thread} for {buttons, text}; returns a listener id"),
    op("next", MESSAGING, "What {listener} took"),
    op("prompt", MESSAGING, "Hand {thread} a message as if the user sent it"),
    op("turn_start", TURNS, "Start a turn {thread, turn: {text, mode: visible|quiet|fork|fresh, source, parent, ...}}; returns its id"),
    op("turn_wait", TURNS, "A turn's outcome {id, timeout_ms}"),
    op("turn_cancel", TURNS, "Cancel turn {id}"),
    op("turns", TURNS, "Running turns, of {thread} or all"),
    op("stop", TURNS, "Stop everything running in {thread}, like /stop"),
    op("callTool", Some("tools"), "Run an agent tool {thread, name, input}"),
    op("llm", Some("llm"), "One completion without tools: {prompt} or {messages}, {system}; in {thread}'s conversation's model, counted in its usage"),
    op("model_set", Some("models"), "Switch to {model} of the current provider"),
    op("sessions", SESSIONS, "Stored conversations, newest first, of {thread} or all: {id, chat, name, settings, messages, bound}"),
    op("session_new", SESSIONS, "Start a new conversation in {thread}, optionally with {name, settings}; returns its id"),
    op("session_update", SESSIONS, "Rename {session} ({name}) or change its {settings}: {model, system, tools}, null deletes"),
    op("session_switch", SESSIONS, "Continue stored {session} in {thread}"),
    op("history", SESSIONS, "The journal of {session} (or {thread}'s, or all): entries after {since} of {kinds}, at most {limit} (default 100)"),
    op("journal_append", None, "Record {type, data} in the journal of {session} or {thread}'s (kind `custom`, caller you)"),
    op("messages", SESSIONS, "The live conversation of {thread} (what the model sees next): {session, messages, tokens, window}"),
    op("messages_set", SESSIONS, "Replace the live conversation of {thread} with {messages} (earlier ones stay searchable)"),
    op("search", SESSIONS, "Full-text search over everything said in any conversation: {query, limit} → [{at, role, text}]"),
    op("usage", SESSIONS, "Token usage of {thread}'s conversation and of today"),
    op("extensions", Some("admin"), "Every extension with its state and what it registers"),
    op("extension_enable", Some("admin"), "(Re)start {name} and keep it enabled; returns its status line, fails with its error"),
    op("extension_disable", Some("admin"), "Stop {name} and keep it disabled"),
    op("extensions_reload", Some("admin"), "Restart every extension but the caller"),
    op("settings", None, "The calling extension's settings, schema defaults filled in"),
    op("settings_set", None, "Set {path, value} in the calling extension's settings (null deletes)"),
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
            "tools" => json!(self.tool_list()),
            "commands" => Value::Array(self.command_list().into_iter().map(|(c, owner)| json!({"name": c.name, "description": c.description, "owner": owner})).collect()),
            "status" => {
                let busy = thread(p).ok().map(|t| self.turns.busy(&t));
                let m = self.model.read().unwrap().clone();
                json!({"provider": m.0, "model": m.1, "workspace": self.workspace, "busy": busy})
            }
            "messengers" => self.messengers().await,
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
            "listen" => {
                let buttons: Vec<String> = serde_json::from_value(p["buttons"].clone()).unwrap_or_default();
                json!(self.listen(thread(p)?, buttons, p["text"] == true, ms("ttl_ms", 600_000)))
            }
            "next" => self.next(p["listener"].as_u64().ok_or_else(|| anyhow!("missing `listener`"))?, ms("timeout_ms", 300_000)).await?,
            "prompt" => {
                let t = thread(p)?;
                let m = self.messenger(&t)?;
                let (gw, text) = (self.clone(), arg("text")?.to_string());
                let source = p["source"].as_str().map(String::from).unwrap_or_else(|| caller.clone());
                let deliver = p["deliver"].as_str().unwrap_or("steer").to_string();
                anyhow::ensure!(["steer", "followUp", "nextTurn"].contains(&deliver.as_str()), "`deliver` is steer, followUp or nextTurn");
                // Not awaited: the caller may be inside a turn of that very thread.
                tokio::spawn(async move {
                    if let Err(e) = gw.deliver(m, t, &text, &source, &deliver).await {
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
                // In a thread: its conversation's model, and its usage counts the call.
                let session = match thread(p) {
                    Ok(t) => self.db.current_session(&t.key())?,
                    Err(_) => None,
                };
                let model = match &session {
                    Some(s) => self.db.session_settings(s)?["model"].as_str().map(String::from),
                    None => None,
                };
                let provider = match model {
                    Some(m) => providers::build_spec(&m).await?,
                    None => self.provider.read().unwrap().clone(),
                };
                let c = provider.complete(&crate::util::new_uuid(), system, &messages, &[]).await?;
                if let Some(s) = &session {
                    self.db.record_usage(s, &c.usage)?;
                }
                json!(c.message.text())
            }
            "model_set" => {
                let (provider, model) = self.switch_model(arg("model")?).await?;
                json!({"provider": provider, "model": model})
            }
            "sessions" => json!(self.db.sessions(thread(p).ok().map(|t| t.key()).as_deref())?),
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
                self.waits.cancel(&t, "new");
                self.turns.cancel_thread(&t);
                let chat = self.chat(&t).await?;
                let mut agent = chat.agent.lock().await;
                if name == "session_switch" {
                    let id = arg("session")?;
                    anyhow::ensure!(self.db.session(id)?.is_some(), "no session `{id}`");
                    agent.switch(id)?;
                } else {
                    agent.reset()?;
                    self.db.update_session(agent.session(), p["name"].as_str(), &p["settings"])?;
                }
                json!(agent.session())
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
            "usage" => self.db.usage(&thread(p)?.key())?,
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
    fn home(&self) -> Result<Thread> {
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
        self.channels.get(&thread.messenger).cloned().ok_or_else(|| anyhow!("messenger `{}` is not running", thread.messenger))
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
        let mut all: Vec<&Arc<dyn Messenger>> = self.channels.values().collect();
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
            let threads: Vec<Value> = ids
                .into_iter()
                .map(|id| {
                    let thread = Thread::new(&d.id, &id);
                    json!({"id": id, "active": active.as_ref() == Some(&thread), "last_seen": seen.get(&thread)})
                })
                .collect();
            out.push(json!({"id": d.id, "name": d.name, "capabilities": d.capabilities, "extra": d.extra, "threads": threads}));
        }
        Value::Array(out)
    }

    fn listen(self: &Arc<Self>, thread: Thread, buttons: Vec<String>, text: bool, ttl: Duration) -> u64 {
        let (id, rx) = self.waits.add(thread, waits::Accept { buttons, text });
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
            workspace: self.workspace.clone(),
            db: self.db.clone(),
            origin,
            inbox: None,
            caller: caller.into(),
        };
        Ok(self.tools().call(None, name, input, &ctx).await)
    }

    /// Rebuilds the provider with another model of the active provider and persists it.
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
        let model = model.as_str();
        let previous = self.model.read().unwrap().1.clone();
        crate::agent::SessionStore::journal(&*self.db, &crate::db::Entry::new("model_change", json!({"model": model, "previous": previous})));
        let mut sel = providers::selection()?;
        sel.model = Some(model.to_string());
        let provider_id = sel.provider.id.to_string();
        let provider = providers::build(sel).await?;
        *self.provider.write().unwrap() = provider;
        *self.model.write().unwrap() = (provider_id.clone(), model.to_string());
        let mut cfg = crate::config::app()?;
        cfg.provider = Some(provider_id.clone());
        cfg.model = Some(model.to_string());
        crate::config::save_app(&cfg)?;
        Ok((provider_id, model.to_string()))
    }
}
