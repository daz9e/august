//! `chatgpt`: models billed to the user's ChatGPT plan. Provider `chatgpt` is the Responses
//! API with "Sign in with ChatGPT" (`auth.rs`); `chatgpt-codex` the Codex backend with the
//! Codex CLI's login (`codex.rs`). Each is an account signed in to through August
//! (`/login chatgpt`): the link comes into the chat, the redirect lands on localhost (or its
//! address is pasted). August keeps the tokens as this extension's secrets.

mod auth;
mod codex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use august_ext::llm::{LlmProvider, ModelInfo};
use august_ext::{August, Login, Signed};
use august_llm::responses::{Responses, TokenSource};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

const API_BASE: &str = "https://api.openai.com/v1";
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

static AUGUST: std::sync::OnceLock<August> = std::sync::OnceLock::new();

fn august() -> &'static August {
    AUGUST.get().expect("august is set at start")
}

fn home() -> std::path::PathBuf {
    std::env::var("AUGUST_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".august"))
}

/// Secret `key` as JSON (missing: `T::default()`). Tokens an older August kept in
/// `<home>/<legacy>` move into the secret the first time.
async fn load<T: serde::de::DeserializeOwned + serde::Serialize + Default>(key: &str, legacy: &str) -> Result<T> {
    if let Some(s) = august().secret(key).await? {
        return serde_json::from_str(&s).with_context(|| format!("secret {key}"));
    }
    let old = home().join(legacy);
    let Ok(text) = std::fs::read_to_string(&old) else { return Ok(T::default()) };
    let value: T = serde_json::from_str(&text).with_context(|| format!("parse {}", old.display()))?;
    save(key, &value).await?;
    std::fs::rename(&old, old.with_extension("json.migrated")).ok();
    Ok(value)
}

async fn save<T: serde::Serialize>(key: &str, value: &T) -> Result<()> {
    august().set_secret(key, Some(&serde_json::to_string(value)?)).await
}

fn new_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os rng");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Signed-in profile; the mutex serializes refreshes (refresh tokens rotate).
pub struct Session {
    http: reqwest::Client,
    profile: Mutex<auth::Profile>,
}

#[async_trait]
impl TokenSource for Session {
    async fn token(&self) -> Result<String> {
        let mut p = self.profile.lock().await;
        auth::ensure_fresh(&self.http, &mut p).await?;
        Ok(p.access_token.clone())
    }

    async fn on_unauthorized(&self) -> Result<bool> {
        let mut p = self.profile.lock().await;
        auth::refresh(&self.http, &mut p).await?;
        Ok(true)
    }
}

async fn load_profile() -> Result<auth::Profile> {
    let p = auth::AuthFile::load().await?.profile.context("not signed in to ChatGPT, run /login chatgpt")?;
    anyhow::ensure!(p.has_plan_scope(), "signed in, but ChatGPT plan usage was not granted; run /login chatgpt and allow it");
    Ok(p)
}

/// The sessions of this process, one per provider, made at first use and dropped at a
/// login or logout.
static SESSIONS: std::sync::Mutex<Option<Arc<Session>>> = std::sync::Mutex::new(None);
static CODEX: std::sync::Mutex<Option<Arc<codex::Session>>> = std::sync::Mutex::new(None);

async fn session() -> Result<Arc<Session>> {
    if let Some(s) = SESSIONS.lock().unwrap().clone() {
        return Ok(s);
    }
    let s = Arc::new(Session { http: august_llm::http_client(), profile: Mutex::new(load_profile().await?) });
    Ok(SESSIONS.lock().unwrap().get_or_insert(s).clone())
}

async fn codex_session() -> Result<Arc<codex::Session>> {
    if let Some(s) = CODEX.lock().unwrap().clone() {
        return Ok(s);
    }
    let s = Arc::new(codex::session().await?);
    Ok(CODEX.lock().unwrap().get_or_insert(s).clone())
}

/// Models available to the signed-in account (`visibility: "list"` only).
async fn list_models() -> Result<Vec<String>> {
    let token = session().await?.token().await?;
    let resp = august_llm::http_client().get(format!("{API_BASE}/models")).bearer_auth(token).send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    anyhow::ensure!(status.is_success(), "HTTP {status}: {v}");
    let items = v["models"].as_array().or(v["data"].as_array());
    Ok(items
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|s| s == "list"))
        .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()).map(String::from))
        .collect())
}

async fn login(account: &str, steps: Login) -> Result<Signed> {
    let http = august_llm::http_client();
    let email = if account == "chatgpt-codex" {
        let p = codex::login(&http, &steps).await?;
        *CODEX.lock().unwrap() = None;
        p.email
    } else {
        let p = auth::login(&http, &steps).await?;
        *SESSIONS.lock().unwrap() = None;
        anyhow::ensure!(p.has_plan_scope(), "ChatGPT plan usage was not granted, the chatgpt provider will not work");
        p.email
    };
    Ok(Signed { who: email, expires_at: None })
}

async fn logout(account: &str) -> Result<()> {
    if account == "chatgpt-codex" {
        codex::logout().await?;
        *CODEX.lock().unwrap() = None;
    } else {
        auth::logout(&august_llm::http_client()).await?;
        *SESSIONS.lock().unwrap() = None;
    }
    Ok(())
}

fn infos(ids: Vec<String>) -> Vec<ModelInfo> {
    ids.into_iter().map(|id| ModelInfo { id, context_window: None }).collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    AUGUST.set(august.clone()).ok();
    august.register_provider(
        "chatgpt",
        "ChatGPT Plus/Pro (sign in with browser)",
        None,
        |_| async { Ok(infos(list_models().await?)) },
        |req, stream| async move {
            let model = Responses::new(API_BASE, session().await?, req.model, Some(req.effort)).with_options(req.options);
            let mut on_text = |t: &str| stream.text(t);
            model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, &mut on_text).await
        },
    );
    august.register_provider(
        "chatgpt-codex",
        "ChatGPT via Codex login (legacy)",
        None,
        |_| async { Ok(infos(codex::list_models(&august_llm::http_client()).await?)) },
        |req, stream| async move {
            let model = codex::model(codex_session().await?, req.model, req.effort).with_options(req.options);
            let mut on_text = |t: &str| stream.text(t);
            model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, &mut on_text).await
        },
    );
    august.register_login_account("chatgpt", "ChatGPT Plus/Pro", &["chatgpt"], |id, steps| async move { login(&id, steps).await }, |id| async move { logout(&id).await });
    august.register_login_account("chatgpt-codex", "ChatGPT via Codex login (legacy)", &["chatgpt-codex"], |id, steps| async move { login(&id, steps).await }, |id| async move { logout(&id).await });
    // Tell August about sign-ins it doesn't know of (an older version's, kept in files).
    tokio::spawn(async {
        let chatgpt = auth::AuthFile::load().await.ok().and_then(|f| f.profile).map(|p| ("chatgpt", p.email));
        let codex = codex::CodexFile::load().await.ok().and_then(|f| f.profile).map(|p| ("chatgpt-codex", p.email));
        for (id, email) in chatgpt.into_iter().chain(codex) {
            // ponytail: August takes the update only once it lists this extension as running; retry briefly.
            for _ in 0..20 {
                if AUGUST.get().unwrap().account_update(id, "connected", Some(&email)).await.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    });
    august.run().await;
}
