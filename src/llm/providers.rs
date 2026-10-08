//! Provider catalog: which providers exist, where their credentials come from,
//! how to list their models and how to build them. Env vars override `~/.august`.

use crate::llm::{self, LlmProvider};
use crate::config::{self, ApiCredential, Config};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    ApiKey,
    /// Browser sign-in (ChatGPT).
    OAuth,
    /// Browser sign-in with the Codex CLI client (legacy ChatGPT login).
    CodexOAuth,
    /// An extension holds the settings and the login.
    Extension,
}

/// One vendor: identity, how it authenticates, and how to build / list it.
#[async_trait]
pub trait ProviderDef: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn auth(&self) -> Auth;
    /// Env var that overrides the stored API key (empty if none).
    fn key_env(&self) -> &'static str {
        ""
    }
    fn default_model(&self) -> Option<&'static str> {
        None
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>>;
    /// Models the provider offers with the given credentials.
    async fn list_models(&self, cred: Option<&ApiCredential>) -> Result<Vec<String>>;
}

/// All providers, in menu order.
pub fn registry() -> &'static [&'static dyn ProviderDef] {
    static REGISTRY: [&dyn ProviderDef; 2] = [
        &ChatGptDef,
        &CodexDef,
    ];
    &REGISTRY
}

/// Built-in providers.
pub fn all() -> impl Iterator<Item = &'static dyn ProviderDef> {
    registry().iter().copied()
}

/// A built-in provider, or else one an extension offers (resolved when it is first used).
pub fn info(id: &str) -> Result<&'static dyn ProviderDef> {
    anyhow::ensure!(config::valid_name(id), "bad provider id: {id}");
    if let Some(p) = all().find(|p| p.id() == id) {
        return Ok(p);
    }
    static REMOTE: std::sync::Mutex<Vec<&'static RemoteDef>> = std::sync::Mutex::new(Vec::new());
    let mut remote = REMOTE.lock().unwrap();
    if let Some(p) = remote.iter().find(|p| p.id == id) {
        return Ok(*p);
    }
    let def: &'static RemoteDef = Box::leak(Box::new(RemoteDef { id: Box::leak(id.to_string().into_boxed_str()) }));
    remote.push(def);
    Ok(def)
}

/// A provider an extension offers; the extension answers for everything about it.
struct RemoteDef {
    id: &'static str,
}

