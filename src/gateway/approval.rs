//! Questions in a thread, built on sending a message and waiting for what comes back: a
//! button press, a number, an option's name or any text. Approvals are questions with
//! Allow / Deny.

use super::waits::{Accept, Reply, Waits};
use crate::agent::Inbox;
use crate::messengers::{Button, Messenger, OutMessage, Thread};
use crate::tools::Approver;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// How long an approval waits for the user.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Answer {
    /// The option with this index: its button, its number or its name.
    Option(usize),
    /// Anything else the user wrote.
    Text(String),
}

/// Sends `text` to `thread` with `options` (as buttons where the messenger has enough,
/// else as a numbered list) and waits up to `timeout`. Returns the sent message's id and
/// the answer, `None` if there was none in time.
pub(super) async fn ask(
    messenger: &dyn Messenger,
    waits: &Waits,
    thread: &Thread,
    text: &str,
    options: &[String],
    timeout: Duration,
) -> Result<(String, Option<Answer>)> {
    let key: String = crate::util::new_uuid().chars().take(8).collect();
    let ids: Vec<String> = (0..options.len()).map(|i| format!("{key}.{i}")).collect();
    let fits = (1..=messenger.describe().capabilities.buttons).contains(&options.len());
    let message = if fits {
        let buttons = ids.iter().zip(options).map(|(id, label)| Button { id: id.clone(), label: label.clone() }).collect();
        OutMessage { text: text.into(), buttons }
    } else {
        let list: Vec<String> = options.iter().enumerate().map(|(i, o)| format!("{}. {o}", i + 1)).collect();
        OutMessage::text(format!("{text}\n{}", list.join("\n")))
    };
    let (wait, rx) = waits.add(thread.clone(), Accept { buttons: ids.clone(), text: true });
    let sent = messenger.send(&thread.id, &message).await;
    let reply = match &sent {
        Ok(_) => tokio::time::timeout(timeout, rx).await.ok().and_then(Result::ok),
        Err(_) => None,
    };
    waits.remove(wait);
    let answer = reply.map(|r| match r {
        Reply::Press(button) => Answer::Option(ids.iter().position(|id| *id == button).unwrap_or(0)),
        Reply::Text(t) => {
            let by_number = t.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < options.len());
            let by_name = options.iter().position(|o| o.trim().eq_ignore_ascii_case(t.trim()));
            by_number.or(by_name).map(Answer::Option).unwrap_or(Answer::Text(t))
        }
    });
    Ok((sent?, answer))
}

/// Approvals in a thread. A text answer other than a yes denies, and while a turn runs
/// it also goes to the agent, so "no, do it this way" isn't lost.
pub(super) struct ChatApprover {
    pub(super) messenger: Arc<dyn Messenger>,
    pub(super) thread: Thread,
    pub(super) waits: Arc<Waits>,
    pub(super) inbox: Option<Arc<Inbox>>,
}

fn is_yes(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "y" | "yes" | "ok" | "allow" | "да" | "ок")
}

#[async_trait]
impl Approver for ChatApprover {
    async fn approve(&self, action: &str) -> bool {
        let action = action.replace("```", "'''");
        let text = format!("⚠️ **Approval needed**\n```\n{action}\n```");
        let options = ["✅ Allow".to_string(), "❌ Deny".to_string()];
        let Ok((message, answer)) = ask(&*self.messenger, &self.waits, &self.thread, &text, &options, APPROVAL_TIMEOUT).await else {
            return false;
        };
        let (verdict, allowed) = match answer {
            Some(Answer::Option(0)) => ("✅ Allowed", true),
            Some(Answer::Option(_)) => ("❌ Denied", false),
            Some(Answer::Text(t)) if is_yes(&t) => ("✅ Allowed", true),
            Some(Answer::Text(t)) => {
                if let Some(inbox) = &self.inbox {
                    inbox.offer(&t);
                }
                ("❌ Denied", false)
            }
            None => ("⌛ Timed out, denied", false),
        };
        if self.messenger.describe().capabilities.edit {
            let done = OutMessage::text(format!("{verdict}\n```\n{action}\n```"));
            self.messenger.edit(&self.thread.id, &message, &done).await.ok();
        }
        allowed
    }
}
