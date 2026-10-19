//! Legacy ChatGPT sign-in: the Codex CLI's public OAuth client (PKCE over a loopback redirect)
//! and the `chatgpt.com/backend-api/codex` Responses endpoint, billed to the ChatGPT plan.

use super::*;
use crate::config;
use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const SCOPE: &str = "openid profile email offline_access";
const API: &str = "https://chatgpt.com/backend-api/codex";
const ORIGINATOR: &str = "august";
const CLAIM: &str = "https://api.openai.com/auth";
/// Refresh this long before expiry so a request never starts with a dying token.
const EXPIRY_MARGIN_SECS: u64 = 180;
const PROVIDER: &str = "chatgpt";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    /// Unix seconds.
    pub expires_at: u64,
}

fn auth_path() -> std::path::PathBuf {
    config::home().join("chatgpt-auth.json")
}

fn load() -> Result<Option<Profile>> {
    match std::fs::read_to_string(auth_path()) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The signed-in account's email, if any.
pub fn signed_in() -> Option<String> {
    load().ok().flatten().map(|p| p.email)
}

fn save(p: &Profile) -> Result<()> {
    config::write_private(&auth_path(), &serde_json::to_string_pretty(p)?)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("os rng");
    B64.encode(buf)
}

/// Claims of a JWT from the token endpoint (fetched over TLS, so the signature isn't checked).
fn claims(jwt: &str) -> Result<Value> {
    let payload = jwt.split('.').nth(1).context("malformed token")?;
    Ok(serde_json::from_slice(&B64.decode(payload.trim_end_matches('='))?)?)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = http.post(TOKEN_URL).header("accept", "application/json").form(form).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        let code = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from));
        if matches!(code.as_deref(), Some("invalid_grant" | "refresh_token_expired" | "refresh_token_reused")) {
            std::fs::remove_file(auth_path()).ok();
            bail!("ChatGPT session expired, run `august login`");
        }
        bail!("token endpoint: HTTP {status}: {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

fn profile_from(tok: TokenResponse, old: Option<&Profile>) -> Result<Profile> {
    let account_id = claims(&tok.access_token)?[CLAIM]["chatgpt_account_id"]
        .as_str()
        .context("access token has no chatgpt_account_id")?
        .to_string();
    let email = tok
        .id_token
        .as_deref()
        .and_then(|t| claims(t).ok())
        .and_then(|c| c["email"].as_str().map(String::from))
        .or_else(|| old.map(|p| p.email.clone()))
        .unwrap_or_default();
    Ok(Profile {
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

/// Browser sign-in. Saves and returns the profile.
pub async fn login(http: &reqwest::Client) -> Result<Profile> {
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
        .with_context(|| format!("port {CALLBACK_PORT} is busy (is another login running?)"))?;
    println!("Open this link to sign in with ChatGPT:\n\n{url}\n");
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();

    let q = tokio::time::timeout(Duration::from_secs(300), wait_callback(listener))
        .await
        .context("timed out waiting for the browser sign-in")??;
    if let Some(err) = q.get("error") {
        bail!("sign-in failed: {err} {}", q.get("error_description").map(String::as_str).unwrap_or_default());
    }
    anyhow::ensure!(q.get("state") == Some(&state), "OAuth state mismatch");
    let code = q.get("code").context("missing authorization code")?;
    let tok = token_request(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", &verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await?;
    let profile = profile_from(tok, None)?;
    save(&profile)?;
    Ok(profile)
}

/// Serves the loopback redirect until it carries the OAuth query.
async fn wait_callback(listener: tokio::net::TcpListener) -> Result<HashMap<String, String>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req.split_whitespace().nth(1).unwrap_or("/");
        let url = reqwest::Url::parse(&format!("http://localhost{target}"))?;
        if url.path() != "/auth/callback" {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await.ok();
            continue;
        }
        let page = "<html><body><h3>Signed in. You can close this tab.</h3></body></html>";
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}", page.len());
        sock.write_all(resp.as_bytes()).await.ok();
        return Ok(url.query_pairs().into_owned().collect());
    }
}

/// A valid access token and the account id, refreshing (and persisting) when needed.
async fn credentials(http: &reqwest::Client, slot: &tokio::sync::Mutex<Option<Profile>>, force: bool) -> Result<(String, String)> {
    let mut slot = slot.lock().await;
    if slot.is_none() {
        *slot = load()?;
    }
    let Some(p) = slot.as_mut() else { bail!("not signed in to ChatGPT, run `august login`") };
    if force || p.expires_at <= now() + EXPIRY_MARGIN_SECS {
        let tok = token_request(
            http,
            &[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", &p.refresh_token)],
        )
        .await;
        let tok = match tok {
            Ok(t) => t,
            Err(e) => {
                *slot = None;
                return Err(e);
            }
        };
        *p = profile_from(tok, Some(p))?;
        save(p)?;
    }
    Ok((p.access_token.clone(), p.account_id.clone()))
}

fn request(http: &reqwest::Client, method: reqwest::Method, path: &str, token: &str, account: &str) -> reqwest::RequestBuilder {
    http.request(method, format!("{API}{path}"))
        .bearer_auth(token)
        .header("chatgpt-account-id", account)
        .header("originator", ORIGINATOR)
        .header("OpenAI-Beta", "responses=experimental")
}

fn encode(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        for b in &m.content {
            match (m.role, b) {
                (Role::User, Block::Text { text }) => out.push(json!({"role": "user", "content": text})),
                (Role::Assistant, Block::Text { text }) => out.push(json!({
                    "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}],
                })),
                (_, Block::ToolUse { id, name, input }) => out.push(json!({
                    "type": "function_call", "call_id": id, "name": name, "arguments": input.to_string(),
                })),
                (_, Block::ToolResult { tool_use_id, content, .. }) => out.push(json!({
                    "type": "function_call_output", "call_id": tool_use_id, "output": content,
                })),
                (Role::User, Block::Image { media_type, path }) => {
                    let part = match image_base64(path) {
                        Some(data) => json!({"type": "input_image", "image_url": format!("data:{media_type};base64,{data}")}),
                        None => json!({"type": "input_text", "text": missing_image(path)}),
                    };
                    out.push(json!({"role": "user", "content": [part]}));
                }
                // Reasoning items (encrypted) carry the chain of thought across requests;
                // web search calls are kept only for display.
                (_, Block::Opaque { provider, value }) if provider == PROVIDER && value["type"] == "reasoning" => {
                    out.push(value.clone())
                }
                _ => {}
            }
        }
    }
    out
}

fn decode(output: &[Value]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for c in item["content"].as_array().into_iter().flatten() {
                    let text = c["text"].as_str().or(c["refusal"].as_str());
                    if let Some(text) = text {
                        blocks.push(Block::Text { text: text.into() });
                    }
                }
            }
            Some("function_call") => {
                let args = item["arguments"].as_str().unwrap_or("{}");
                blocks.push(Block::ToolUse {
                    id: item["call_id"].as_str().unwrap_or_default().into(),
                    name: item["name"].as_str().unwrap_or_default().into(),
                    input: serde_json::from_str(args).unwrap_or_else(|_| json!({ "_raw": args })),
                });
            }
            Some("reasoning") | Some("web_search_call") => blocks.push(Block::Opaque { provider: PROVIDER.into(), value: item.clone() }),
            _ => {}
        }
    }
    blocks
}

