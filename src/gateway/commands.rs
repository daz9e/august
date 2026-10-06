//! Slash commands (`/new`, `/stop`, `/model`, ...).

use super::Gateway;
use crate::channels::{Channel, ChatId, CommandSpec};
use crate::llm::providers;
use anyhow::Result;
use std::sync::Arc;

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("new", "Start a fresh conversation"),
    CommandSpec::new("stop", "Cancel the current task"),
    CommandSpec::new("goal", "Keep working until a goal is reached (/goal clear to stop)"),
    CommandSpec::new("queue", "Run a message as its own turn after the current one"),
    CommandSpec::new("compact", "Summarise older messages to free up context"),
    CommandSpec::new("usage", "Show token usage of this conversation and today"),
    CommandSpec::new("memory", "Show what I remember about you"),
    CommandSpec::new("tasks", "List scheduled tasks"),
    CommandSpec::new("model", "Show or change the model"),
    CommandSpec::new("status", "Show provider, model and workspace"),
    CommandSpec::new("extensions", "List extensions and their status"),
    CommandSpec::new("reload", "Restart all extensions"),
    CommandSpec::new("mcp", "List MCP servers and their tools"),
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
            if let Err(e) = ch.set_commands(&list).await {
                eprintln!("{}: could not register commands: {e:#}", ch.id());
            }
        }
    }

    pub(super) async fn command(self: &Arc<Self>, channel: &Arc<dyn Channel>, id: &ChatId, name: &str, args: &str) -> Result<()> {
        let chat = id.chat.as_str();
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
                match state.cancel.lock().unwrap().as_ref() {
                    Some(n) => {
                        n.notify_one();
                        "Stopping…".into()
                    }
                    None => "Nothing is running.".to_string(),
                }
            }
            "goal" if args.is_empty() => match state.goal.lock().unwrap().as_ref() {
                Some(g) => format!("🎯 Goal: {} ({} turns so far). /goal clear to drop it.", g.text, g.turns),
                None => "No goal. Set one with /goal <what should be achieved>.".into(),
            },
            "goal" if args == "clear" => {
                let had = state.goal.lock().unwrap().take().is_some();
                if had { "Goal dropped.".into() } else { "No goal to drop.".into() }
            }
            "goal" => {
                *state.goal.lock().unwrap() = Some(super::Goal { text: args.to_string(), turns: 0 });
                let (gw, ch, id, chat) = (self.clone(), channel.clone(), id.clone(), chat.to_string());
                let text = format!("[New goal] {args}\nWork on it until it is achieved; I'll check after each turn.");
                tokio::spawn(async move {
                    if let Err(e) = gw.turn(ch, id, &chat, &text, Vec::new(), false).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                format!("🎯 Goal set: {args}")
            }
            "queue" if args.is_empty() => "Usage: /queue <message>".into(),
            "queue" => {
                let (gw, ch, id, chat, text) = (self.clone(), channel.clone(), id.clone(), chat.to_string(), args.to_string());
                tokio::spawn(async move {
                    if let Err(e) = gw.turn(ch, id, &chat, &text, Vec::new(), false).await {
                        eprintln!("gateway: {e:#}");
                    }
                });
                "📋 Queued.".into()
            }
            "new" | "reset" => {
                if let Some(n) = state.cancel.lock().unwrap().as_ref() {
                    n.notify_one();
                }
                state.goal.lock().unwrap().take();
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
            "usage" => self.db.usage_report(&format!("{}:{}", id.channel, id.chat))?,
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
            "extensions" => self.ext.status(),
            "mcp" => self.mcp.status(),
            "reload" => {
                let status = self.ext.reload().await;
                self.publish_commands().await;
                format!("Extensions reloaded.\n{status}")
            }
            other => {
                let origin = Some((id.channel.clone(), id.chat.clone()));
                match self.ext.run_command(other, args, &origin).await {
                    Some(Ok(Some(reply))) => reply,
                    Some(Ok(None)) => return Ok(()),
                    Some(Err(e)) => format!("⚠️ /{other} failed: {e}"),
                    None => format!("Unknown command /{other}. Try /help."),
                }
            }
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
