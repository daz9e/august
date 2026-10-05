//! Legacy sign-in: the Codex CLI's public OAuth client and the `chatgpt.com/backend-api/codex`
//! Responses endpoint. Same wire format as `responses.rs`, billed to the ChatGPT plan.

use super::auth::{TokenError, id_claims, now, random_b64, respond, token_request_to};
use super::*;
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const API_BASE: &str = "https://chatgpt.com/backend-api/codex";
const ORIGINATOR: &str = "august";
const CLAIM: &str = "https://api.openai.com/auth";
const EXPIRY_MARGIN_SECS: u64 = 180;
const AUTH_FILE: &str = "codex-auth.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodexFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<CodexProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexProfile {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl CodexFile {
    pub fn load() -> Result<Self> {
        crate::config::load(AUTH_FILE)
    }
    fn save(&self) -> Result<()> {
        crate::config::save(AUTH_FILE, self)
    }
}

pub fn is_signed_in() -> Result<bool> {
    Ok(CodexFile::load()?.profile.is_some())
}

pub fn logout() -> Result<()> {
    let mut f = CodexFile::load()?;
    f.profile = None;
    f.save()
}

fn profile_from(tok: super::auth::TokenResponse, old: Option<&CodexProfile>) -> Result<CodexProfile> {
    let claims = id_claims(&tok.access_token)?;
    let account_id = claims[CLAIM]["chatgpt_account_id"]
        .as_str()
        .context("access token has no chatgpt_account_id")?
        .to_string();
    let email = tok
        .id_token
        .as_deref()
        .and_then(|t| id_claims(t).ok())
        .and_then(|c| c["email"].as_str().map(String::from))
        .or_else(|| old.map(|p| p.email.clone()))
        .unwrap_or_default();
    Ok(CodexProfile {
        refresh_token: tok
            .refresh_token
            .or_else(|| old.map(|p| p.refresh_token.clone()))
            .context("no refresh_token in token response")?,
        access_token: tok.access_token,
        account_id,
        email,
        expires_at: now() + tok.expires_in,
    })
}

pub async fn login(http: &reqwest::Client) -> Result<CodexProfile> {
    let verifier = random_b64(32);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16);

    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| {
            format!("port {CALLBACK_PORT} is in use, probably by an unfinished login or the Codex CLI")
        })?;
    println!("Opening the browser to sign in with ChatGPT. If it did not open, visit:\n\n{url}\n");
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let code = wait_for_code(listener, state).await?;
    let tok = token_request_to(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await?;
    let profile = profile_from(tok, None)?;
    let mut file = CodexFile::load()?;
    file.profile = Some(profile.clone());
    file.save()?;
    Ok(profile)
}

/// Serves the loopback redirect until a callback with the right state arrives.
async fn wait_for_code(listener: tokio::net::TcpListener, state: String) -> Result<String> {
    let state = std::sync::Arc::new(state);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String>>(1);
    let mut conns = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (sock, _) = accepted?;
                conns.spawn(handle_callback(sock, state.clone(), tx.clone()));
            }
            Some(result) = rx.recv() => return result,
        }
    }
}

async fn handle_callback(
    mut sock: tokio::net::TcpStream,
    state: std::sync::Arc<String>,
    tx: tokio::sync::mpsc::Sender<Result<String>>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 64 * 1024 {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let target = req.split_whitespace().nth(1).unwrap_or("/");
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return respond(&mut sock, 400, "Bad request.").await;
    };
    if url.path() != CALLBACK_PATH {
        return respond(&mut sock, 404, "Callback route not found.").await;
    }
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if let Some(err) = q.get("error") {
        respond(&mut sock, 400, "ChatGPT was not connected. Return to the terminal.").await;
        let desc = q.get("error_description").map(String::as_str).unwrap_or_default();
        let _ = tx.send(Err(anyhow::anyhow!("sign-in failed: {err} {desc}"))).await;
        return;
    }
    match (q.get("code").filter(|c| !c.is_empty()), q.get("state")) {
        (Some(code), Some(s)) if s == state.as_str() => {
            respond(&mut sock, 200, "ChatGPT connected to August. You can close this window.").await;
            let _ = tx.send(Ok(code.clone())).await;
        }
        (None, _) => respond(&mut sock, 400, "Missing authorization code.").await,
        _ => respond(&mut sock, 400, "OAuth state mismatch.").await,
    }
}

async fn refresh(http: &reqwest::Client, p: &mut CodexProfile) -> Result<()> {
    let res = token_request_to(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", &p.refresh_token),
        ],
    )
    .await;
    let tok = match res {
        Ok(t) => t,
        Err(e) => {
            if let Some(te) = e.downcast_ref::<TokenError>()
                && matches!(te.code.as_str(), "invalid_grant" | "refresh_token_expired" | "refresh_token_reused")
            {
                logout()?;
                bail!("ChatGPT session expired, run `cargo run -- login` ({e})");
            }
            return Err(e);
        }
    };
    *p = profile_from(tok, Some(p))?;
    let mut file = CodexFile::load()?;
    file.profile = Some(p.clone());
    file.save()
}

pub struct Session {
    http: reqwest::Client,
    profile: Mutex<CodexProfile>,
    account_id: String,
}

fn headers_for(account_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("chatgpt-account-id", account_id.to_string()),
        ("originator", ORIGINATOR.to_string()),
        ("OpenAI-Beta", "responses=experimental".to_string()),
    ]
}

#[async_trait]
impl TokenSource for Session {
    async fn token(&self) -> Result<String> {
        let mut p = self.profile.lock().await;
        if p.expires_at <= now() + EXPIRY_MARGIN_SECS {
            refresh(&self.http, &mut p).await?;
        }
        Ok(p.access_token.clone())
    }

    async fn on_unauthorized(&self) -> Result<bool> {
        let mut p = self.profile.lock().await;
        refresh(&self.http, &mut p).await?;
        Ok(true)
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        headers_for(&self.account_id)
    }
}

fn load_profile() -> Result<CodexProfile> {
    CodexFile::load()?
        .profile
        .context("not signed in to ChatGPT, run `cargo run -- login`")
}

/// Best effort: the Codex backend's model list, if it serves one.
pub async fn list_models(http: &reqwest::Client) -> Result<Vec<String>> {
    let mut p = load_profile()?;
    if p.expires_at <= now() + EXPIRY_MARGIN_SECS {
        refresh(http, &mut p).await?;
    }
    let mut req = http
        .get(format!("{API_BASE}/models?client_version=1.0.0"))
        .bearer_auth(&p.access_token);
    for (k, v) in headers_for(&p.account_id) {
        req = req.header(k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {v}");
    }
    Ok(v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|s| s == "list"))
        .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()).map(String::from))
        .collect())
}

pub type Codex = Responses<Session>;

pub async fn provider(model: String, effort: String) -> Result<Codex> {
    let profile = load_profile()?;
    let account_id = profile.account_id.clone();
    let session = Session {
        http: crate::util::http_client(),
        profile: Mutex::new(profile),
        account_id,
    };
    Ok(Responses::new(API_BASE, session, model, Some(effort)))
}