fn error_message(code: Option<&str>, detail: String) -> String {
    match code {
        Some("usage_limit_reached") | Some("subscription_sharing_usage_limit_exceeded") => {
            format!("ChatGPT plan usage limit reached ({detail})")
        }
        _ => detail,
    }
}

pub async fn complete(
    http: &reqwest::Client,
    slot: &tokio::sync::Mutex<Option<Profile>>,
    cfg: &Config,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    on_text: OnText<'_>,
) -> Result<Completion> {
    let mut tools: Vec<Value> = tools
        .iter()
        .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false}))
        .collect();
    if cfg.web_search {
        tools.push(json!({"type": "web_search", "external_web_access": true}));
    }
    let body = json!({
        "model": cfg.model,
        "instructions": system,
        "input": encode(messages),
        "tools": tools,
        "stream": true,
        "store": false,
        "reasoning": {"effort": cfg.effort},
        "include": ["reasoning.encrypted_content"],
    });

    let mut force = false;
    let resp = loop {
        let (token, account) = credentials(http, slot, force).await?;
        match sse::post(request(http, reqwest::Method::POST, "/responses", &token, &account), &body).await {
            Ok(r) => break r,
            Err(e) if e.status == Some(401) && !force => force = true,
            Err(e) => {
                let code = serde_json::from_str::<Value>(&e.body).ok().and_then(|v| {
                    v["error"]["code"].as_str().or(v["error"]["type"].as_str()).map(String::from)
                });
                bail!(error_message(code.as_deref(), e.to_string()));
            }
        }
    };

    // The backend leaves `output` empty in the final event; items arrive as `output_item.done`.
    let mut items = Vec::<Value>::new();
    let mut done = Value::Null;
    sse::read(resp, |ev| {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return Ok(false) };
        match v["type"].as_str() {
            Some("response.output_text.delta") | Some("response.refusal.delta") => {
                if let Some(d) = v["delta"].as_str().filter(|d| !d.is_empty()) {
                    on_text(d);
                }
            }
            Some("response.output_item.done") => items.push(v["item"].clone()),
            Some("response.completed") | Some("response.incomplete") => {
                done = v["response"].clone();
                return Ok(true);
            }
            Some("response.failed") => {
                let err = &v["response"]["error"];
                bail!(error_message(err["code"].as_str(), err.to_string()));
            }
            Some("error") => bail!(error_message(v["code"].as_str(), v.to_string())),
            _ => {}
        }
        Ok(false)
    })
    .await?;

    let output = match done["output"].as_array() {
        Some(o) if !o.is_empty() => o.clone(),
        _ => items,
    };
    let content = decode(&output);
    let has_calls = content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
    let stop = match (done["status"].as_str(), done["incomplete_details"]["reason"].as_str()) {
        _ if has_calls => Stop::ToolUse,
        (Some("incomplete"), Some("max_output_tokens")) => Stop::MaxTokens,
        (Some("completed"), _) => Stop::EndTurn,
        (other, reason) => Stop::Other(reason.or(other).unwrap_or("unknown").into()),
    };
    let u = &done["usage"];
    // `input_tokens` includes the cached part.
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Ok(Completion {
        message: Message { role: Role::Assistant, content },
        stop,
        usage: Usage {
            input: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            cache_write: 0,
        },
    })
}

