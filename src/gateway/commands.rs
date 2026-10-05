//! Slash commands (`/new`, `/stop`, `/model`, ...).

use super::Gateway;
use crate::channels::{Channel, ChatId, CommandSpec};
use crate::llm::providers;
use anyhow::Result;
use std::sync::Arc;

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "new", description: "Start a fresh conversation" },
    CommandSpec { name: "stop", description: "Cancel the current task" },
    CommandSpec { name: "compact", description: "Summarise older messages to free up context" },
    CommandSpec { name: "memory", description: "Show what I remember about you" },
    CommandSpec { name: "tasks", description: "List scheduled tasks" },
    CommandSpec { name: "model", description: "Show or change the model" },
    CommandSpec { name: "status", description: "Show provider, model and workspace" },
    CommandSpec { name: "help", description: "List commands" },
];

impl Gateway {
    pub(super) async fn command(&self, channel: &Arc<dyn Channel>, id: &ChatId, name: &str, args: &str) -> Result<()> {
        let chat = id.chat.as_str();
        let state = self.chat(id).await?;
        let reply = match name {
            "start" | "help" => {
                let mut s = String::from("**August** — your personal agent. Just write a message.\n\n");
                for c in COMMANDS {
                    s += &format!("/{} — {}\n", c.name, c.description);
                }
                s
            }
            "stop" => {
                match state.cancel.lock().unwrap().as_ref() {
                    Some(n) => {
                        n.notify_one();
                        "Stopping…".into()
                    }
                    None => "Nothing is running.".to_string(),
                }
            }
            "new" | "reset" => {
                if let Some(n) = state.cancel.lock().unwrap().as_ref() {
                    n.notify_one();
                }
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
            "tasks" => crate::tools::format_tasks(&self.db.tasks(Some((&id.channel, &id.chat)))?),
            "status" => format!(
                "Model: `{}`\nWorkspace: `{}`\nBusy: {}",
                self.provider_label.read().unwrap(),
                self.workspace.display(),
                if state.cancel.lock().unwrap().is_some() { "yes" } else { "no" }
            ),
            "model" if args.is_empty() => {
                format!("Current model: `{}`\nChange with `/model <id>`.", self.provider_label.read().unwrap())
            }
            "model" => match self.switch_model(args).await {
                Ok(label) => format!("Now using `{label}` (applies to the next message)."),
                Err(e) => format!("Could not switch model: {e:#}"),
            },
            other => format!("Unknown command /{other}. Try /help."),
        };
        channel.send(chat, &reply, &[]).await?;
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
