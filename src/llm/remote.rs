//! A model provider that lives in an extension: August sends `complete`, the extension
//! streams text back and replies with the completion.

use super::error::{ErrorKind, ProviderError};
use super::*;
use crate::extensions::Extensions;
use std::sync::{Arc, Mutex, OnceLock, Weak};

static EXTENSIONS: OnceLock<Weak<Extensions>> = OnceLock::new();

/// The extensions remote providers are reached through (set once by the gateway).
pub fn use_extensions(ext: &Arc<Extensions>) {
    EXTENSIONS.set(Arc::downgrade(ext)).ok();
}

fn extensions() -> Result<Arc<Extensions>> {
    EXTENSIONS.get().and_then(Weak::upgrade).ok_or_else(|| anyhow::anyhow!("extensions are not running"))
}

pub struct Remote {
    id: String,
    /// Empty until known: the provider's default model is asked for at the first call.
    model: OnceLock<String>,
    effort: String,
    /// Learned from the provider's model list after the first call.
    window: Mutex<Option<usize>>,
}

impl Remote {
    pub fn new(id: &str, model_name: &str, effort: &str) -> Self {
        let model = OnceLock::new();
        if !model_name.is_empty() {
            model.set(model_name.to_string()).ok();
        }
        Self { id: id.into(), model, effort: effort.into(), window: Mutex::new(None) }
    }
}

/// The model provider `id` starts with when none is chosen.
pub async fn default_model(id: &str) -> Result<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(p) = extensions()?.providers().into_iter().find(|p| p.id == id) {
            return p.default_model.ok_or_else(|| anyhow::anyhow!("no model selected for {id}: run `august model`"));
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no provider `{id}` is offered by any extension");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The models provider `id` offers.
pub async fn models(id: &str) -> Result<Vec<ModelInfo>> {
    let list = extensions()?.call_provider(id, "models", serde_json::json!({"provider": id}), None).await.map_err(|e| anyhow::anyhow!(e.message))?;
    Ok(list.as_array().into_iter().flatten().filter_map(ModelInfo::from_json).collect())
}

#[async_trait]
impl LlmProvider for Remote {
    fn name(&self) -> &str {
        self.model.get().map_or("", String::as_str)
    }

    fn context_window(&self) -> Option<usize> {
        *self.window.lock().unwrap()
    }

    async fn complete(&self, session: &str, system: &str, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        self.complete_stream(session, system, messages, tools, &mut |_| {}).await
    }

    async fn complete_stream(
        &self,
        session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let model = match self.model.get() {
            Some(m) => m.clone(),
            None => {
                let m = default_model(&self.id).await?;
                self.model.set(m.clone()).ok();
                m
            }
        };
        let req = Request {
            provider: self.id.clone(),
            model: model.clone(),
            effort: self.effort.clone(),
            session: session.into(),
            system: system.into(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let ext = extensions()?;
        let call = ext.call_provider(&self.id, "complete", req.to_json(), Some(tx));
        tokio::pin!(call);
        let mut show = |event: Value| {
            if event["type"] == "text" && let Some(t) = event["text"].as_str() {
                on_text(t);
            }
        };
        let result = loop {
            tokio::select! {
                r = &mut call => break r,
                Some(event) = rx.recv() => show(event),
            }
        };
        while let Ok(event) = rx.try_recv() {
            show(event);
        }
        let value = result.map_err(|e| match e.kind {
            Some(kind) => anyhow::Error::from(ProviderError { kind: ErrorKind::parse(&kind), message: e.message }),
            None => anyhow::anyhow!(e.message),
        })?;
        let completion = Completion::from_json(&value).ok_or_else(|| anyhow::anyhow!("provider {} sent a bad completion", self.id))?;
        if self.window.lock().unwrap().is_none() && let Ok(list) = models(&self.id).await {
            *self.window.lock().unwrap() = list.iter().find(|m| m.id == model).and_then(|m| m.context_window);
        }
        Ok(completion)
    }
}
