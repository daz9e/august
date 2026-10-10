//! Slash commands: the core runs whatever extensions register (the everyday ones come with
//! the default `commands` extension) and tells messengers the list.

use super::Gateway;
use crate::extensions::Origin;
use crate::messengers::{CommandSpec, Messenger, OutMessage, Thread};
use anyhow::Result;
use std::sync::Arc;

impl Gateway {
    /// Every command extensions registered, with its owner.
    pub(super) fn command_list(&self) -> Vec<(CommandSpec, String)> {
        let list = self.ext.commands().into_iter().map(|(owner, name, description)| {
            let description = if description.is_empty() { format!("/{name}") } else { description };
            (CommandSpec { name, description }, owner)
        });
        list.collect()
    }

    /// Tells every messenger the current command list (after extensions changed).
    pub(super) async fn publish_commands(&self) {
        let list: Vec<CommandSpec> = self.command_list().into_iter().map(|(c, _)| c).collect();
        for ch in self.all_channels() {
            if !ch.describe().capabilities.commands {
                continue;
            }
            if let Err(e) = ch.set_commands(&list).await {
                eprintln!("{}: could not register commands: {e:#}", ch.id());
            }
        }
    }

    pub(super) async fn command(&self, channel: &Arc<dyn Messenger>, id: &Thread, name: &str, args: &str) -> Result<()> {
        let chat = id.id.as_str();
        let reply = match self.ext.run_command(name, args, &Origin::thread(id.clone())).await {
            Some(Ok(Some(reply))) => Some(reply),
            Some(Ok(None)) => None,
            Some(Err(e)) => Some(format!("⚠️ /{name} failed: {e}")),
            // The emergency brake works even with no extension running to own it.
            None if name == "stop" => Some(format!("Stopped {} turn(s).", self.stop(id).await)),
            // So is the list of what can be typed.
            None if name == "help" => {
                let mut s = String::from("/help — List commands\n/stop — Cancel the current task\n");
                for (c, _) in self.command_list().into_iter().filter(|(c, _)| c.name != "stop") {
                    s += &format!("/{} — {}\n", c.name, c.description);
                }
                Some(s)
            }
            None => Some(format!("Unknown command /{name}. Try /help.")),
        };
        if let Some(reply) = reply {
            channel.send(chat, &OutMessage::text(reply)).await?;
        }
        channel.presence(chat, false).await;
        Ok(())
    }
}