/// Model slugs the backend lists. Goes through the shared session (`slot`): refresh tokens
/// rotate, so a second copy would leave the other holding a dead one.
pub async fn models(http: &reqwest::Client, slot: &tokio::sync::Mutex<Option<Profile>>) -> Result<Vec<String>> {
    let (token, account) = credentials(http, slot, false).await?;
    let resp = request(http, reqwest::Method::GET, "/models?client_version=1.0.0", &token, &account).send().await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_output() {
        let out = json!([
            {"type":"reasoning","id":"r","encrypted_content":"x"},
            {"type":"message","content":[{"type":"output_text","text":"hi"}]},
            {"type":"function_call","call_id":"c","name":"sh","arguments":"{\"a\":1}"}
        ]);
        let b = decode(out.as_array().unwrap());
        assert!(matches!(&b[0], Block::Opaque { provider, .. } if provider == PROVIDER));
        assert!(matches!(&b[1], Block::Text { text } if text == "hi"));
        assert!(matches!(&b[2], Block::ToolUse { input, .. } if input["a"] == 1));
        // Reasoning round-trips; another provider's opaque blocks are dropped.
        let msgs = [Message { role: Role::Assistant, content: b }, Message {
            role: Role::Assistant,
            content: vec![Block::Opaque { provider: "anthropic".into(), value: json!({}) }],
        }];
        assert_eq!(encode(&msgs).len(), 3);
    }
}
