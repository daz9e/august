//! A model provider that lives in an extension: August sends `complete`, the extension
//! streams text back and replies with the completion.

use super::error::{ErrorKind, ProviderError};
use super::*;
use crate::extensions::Extensions;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub struct Remote {
    /// The extensions it is reached through.
    ext: Weak<Extensions>,
    id: String,
    /// Empty until known: the provider's default model is asked for at the first call.
    model: OnceLock<String>,
    effort: String,
    /// Merged into the provider's request body (see `Request::options`).
    options: Value,
    /// Learned from the provider's model list after the first call.
    window: Mutex<Option<usize>>,
}

impl Remote {
    pub fn new(ext: &Arc<Extensions>, id: &str, model_name: &str, effort: &str) -> Self {
        let model = OnceLock::new();
        if !model_name.is_empty() {
            model.set(model_name.to_string()).ok();
        }
        Self { ext: Arc::downgrade(ext), id: id.into(), model, effort: effort.into(), options: Value::Null, window: Mutex::new(None) }
    }

    fn extensions(&self) -> Result<Arc<Extensions>> {
        self.ext.upgrade().ok_or_else(|| anyhow::anyhow!("extensions are not running"))
    }
}

/// The model provider `id` starts with when none is chosen: its default, else the first
/// it lists.
pub async fn default_model(ext: &Extensions, id: &str) -> Result<String> {
    anyhow::ensure!(!id.is_empty(), "no model provider is chosen yet: sign in to one with /login");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(p) = ext.providers().into_iter().map(|(_, p)| p).find(|p| p.id == id) {
            if let Some(m) = p.default_model {
                return Ok(m);
            }
            let first = models(ext, id).await?.into_iter().next();
            return first.map(|m| m.id).ok_or_else(|| anyhow::anyhow!("{id} offers no models"));
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no provider `{id}` is offered by any extension");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The models provider `id` offers.
pub async fn models(ext: &Extensions, id: &str) -> Result<Vec<ModelInfo>> {
    let list = ext.call_provider(id, "models", serde_json::json!({"provider": id}), None).await.map_err(|e| anyhow::anyhow!(e.message))?;
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

    fn tuned(&self, effort: Option<&str>, options: &Value) -> Option<Arc<dyn LlmProvider>> {
        let model = OnceLock::new();
        if !self.name().is_empty() {
            model.set(self.name().to_string()).ok();
        }
        let effort = effort.unwrap_or(&self.effort).to_string();
        let mut tuned = Self { ext: self.ext.clone(), id: self.id.clone(), model, effort, options: Value::Null, window: Mutex::new(None) };
        tuned.options = if options.is_null() { self.options.clone() } else { options.clone() };
        *tuned.window.lock().unwrap() = self.context_window();
        Some(Arc::new(tuned))
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
                let m = default_model(&*self.extensions()?, &self.id).await?;
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
            options: self.options.clone(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let ext = self.extensions()?;
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
        if self.window.lock().unwrap().is_none() && let Ok(list) = models(&ext, &self.id).await {
            *self.window.lock().unwrap() = list.iter().find(|m| m.id == model).and_then(|m| m.context_window);
        }
        Ok(completion)
    }
}
