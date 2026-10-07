//! Questions as chat buttons: approvals (Allow / Deny) and `ctx.ask` with any options.

use crate::messengers::{Button, Messenger};
use crate::tools::Approver;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::oneshot;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// Unanswered questions: key -> where the tapped button's value goes.
pub(super) type Pending = Arc<StdMutex<HashMap<String, oneshot::Sender<String>>>>;

pub(super) struct ChatApprover {
    pub(super) channel: Arc<dyn Messenger>,
    pub(super) chat: String,
    pub(super) pending: Pending,
}

impl ChatApprover {
    /// Sends `text` with a button per `(label, value)` and waits for a tap; the message is
    /// then edited to `done(label of the answer, or None on timeout)`. Returns the value.
    pub(super) async fn ask(&self, text: &str, options: &[(&str, &str)], done: impl Fn(Option<&str>) -> String) -> Option<String> {
        let key: String = crate::util::new_uuid().chars().take(8).collect();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), tx);
        let buttons: Vec<Button> =
            options.iter().map(|(label, value)| Button { label: label.to_string(), data: format!("ap:{key}:{value}") }).collect();
        let Ok(msg) = self.channel.send(&self.chat, text, &buttons).await else {
            self.pending.lock().unwrap().remove(&key);
            return None;
        };
        let answer = tokio::time::timeout(ANSWER_TIMEOUT, rx).await.ok().and_then(|r| r.ok());
        self.pending.lock().unwrap().remove(&key);
        let label = answer.as_ref().and_then(|a| options.iter().find(|(_, v)| v == a)).map(|(l, _)| *l);
        self.channel.edit(&self.chat, &msg, &done(label), &[]).await.ok();
        answer
    }
}

#[async_trait]
impl Approver for ChatApprover {
    async fn approve(&self, action: &str) -> bool {
        let action = action.replace("```", "'''");
        let text = format!("⚠️ **Approval needed**\n```\n{action}\n```");
        let options = [("✅ Allow", "y"), ("❌ Deny", "n")];
        let done = |label: Option<&str>| {
            let verdict = match label {
                Some("✅ Allow") => "✅ Allowed",
                Some(_) => "❌ Denied",
                None => "⌛ Timed out, denied",
            };
            format!("{verdict}\n```\n{action}\n```")
        };
        self.ask(&text, &options, done).await.as_deref() == Some("y")
    }
}
