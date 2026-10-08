//! `opencode`: OpenCode Zen (pay as you go, provider `opencode`) and OpenCode Go
//! (subscription, provider `opencode-go`). One API key, but each model speaks its own wire
//! format; models.dev says which (setting `format` forces one: chat | responses | messages).

use anyhow::{Context, Result, anyhow};
use august_ext::August;
use august_ext::llm::{Completion, LlmProvider, ModelInfo, Request};
use august_llm::anthropic::Anthropic;
use august_llm::openai::OpenAi;
use august_llm::responses::{ApiKey, Responses};
use serde_json::{Value, json};

const ZEN_BASE: &str = "https://opencode.ai/zen/v1";
const GO_BASE: &str = "https://opencode.ai/zen/go/v1";
/// Required by OpenCode Go: a stable id per conversation, for routing and caching.
const SESSION_HEADER: &str = "x-opencode-session";
const MODELS_DEV: &str = "https://models.dev/api.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `/chat/completions`
    Chat,
    /// `/responses`
    Responses,
    /// `/messages` (Anthropic)
    Messages,
}

fn parse_format(s: &str) -> Result<Format> {
    Ok(match s {
        "chat" => Format::Chat,
        "responses" => Format::Responses,
        "messages" => Format::Messages,
        other => anyhow::bail!("unknown format: {other} (chat | responses | messages)"),
    })
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn text(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// `opencode` or `opencode-go`: where it is served (`base_url` setting overrides both).
fn base_url(settings: &Value, id: &str) -> String {
    text(settings, "base_url").unwrap_or_else(|| if id == "opencode-go" { GO_BASE } else { ZEN_BASE }.into())
}

/// models.dev, the catalog opencode itself uses (fetched once).
async fn catalog() -> Result<&'static Value> {
    static CATALOG: tokio::sync::OnceCell<Value> = tokio::sync::OnceCell::const_new();
    CATALOG
        .get_or_try_init(|| async {
            let req = august_llm::http_client().get(MODELS_DEV).timeout(std::time::Duration::from_secs(15));
            anyhow::Ok(req.send().await?.error_for_status()?.json().await?)
        })
        .await
}

/// The model's wire format from its AI SDK package in models.dev.
async fn catalog_format(id: &str, model: &str) -> Result<Option<Format>> {
    let provider = &catalog().await?[id];
    let entry = &provider["models"][model];
    if entry.is_null() {
        return Ok(None);
    }
    let npm = entry["provider"]["npm"].as_str().or(provider["npm"].as_str()).unwrap_or_default();
    Ok(Some(match npm {
        "@ai-sdk/anthropic" => Format::Messages,
        "@ai-sdk/openai" => Format::Responses,
        "@ai-sdk/openai-compatible" => Format::Chat,
        other => anyhow::bail!("model {model} uses {other}, which August does not support yet"),
    }))
}

/// Name-based fallback when models.dev is unreachable or doesn't list the model.
fn guess_format(model: &str) -> Format {
    if model.starts_with("claude-") {
        Format::Messages
    } else if model.starts_with("gpt-") {
        Format::Responses
    } else {
        Format::Chat
    }
}

async fn complete(august: &August, req: Request, on_text: &mut (dyn for<'a> FnMut(&'a str) + Send)) -> Result<Completion> {
    let settings = august.settings().await?;
    let key = match env("OPENCODE_API_KEY") {
        Some(k) => Some(k),
        None => august.secret("opencode").await?,
    };
    let key = key.or_else(|| text(&settings, "key")).ok_or_else(|| anyhow!("not signed in to OpenCode: run /login opencode or set OPENCODE_API_KEY"))?;
    let format = match env("AUGUST_API_FORMAT").or_else(|| text(&settings, "format")) {
        Some(f) => parse_format(&f)?,
        None => catalog_format(&req.provider, &req.model).await.ok().flatten().unwrap_or_else(|| guess_format(&req.model)),
    };
    let base = base_url(&settings, &req.provider);
    let model: Box<dyn LlmProvider> = match format {
        Format::Messages => Box::new(Anthropic::new(&base, key, req.model.clone(), req.effort.clone()).with_session_header(SESSION_HEADER)),
        Format::Chat => Box::new(OpenAi::new(base, key, req.model.clone()).with_session_header(SESSION_HEADER)),
        Format::Responses => {
            // Only OpenAI's own models take `reasoning` (grok, muse, ... may reject it).
            let effort = req.model.starts_with("gpt-").then(|| req.effort.clone());
            Box::new(Responses::new(&base, ApiKey(key), req.model.clone(), effort).with_session_header(SESSION_HEADER))
        }
    };
    model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, on_text).await
}

/// Models served by the gateway (public endpoint, no key needed), with their context
/// windows from models.dev. Free-tier models (`*-free`) are left out: OpenCode only serves
/// them to its own client.
async fn list(settings: Value, id: String) -> Result<Vec<ModelInfo>> {
    let v: Value = august_llm::http_client().get(format!("{}/models", base_url(&settings, &id))).send().await?.error_for_status()?.json().await?;
    let catalog = catalog().await.ok();
    let window = |m: &str| catalog.and_then(|c| c[&id]["models"][m]["limit"]["context"].as_u64()).map(|n| n as usize);
    let models = v["data"].as_array().context("unexpected /models response")?;
    Ok(models
        .iter()
        .filter_map(|m| m["id"].as_str())
        .filter(|m| !m.ends_with("-free"))
        .map(|m| ModelInfo { id: m.into(), context_window: window(m) })
        .collect())
}

fn provide(august: &August, id: &str, label: &str) {
    let (for_models, for_complete) = (august.clone(), august.clone());
    august.register_provider(
        id,
        label,
        None,
        move |id| {
            let august = for_models.clone();
            async move { list(august.settings().await?, id).await }
        },
        move |req, stream| {
            let august = for_complete.clone();
            async move {
                let mut on_text = |t: &str| stream.text(t);
                complete(&august, req, &mut on_text).await
            }
        },
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "format": {"type": "string", "enum": ["chat", "responses", "messages"], "description": "Force one wire format instead of asking models.dev"},
            "base_url": {"type": "string", "description": "Another address of the OpenCode API"},
        },
    }));
    provide(&august, "opencode-go", "OpenCode Go (subscription)");
    provide(&august, "opencode", "OpenCode Zen (pay as you go)");
    // One key for both; the API lists models without one, so it can't be checked up front.
    august.register_key_account("opencode", "OpenCode Zen / Go", &["opencode-go", "opencode"], "API key", Some("OPENCODE_API_KEY"), |_, _| async { Ok(()) });
    august.run().await;
}
