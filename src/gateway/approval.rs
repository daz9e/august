//! Approvals as chat buttons: the agent asks, the user taps Allow or Deny.

use crate::channels::{Button, Channel};
use crate::tools::Approver;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::oneshot;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) type Pending = Arc<StdMutex<HashMap<String, oneshot::Sender<bool>>>>;

pub(super) struct ChatApprover {
    pub(super) channel: Arc<dyn Channel>,
    pub(super) chat: String,
    pub(super) pending: Pending,
}

#[async_trait]
impl Approver for ChatApprover {
    async fn approve(&self, action: &str) -> bool {
        let key: String = crate::util::new_uuid().chars().take(8).collect();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), tx);
        let text = format!("⚠️ **Approval needed**\n```\n{}\n```", action.replace("```", "'''"));
        let buttons = [
            Button { label: "✅ Allow".into(), data: format!("ap:{key}:y") },
            Button { label: "❌ Deny".into(), data: format!("ap:{key}:n") },
        ];
        let Ok(msg) = self.channel.send(&self.chat, &text, &buttons).await else {
            self.pending.lock().unwrap().remove(&key);
            return false;
        };
        let answer = tokio::time::timeout(APPROVAL_TIMEOUT, rx).await;
        self.pending.lock().unwrap().remove(&key);
        let (allowed, verdict) = match answer {
            Ok(Ok(true)) => (true, "✅ Allowed"),
            Ok(Ok(false)) => (false, "❌ Denied"),
            _ => (false, "⌛ Timed out, denied"),
        };
        let done = format!("{verdict}\n```\n{}\n```", action.replace("```", "'''"));
        self.channel.edit(&self.chat, &msg, &done, &[]).await.ok();
        allowed
    }
}
