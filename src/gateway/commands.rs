//! Slash commands (`/new`, `/stop`, `/model`, ...).

use super::Gateway;
use crate::extensions::Origin;
use crate::messengers::{Messenger, Thread, CommandSpec, OutMessage};
use crate::llm::providers;
use anyhow::Result;
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
    /// Built-in commands followed by the ones extensions registered.
    pub(super) fn command_list(&self) -> Vec<CommandSpec> {
        let reserved: Vec<&str> = COMMANDS.iter().map(|c| c.name.as_ref()).chain(["start", "reset"]).collect();
        let ext = self.ext.commands(&reserved).into_iter().map(|(name, description)| CommandSpec {
            description: if description.is_empty() { format!("/{name}").into() } else { description.into() },
            name: name.into(),
        });
        COMMANDS.iter().cloned().chain(ext).collect()
    }

    /// Tells every messenger the current command list (after extensions changed).
    pub(super) async fn publish_commands(&self) {
        let list = self.command_list();
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
        let state = self.chat(id).await?;
        let reply = match name {
            "start" | "help" => {
                let mut s = String::from("**August** — your personal agent. Just write a message.\n\n");
                for c in self.command_list() {
                    s += &format!("/{} — {}\n", c.name, c.description);
                }
                s
            }
            "stop" => {
                self.waits.cancel(id, "stop");
                // Extensions hear it first, so a loop of theirs doesn't start the next turn.
                self.ext.emit("stop", serde_json::json!({}), &Origin::thread(id.clone())).await;
                // Every turn of the thread: the reply, quiet ones, sub-agents.
                match self.turns.cancel_thread(id) {
                    0 => "Nothing is running.".to_string(),
                    _ => "Stopping…".to_string(),
                }
            }
            "queue" if args.is_empty() => "Usage: /queue <message>".into(),
            "queue" => {
                let (gw, ch, id, chat, text) = (self.clone(), channel.clone(), id.clone(), chat.to_string(), args.to_string());
                tokio::spawn(async move {
                    if let Err(e) = gw.turn(ch, id, &chat, &text, Vec::new()).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                "📋 Queued.".into()
            }
            "new" | "reset" => {
                self.waits.cancel(id, "new");
                self.turns.cancel_thread(id);
                state.agent.lock().await.reset()?;
                "Started a new conversation.".into()
            }
            "compact" => {
                let mut agent = state.agent.lock().await;
                match agent.compact(true).await {
                    Ok(Some((before, after))) => format!("Compacted: ~{before} → ~{after} tokens."),
                    Ok(None) => "Nothing to compact yet.".into(),
                    Err(e) => format!("Could not compact: {e:#}"),
                }
            }
            "memory" => {
                let facts = self.db.facts()?;
                if facts.is_empty() {
                    "I haven't saved any facts yet.".into()
                } else {
                    facts.iter().map(|f| format!("#{} {}", f.id, f.text)).collect::<Vec<_>>().join("\n")
                }
            }
            "usage" => self.db.usage_report(&format!("{}:{}", id.messenger, id.id))?,
            "status" => format!(
                "Model: `{}`\nWorkspace: `{}`\nBusy: {}",
                self.provider_label.read().unwrap(),
                self.workspace.display(),
                if self.turns.busy(id) { "yes" } else { "no" }
            ),
            "model" if args.is_empty() => {
                format!("Current model: `{}`\nChange with `/model <id>`.", self.provider_label.read().unwrap())
            }
            "model" => match self.switch_model(args).await {
                Ok(label) => format!("Now using `{label}` (applies to the next message)."),
                Err(e) => format!("Could not switch model: {e:#}"),
            },
            "extensions" => {
                let reply = self.ext.command(args).await;
                if !args.is_empty() {
                    self.publish_commands().await;
                }
                reply
            }
            "reload" => {
                let status = self.ext.reload().await;
                self.publish_commands().await;
                format!("Extensions reloaded.\n{status}")
            }
            other => {
                match self.ext.run_command(other, args, &Origin::thread(id.clone())).await {
                    Some(Ok(Some(reply))) => reply,
                    Some(Ok(None)) => {
                        channel.presence(chat, false).await;
                        return Ok(());
                    }
                    Some(Err(e)) => format!("⚠️ /{other} failed: {e}"),
                    None => format!("Unknown command /{other}. Try /help."),
                }
            }
        };
        channel.send(chat, &OutMessage::text(reply)).await?;
        channel.presence(chat, false).await;
        Ok(())
    }

    /// Rebuilds the provider with another model of the active provider and persists it.
    async fn switch_model(&self, model: &str) -> Result<String> {
        let mut sel = providers::selection()?;
        sel.model = Some(model.to_string());
        let label = format!("{} · {model}", sel.provider.id);
        let provider = providers::build(sel).await?;
        *self.provider.write().unwrap() = provider;
        *self.provider_label.write().unwrap() = label.clone();
        let mut cfg: crate::config::Config = crate::config::load(crate::config::CONFIG)?;
        cfg.model = Some(model.to_string());
        crate::config::save(crate::config::CONFIG, &cfg)?;
        Ok(label)
    }
}
