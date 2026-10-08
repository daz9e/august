//! Slash commands (`/new`, `/stop`, `/model`, ...): the built-in ones wrap operations of
//! the core's table (`ops.rs`) and only word their replies.

use super::Gateway;
use super::ops::Caller;
use crate::extensions::{self, Origin};
use crate::messengers::{Messenger, Thread, CommandSpec, OutMessage};
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("new", "Start a fresh conversation"),
    CommandSpec::new("stop", "Cancel the current task"),
    CommandSpec::new("queue", "Run a message as its own turn after the current one"),
    CommandSpec::new("compact", "Summarise older messages to free up context"),
    CommandSpec::new("usage", "Show token usage of this conversation and today"),
    CommandSpec::new("memory", "Show what I remember about you"),
    CommandSpec::new("model", "Show or change the model"),
    CommandSpec::new("status", "Show provider, model and workspace"),
    CommandSpec::new("extensions", "List extensions; enable or disable one"),
    CommandSpec::new("reload", "Restart all extensions"),
    CommandSpec::new("help", "List commands"),
];

impl Gateway {
    /// Built-in commands followed by the ones extensions registered, with their owner.
    pub(super) fn command_list(&self) -> Vec<(CommandSpec, String)> {
        let reserved: Vec<&str> = COMMANDS.iter().map(|c| c.name.as_ref()).chain(["start", "reset"]).collect();
        let ext = self.ext.commands(&reserved).into_iter().map(|(owner, name, description)| {
            let description = if description.is_empty() { format!("/{name}").into() } else { description.into() };
            (CommandSpec { name: name.into(), description }, owner)
        });
        COMMANDS.iter().map(|c| (c.clone(), "august".to_string())).chain(ext).collect()
    }

    /// Tells every messenger the current command list (after extensions changed).
    pub(super) async fn publish_commands(&self) {
        let list: Vec<CommandSpec> = self.command_list().into_iter().map(|(c, _)| c).collect();
        for ch in self.channels.values() {
            if !ch.describe().capabilities.commands {
                continue;
            }
            if let Err(e) = ch.set_commands(&list).await {
                eprintln!("{}: could not register commands: {e:#}", ch.id());
            }
        }
    }

    pub(super) async fn command(self: &Arc<Self>, channel: &Arc<dyn Messenger>, id: &Thread, name: &str, args: &str) -> Result<()> {
        let chat = id.id.as_str();
        let reply = match name {
            "queue" if !args.is_empty() => {
                let (gw, ch, id, chat, text) = (self.clone(), channel.clone(), id.clone(), chat.to_string(), args.to_string());
                tokio::spawn(async move {
                    if let Err(e) = gw.turn(ch, id, &chat, &text, Vec::new()).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                "📋 Queued.".into()
            }
            _ if is_builtin(name) => self.builtin(id, name, args).await.unwrap_or_else(|e| format!("⚠️ /{name} failed: {e:#}")),
            other => match self.ext.run_command(other, args, &Origin::thread(id.clone())).await {
                Some(Ok(Some(reply))) => reply,
                Some(Ok(None)) => {
                    channel.presence(chat, false).await;
                    return Ok(());
                }
                Some(Err(e)) => format!("⚠️ /{other} failed: {e}"),
                None => format!("Unknown command /{other}. Try /help."),
            },
        };
        channel.send(chat, &OutMessage::text(reply)).await?;
        channel.presence(chat, false).await;
        Ok(())
    }

    /// A built-in command: the operation it wraps, and its reply.
    async fn builtin(self: &Arc<Self>, id: &Thread, name: &str, args: &str) -> Result<String> {
        let thread = json!({"thread": {"messenger": id.messenger, "id": id.id}});
        let op = |name: &'static str, params: Value| async move { self.op(Caller::User, name, &params).await };
        Ok(match name {
            "start" | "help" => {
                let mut s = String::from("**August** — your personal agent. Just write a message.\n\n");
                for (c, _) in self.command_list() {
                    s += &format!("/{} — {}\n", c.name, c.description);
                }
                s
            }
            "stop" => match op("stop", thread).await?["cancelled"].as_u64() {
                Some(0) | None => "Nothing is running.".into(),
                Some(_) => "Stopping…".into(),
            },
            "queue" => "Usage: /queue <message>".into(),
            "new" | "reset" => {
                op("session_new", thread).await?;
                "Started a new conversation.".into()
            }
            "compact" => match op("compact", thread).await {
                Ok(v) if v.is_null() => "Nothing to compact yet.".into(),
                Ok(v) => format!("Compacted: ~{} → ~{} tokens.", v["before"], v["after"]),
                Err(e) => format!("Could not compact: {e:#}"),
            },
            "memory" => {
                let facts = op("memory", json!({})).await?;
                let lines: Vec<String> = facts.as_array().into_iter().flatten().map(|f| format!("#{} {}", f["id"], f["text"].as_str().unwrap_or(""))).collect();
                if lines.is_empty() { "I haven't saved any facts yet.".into() } else { lines.join("\n") }
            }
            "usage" => {
                let u = op("usage", thread).await?;
                let line = |u: &Value| {
                    format!(
                        "{} calls · in {} · cache read {} · cache write {} · out {}",
                        u["calls"], u["input"], u["cache_read"], u["cache_write"], u["output"]
                    )
                };
                format!("This session: {}\nToday, all chats: {}", line(&u["session"]), line(&u["today"]))
            }
            "status" => {
                let s = op("status", thread).await?;
                format!(
                    "Model: `{} · {}`\nWorkspace: `{}`\nBusy: {}",
                    s["provider"].as_str().unwrap_or(""),
                    s["model"].as_str().unwrap_or(""),
                    s["workspace"].as_str().unwrap_or(""),
                    if s["busy"] == true { "yes" } else { "no" }
                )
            }
            "model" if args.is_empty() => {
                let s = op("status", json!({})).await?;
                format!("Current model: `{} · {}`\nChange with `/model <id>`.", s["provider"].as_str().unwrap_or(""), s["model"].as_str().unwrap_or(""))
            }
            "model" => match op("model_set", json!({"model": args})).await {
                Ok(m) => format!("Now using `{} · {}` (applies to the next message).", m["provider"].as_str().unwrap_or(""), m["model"].as_str().unwrap_or("")),
                Err(e) => format!("Could not switch model: {e:#}"),
            },
            "extensions" => {
                let changed = match args.split_whitespace().collect::<Vec<_>>()[..] {
                    [] => Ok(()),
                    ["enable", name] => op("extension_enable", json!({"name": name})).await.map(drop),
                    ["disable", name] => op("extension_disable", json!({"name": name})).await.map(drop),
                    _ => return Ok("Usage: /extensions [enable|disable <name>]".into()),
                };
                let status = extensions::status(&op("extensions", json!({})).await?);
                match changed {
                    Ok(()) => status,
                    Err(e) => format!("{e:#}\n\n{status}"),
                }
            }
            "reload" => format!("Extensions reloaded.\n{}", extensions::status(&op("extensions_reload", json!({})).await?)),
            _ => unreachable!("not a built-in command: {name}"),
        })
    }
}

fn is_builtin(name: &str) -> bool {
    COMMANDS.iter().any(|c| c.name == name) || matches!(name, "start" | "reset")
}