#[async_trait]
impl ProviderDef for RemoteDef {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        self.id
    }
    fn auth(&self) -> Auth {
        Auth::Extension
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        Ok(Arc::new(llm::remote::Remote::new(self.id, sel.model.as_deref().unwrap_or_default(), &sel.effort)))
    }
    async fn list_models(&self, _cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        Ok(llm::remote::models(self.id).await?.into_iter().map(|m| m.id).collect())
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Credential for an API-key provider: env first, then its settings file.
pub fn credential(p: &dyn ProviderDef) -> Result<Option<ApiCredential>> {
    if !matches!(p.auth(), Auth::ApiKey) {
        return Ok(None);
    }
    let stored = config::credentials()?.remove(p.id());
    let key = env(p.key_env()).or_else(|| stored.as_ref().map(|c| c.key.clone()));
    Ok(key.map(|key| ApiCredential { key, base_url: None }))
}

/// Handle to a registered provider; `id` is exposed as a field for convenience.
#[derive(Clone, Copy)]
pub struct Provider {
    pub id: &'static str,
    pub def: &'static dyn ProviderDef,
}

impl std::ops::Deref for Provider {
    type Target = dyn ProviderDef;
    fn deref(&self) -> &Self::Target {
        self.def
    }
}

/// Active provider, model and effort: env vars, then `august.json`.
pub struct Selection {
    pub provider: Provider,
    pub model: Option<String>,
    pub effort: String,
}

pub fn selection() -> Result<Selection> {
    let cfg: Config = config::app()?;
    let id = env("AUGUST_PROVIDER")
        .or(cfg.provider.clone())
        .context("no provider configured: run `cargo run -- login`")?;
    let def = info(&id)?;
    let provider = Provider { id: def.id(), def };
    // A model saved for another provider must not leak into an env-selected one.
    let cfg_model = cfg.model.filter(|_| cfg.provider.as_deref() == Some(provider.id()));
    Ok(Selection {
        provider,
        model: env("AUGUST_MODEL")
            .or(cfg_model)
            .or(provider.default_model().map(String::from)),
        effort: env("AUGUST_EFFORT")
            .or(cfg.effort)
            .unwrap_or_else(|| "medium".into()),
    })
}

/// Builds the active provider, wrapped with retries and the optional fallback
/// (`AUGUST_FALLBACK` or `fallback` in `august.json`, as `provider:model`).
pub async fn build(sel: Selection) -> Result<Arc<dyn LlmProvider>> {
    let primary = sel.provider.def.build(&sel).await?;
    let fallback = match fallback_selection(&sel) {
        Ok(Some(fb)) => match fb.provider.def.build(&fb).await {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("fallback provider unavailable: {e:#}");
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            eprintln!("bad fallback setting: {e:#}");
            None
        }
    };
    Ok(Arc::new(llm::resilient::Resilient::new(primary, fallback)))
}

/// Builds a provider for `spec`: a model of the active provider, or `provider:model`.
pub async fn build_spec(spec: &str) -> Result<Arc<dyn LlmProvider>> {
    let mut sel = selection()?;
    match spec.split_once(':').and_then(|(id, m)| Some((info(id).ok()?, m))) {
        Some((def, model)) => {
            let model = Some(model.to_string()).filter(|m| !m.is_empty()).or(def.default_model().map(String::from));
            sel = Selection { provider: Provider { id: def.id(), def }, model, effort: sel.effort };
        }
        // Model names may hold a colon themselves (`llama3:8b`).
        None => sel.model = Some(spec.to_string()),
    }
    build(sel).await
}

fn fallback_selection(active: &Selection) -> Result<Option<Selection>> {
    let cfg: Config = config::app()?;
    let Some(spec) = env("AUGUST_FALLBACK").or(cfg.fallback) else {
        return Ok(None);
    };
    let (id, model) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
    let def = info(id.trim())?;
    let model = Some(model.trim().to_string())
        .filter(|m| !m.is_empty())
        .or(def.default_model().map(String::from));
    Ok(Some(Selection {
        provider: Provider { id: def.id(), def },
        model,
        effort: active.effort.clone(),
    }))
}

/// Models the provider offers with the given credentials.
pub async fn list_models(p: &dyn ProviderDef, cred: Option<&ApiCredential>) -> Result<Vec<String>> {
    p.list_models(cred).await
}

fn need_model(p: &dyn ProviderDef, sel: &Selection) -> Result<String> {
    sel.model
        .clone()
        .with_context(|| format!("no model selected for {}: run `cargo run -- model`", p.id()))
}


struct ChatGptDef;

#[async_trait]
impl ProviderDef for ChatGptDef {
    fn id(&self) -> &'static str {
        "chatgpt"
    }
    fn label(&self) -> &'static str {
        "ChatGPT Plus/Pro (sign in with browser)"
    }
    fn auth(&self) -> Auth {
        Auth::OAuth
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        Ok(Arc::new(
            llm::chatgpt::provider(sel.model.clone(), sel.effort.clone()).await?,
        ))
    }
    async fn list_models(&self, _cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        llm::chatgpt::list_models(&crate::util::http_client()).await
    }
}

struct CodexDef;

#[async_trait]
impl ProviderDef for CodexDef {
    fn id(&self) -> &'static str {
        "chatgpt-codex"
    }
    fn label(&self) -> &'static str {
        "ChatGPT via Codex login (legacy)"
    }
    fn auth(&self) -> Auth {
        Auth::CodexOAuth
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        Ok(Arc::new(
            llm::chatgpt::codex::provider(need_model(self, sel)?, sel.effort.clone()).await?,
        ))
    }
    async fn list_models(&self, _cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        llm::chatgpt::codex::list_models(&crate::util::http_client()).await
    }
}

#[cfg(test)]
mod tests {
    /// Live streaming smoke test against the configured provider:
    /// `cargo test live_stream -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_stream() {
        let p = super::build(super::selection().unwrap()).await.unwrap();
        let mut frags = 0;
        let mut text = String::new();
        let c = p
            .complete_stream(
                "test",
                "Be brief.",
                &[crate::llm::Message::user_text("Count from 1 to 5.")],
                &[],
                &mut |t| {
                    frags += 1;
                    text.push_str(t);
                },
            )
            .await
            .unwrap();
        println!("fragments={frags} text={text:?} usage={:?}", c.usage);
        assert_eq!(text, c.message.text());
    }
}
