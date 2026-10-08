//! Provider catalog: which providers exist, where their credentials come from,
//! how to list their models and how to build them. Env vars override `~/.august`.

use crate::llm::{self, LlmProvider};
use crate::config::{self, ApiCredential, Config};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    ApiKey,
    /// Browser sign-in (ChatGPT).
    OAuth,
    /// Browser sign-in with the Codex CLI client (legacy ChatGPT login).
    CodexOAuth,
    /// A local CLI that holds its own login (Claude Code).
    Cli,
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
    static REGISTRY: [&dyn ProviderDef; 6] = [
        &OpenCode { go: true },
        &OpenCode { go: false },
        &AnthropicDef,
        &ClaudeCliDef,
        &ChatGptDef,
        &CodexDef,
    ];
    &REGISTRY
}

/// Built-in providers, then the user's own (`config/providers/<id>.json` with a `format`).
pub fn all() -> impl Iterator<Item = &'static dyn ProviderDef> {
    registry().iter().copied().chain(custom().iter().copied())
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

/// A provider of the user's own, `config/providers/<id>.json`: one of the wire formats
/// August speaks (`openai`, `anthropic`) at another address, e.g. `openrouter.json`:
/// `{"format": "openai", "base_url": "https://openrouter.ai/api/v1",
/// "key_env": "OPENROUTER_API_KEY", "model": "anthropic/claude-sonnet-4.5", "context_window": 200000}`.
/// The key comes from `key_env`, else `key` in the same file, else none (local servers).
#[derive(serde::Deserialize)]
struct CustomConfig {
    label: Option<String>,
    format: String,
    base_url: String,
    key_env: Option<String>,
    model: Option<String>,
}

struct CustomDef {
    id: &'static str,
    label: &'static str,
    key_env: &'static str,
    model: Option<&'static str>,
    cfg: CustomConfig,
}

fn custom() -> &'static [&'static dyn ProviderDef] {
    static CUSTOM: std::sync::OnceLock<Vec<&'static dyn ProviderDef>> = std::sync::OnceLock::new();
    CUSTOM.get_or_init(|| {
        let all: std::collections::BTreeMap<String, CustomConfig> = config::units("providers")
            .unwrap_or_else(|e| {
                eprintln!("provider settings: {e:#}");
                Default::default()
            })
            .into_iter()
            // A file with a `format` is a provider of the user's own; others hold a key.
            .filter(|(_, v)| v.get("format").is_some())
            .filter_map(|(id, v)| match serde_json::from_value(v) {
                Ok(cfg) => Some((id, cfg)),
                Err(e) => {
                    eprintln!("config/providers/{id}.json: {e}");
                    None
                }
            })
            .collect();
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        all.into_iter()
            .filter(|(id, _)| !registry().iter().any(|p| p.id() == id))
            .map(|(id, cfg)| {
                let def = CustomDef {
                    label: leak(cfg.label.clone().unwrap_or_else(|| id.clone())),
                    key_env: leak(cfg.key_env.clone().unwrap_or_default()),
                    model: cfg.model.clone().map(leak),
                    id: leak(id),
                    cfg,
                };
                &*Box::leak(Box::new(def)) as &'static dyn ProviderDef
            })
            .collect()
    })
}

