//! `anthropic`: Claude models behind the Anthropic Messages API. Provider `anthropic` uses
//! `base_url`; every entry of `endpoints` is one more provider of its own id that
//! speaks the same API elsewhere:
//!
//! ```json
//! {"endpoints": {"minimax": {"label": "MiniMax", "base_url": "https://api.minimax.io/anthropic/v1",
//!   "key_env": "MINIMAX_API_KEY", "model": "MiniMax-M2", "context_window": 200000}}}
//! ```
//!
//! Each provider is an account signed in to with an API key (`/login <id>`); the variable
//! (`ANTHROPIC_API_KEY`, an endpoint's `key_env`) stands in for it. Endpoints are read at
//! start; `/reload` picks up changes.

use anyhow::{Result, anyhow};
use august_ext::August;
use august_ext::llm::{LlmProvider, ModelInfo};
use august_llm::anthropic::{API_BASE, Anthropic};
use serde_json::{Value, json};

const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_WINDOW: usize = 200_000;

struct Endpoint {
    base_url: String,
    key: String,
    window: usize,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn text(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// The variable that stands in for provider `id`'s key.
fn key_env(settings: &Value, id: &str) -> Option<String> {
    if id == "anthropic" { Some("ANTHROPIC_API_KEY".into()) } else { text(&settings["endpoints"][id], "key_env") }
}

/// Where provider `id` is served and with which key: its variable, else the signed-in key,
/// else one written into the settings by hand (older homes).
async fn endpoint(august: &August, id: &str) -> Result<Endpoint> {
    let settings = august.settings().await?;
    let key = match key_env(&settings, id).and_then(|k| env(&k)) {
        Some(k) => Some(k),
        None => august.secret(id).await?,
    };
    if id == "anthropic" {
        let key = key.or_else(|| text(&settings, "key")).ok_or_else(|| anyhow!("not signed in to Anthropic: run /login anthropic or set ANTHROPIC_API_KEY"))?;
        return Ok(Endpoint {
            base_url: env("ANTHROPIC_BASE_URL").or_else(|| text(&settings, "base_url")).unwrap_or_else(|| API_BASE.into()),
            key,
            window: DEFAULT_WINDOW,
        });
    }
    let e = &settings["endpoints"][id];
    anyhow::ensure!(e.is_object(), "no endpoint `{id}` in the anthropic extension's settings");
    Ok(Endpoint {
        base_url: text(e, "base_url").ok_or_else(|| anyhow!("endpoint `{id}` has no base_url"))?,
        key: key.or_else(|| text(e, "key")).unwrap_or_default(),
        window: e["context_window"].as_u64().map_or(DEFAULT_WINDOW, |n| n as usize),
    })
}

/// Where provider `id` is served (the key aside).
async fn base(august: &August, id: &str) -> Result<(String, usize)> {
    let settings = august.settings().await?;
    if id == "anthropic" {
        return Ok((env("ANTHROPIC_BASE_URL").or_else(|| text(&settings, "base_url")).unwrap_or_else(|| API_BASE.into()), DEFAULT_WINDOW));
    }
    let e = &settings["endpoints"][id];
    let url = text(e, "base_url").ok_or_else(|| anyhow!("endpoint `{id}` has no base_url"))?;
    Ok((url, e["context_window"].as_u64().map_or(DEFAULT_WINDOW, |n| n as usize)))
}

/// The models the server lists.
async fn fetch(base_url: &str, key: &str, window: usize) -> Result<Vec<ModelInfo>> {
    let req = august_llm::http_client()
        .get(format!("{}/models?limit=100", base_url.trim_end_matches('/')))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01");
    let body: Value = req.send().await?.error_for_status()?.json().await?;
    Ok(body["data"].as_array().into_iter().flatten().filter_map(|m| Some(ModelInfo { id: m["id"].as_str()?.into(), context_window: Some(window) })).collect())
}

async fn list(e: &Endpoint, default: Option<&str>) -> Result<Vec<ModelInfo>> {
    let info = |id: &str| ModelInfo { id: id.into(), context_window: Some(e.window) };
    match (fetch(&e.base_url, &e.key, e.window).await, default) {
        (Ok(models), _) if !models.is_empty() => Ok(models),
        // Not every server lists models: its configured one stands in.
        (_, Some(m)) => Ok(vec![info(m)]),
        (r, None) => r,
    }
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
                let model = Anthropic::new(&e.base_url, e.key, req.model, req.effort).with_options(req.options);
                let mut on_text = |t: &str| stream.text(t);
                model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, &mut on_text).await
            }
        },
    );
    // A key is checked by listing the models with it.
    august.register_key_account(id, label, &[id], "API key", key_env(settings, id).as_deref(), move |id, key| {
        let august = for_check.clone();
        async move {
            let (url, window) = base(&august, &id).await?;
            fetch(&url, &key, window).await.map(drop)
        }
    });
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "base_url": {"type": "string", "default": API_BASE, "description": "Address of the Anthropic API"},
            "endpoints": {"type": "object", "description": "More providers by id that speak the Anthropic API: {label, base_url, key_env, model, context_window}"},
        },
    }));
    provide(&august, &Value::Null, "anthropic", "Anthropic (API key)", Some(DEFAULT_MODEL));
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
