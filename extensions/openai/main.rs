//! `openai`: models behind an OpenAI-compatible Chat Completions API (OpenAI itself,
//! OpenRouter, Ollama, vLLM, ...). Provider `openai` uses `base_url`; every entry of
//! `endpoints` is one more provider of its own id:
//!
//! ```json
//! {"endpoints": {"openrouter": {"label": "OpenRouter", "base_url": "https://openrouter.ai/api/v1",
//!   "key_env": "OPENROUTER_API_KEY", "model": "anthropic/claude-sonnet-4.5", "context_window": 200000}}}
//! ```
//!
//! Each provider is an account signed in to with an API key (`/login <id>`); the variable
//! (`OPENAI_API_KEY`, an endpoint's `key_env`) stands in for it, and a local server needs
//! none. Endpoints are read at start; `/reload` picks up changes.

use anyhow::{Result, anyhow};
use august_ext::August;
use august_ext::llm::{LlmProvider, ModelInfo};
use august_llm::openai::OpenAi;
use serde_json::{Value, json};

const DEFAULT_URL: &str = "https://api.openai.com/v1";

/// One provider's connection, resolved at the time of a call.
struct Endpoint {
    base_url: String,
    key: String,
    window: Option<usize>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn text(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// The variable that stands in for provider `id`'s key.
fn key_env(settings: &Value, id: &str) -> Option<String> {
    if id == "openai" { Some("OPENAI_API_KEY".into()) } else { text(&settings["endpoints"][id], "key_env") }
}

/// Where provider `id` is served and with which key: its variable, else the signed-in key,
/// else one written into the settings by hand (older homes).
async fn endpoint(august: &August, id: &str) -> Result<Endpoint> {
    let settings = august.settings().await?;
    let key = match key_env(&settings, id).and_then(|k| env(&k)) {
        Some(k) => Some(k),
        None => august.secret(id).await?,
    };
    if id == "openai" {
        return Ok(Endpoint {
            base_url: env("OPENAI_BASE_URL").or_else(|| text(&settings, "base_url")).unwrap_or_else(|| DEFAULT_URL.into()),
            key: key.or_else(|| text(&settings, "key")).unwrap_or_default(),
            window: None,
        });
    }
    let e = &settings["endpoints"][id];
    anyhow::ensure!(e.is_object(), "no endpoint `{id}` in the openai extension's settings");
    Ok(Endpoint {
        base_url: text(e, "base_url").ok_or_else(|| anyhow!("endpoint `{id}` has no base_url"))?,
        key: key.or_else(|| text(e, "key")).unwrap_or_default(),
        window: e["context_window"].as_u64().map(|n| n as usize),
    })
}

async fn list(e: &Endpoint, default: Option<&str>) -> Result<Vec<ModelInfo>> {
    let mut req = august_llm::http_client().get(format!("{}/models", e.base_url.trim_end_matches('/')));
    if !e.key.is_empty() {
        req = req.bearer_auth(&e.key);
    }
    let body: Value = req.send().await?.error_for_status()?.json().await?;
    let mut models: Vec<ModelInfo> = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| Some(ModelInfo { id: m["id"].as_str()?.to_string(), context_window: e.window }))
        .collect();
    if models.is_empty() && let Some(m) = default {
        models.push(ModelInfo { id: m.into(), context_window: e.window });
    }
    Ok(models)
}

fn provide(august: &August, settings: &Value, id: &str, label: &str, default_model: Option<&str>) {
    let (for_models, for_complete, for_check) = (august.clone(), august.clone(), august.clone());
    let default = default_model.map(String::from);
    august.register_provider(
        id,
        label,
        default_model,
        move |id| {
            let (august, default) = (for_models.clone(), default.clone());
            async move { list(&endpoint(&august, &id).await?, default.as_deref()).await }
        },
        move |req, stream| {
            let august = for_complete.clone();
            async move {
                let e = endpoint(&august, &req.provider).await?;
                let model = OpenAi::new(e.base_url, e.key, req.model).with_context_window(e.window).with_options(req.options);
                let mut on_text = |t: &str| stream.text(t);
                model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, &mut on_text).await
            }
        },
    );
    // A key is checked by listing the models with it.
    august.register_key_account(id, label, &[id], "API key", key_env(settings, id).as_deref(), move |id, key| {
        let august = for_check.clone();
        async move {
            let mut e = endpoint(&august, &id).await?;
            e.key = key;
            list(&e, None).await.map(drop)
        }
    });
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "base_url": {"type": "string", "default": DEFAULT_URL, "description": "Address of the OpenAI-compatible API"},
            "endpoints": {"type": "object", "description": "More providers by id: {label, base_url, key_env, model, context_window}"},
        },
    }));
    provide(&august, &Value::Null, "openai", "OpenAI-compatible (OpenAI, OpenRouter, Ollama, ...)", None);
    // Settings can only be read once the link runs.
    let me = august.clone();
    tokio::spawn(async move {
        let Ok(settings) = me.settings().await else { return };
        for (id, e) in settings["endpoints"].as_object().into_iter().flatten() {
            provide(&me, &settings, id, e["label"].as_str().unwrap_or(id), e["model"].as_str());
        }
    });
    august.run().await;
}