#[async_trait]
impl ProviderDef for CustomDef {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        self.label
    }
    fn auth(&self) -> Auth {
        Auth::ApiKey
    }
    fn key_env(&self) -> &'static str {
        self.key_env
    }
    fn default_model(&self) -> Option<&'static str> {
        self.model
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        let key = credential(self)?.map(|c| c.key).unwrap_or_default();
        let model = need_model(self, sel)?;
        Ok(match self.cfg.format.as_str() {
            "openai" => anyhow::bail!("provider {}: OpenAI-compatible endpoints are settings of the `openai` extension now (`endpoints`)", self.id),
            "anthropic" => Arc::new(llm::anthropic::Anthropic::new(&self.cfg.base_url, key, model, sel.effort.clone())),
            other => anyhow::bail!("provider {}: unknown format `{other}` (openai or anthropic)", self.id),
        })
    }
    async fn list_models(&self, cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        if self.cfg.format != "openai" {
            return Ok(self.model.map(String::from).into_iter().collect());
        }
        let mut req = crate::util::http_client().get(format!("{}/models", self.cfg.base_url.trim_end_matches('/')));
        if let Some(c) = cred.filter(|c| !c.key.is_empty()) {
            req = req.bearer_auth(&c.key);
        }
        Ok(model_ids(req.send().await?.error_for_status()?.json().await?))
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

fn require(p: &dyn ProviderDef, cred: Option<ApiCredential>) -> Result<ApiCredential> {
    cred.with_context(|| {
        format!(
            "no credentials for {}: run `cargo run -- login` or set {}",
            p.id(),
            p.key_env()
        )
    })
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

fn model_ids(v: Value) -> Vec<String> {
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect()
}

struct OpenCode {
    go: bool,
}

impl OpenCode {
    fn plan(&self) -> llm::opencode::Plan {
        if self.go {
            llm::opencode::Plan::Go
        } else {
            llm::opencode::Plan::Zen
        }
    }
}

#[async_trait]
impl ProviderDef for OpenCode {
    fn id(&self) -> &'static str {
        if self.go { "opencode-go" } else { "opencode" }
    }
    fn label(&self) -> &'static str {
        if self.go {
            "OpenCode Go (subscription)"
        } else {
            "OpenCode Zen (pay as you go)"
        }
    }
    fn auth(&self) -> Auth {
        Auth::ApiKey
    }
    fn key_env(&self) -> &'static str {
        "OPENCODE_API_KEY"
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        llm::opencode::provider(
            self.plan(),
            require(self, credential(self)?)?.key,
            need_model(self, sel)?,
            sel.effort.clone(),
            env("AUGUST_API_FORMAT").map(|v| v.parse()).transpose()?,
        )
        .await
    }
    async fn list_models(&self, _cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        llm::opencode::list_models(self.plan()).await
    }
}

struct AnthropicDef;

#[async_trait]
impl ProviderDef for AnthropicDef {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn label(&self) -> &'static str {
        "Anthropic (API key)"
    }
    fn auth(&self) -> Auth {
        Auth::ApiKey
    }
    fn key_env(&self) -> &'static str {
        "ANTHROPIC_API_KEY"
    }
    fn default_model(&self) -> Option<&'static str> {
        Some("claude-opus-5-5")
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        Ok(Arc::new(llm::anthropic::Anthropic::new(
            llm::anthropic::API_BASE,
            require(self, credential(self)?)?.key,
            need_model(self, sel)?,
            sel.effort.clone(),
        )))
    }
    async fn list_models(&self, cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        let c = require(self, cred.cloned())?;
        let v: Value = crate::util::http_client()
            .get(format!("{}/models?limit=100", llm::anthropic::API_BASE))
            .header("x-api-key", &c.key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(model_ids(v))
    }
}

struct ClaudeCliDef;

#[async_trait]
impl ProviderDef for ClaudeCliDef {
    fn id(&self) -> &'static str {
        "claude-cli"
    }
    fn label(&self) -> &'static str {
        "Claude Pro/Max via the Claude Code CLI (tools via text protocol)"
    }
    fn auth(&self) -> Auth {
        Auth::Cli
    }
    fn default_model(&self) -> Option<&'static str> {
        Some("opus")
    }
    async fn build(&self, sel: &Selection) -> Result<Arc<dyn LlmProvider>> {
        llm::claude_cli::check_installed().await?;
        Ok(Arc::new(llm::claude_cli::ClaudeCli {
            model: sel.model.clone(),
            effort: sel.effort.clone(),
        }))
    }
    async fn list_models(&self, _cred: Option<&ApiCredential>) -> Result<Vec<String>> {
        // Aliases the CLI resolves to the latest model of each family.
        Ok(["opus", "sonnet", "haiku"].map(String::from).to_vec())
    }
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
