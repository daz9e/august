//! Voice notes and audio files are transcribed for the agent via an OpenAI-compatible
//! `POST {base}/audio/transcriptions`: `AUGUST_TRANSCRIBE_URL` (e.g. Groq) with
//! `AUGUST_TRANSCRIBE_API_KEY`, else OpenAI with that key or the OpenAI provider's key
//! (only when it points at OpenAI itself). `AUGUST_TRANSCRIBE_MODEL` defaults to whisper-1.

use anyhow::{Context, Result, bail};
use august_ext::August;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const OPENAI_URL: &str = "https://api.openai.com/v1";
const TIMEOUT: Duration = Duration::from_secs(100);

/// The OpenAI provider's key, if it talks to OpenAI itself.
fn openai_key(august: &August) -> Option<String> {
    let env = |k: &str| august.env(k);
    let home = env("AUGUST_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env("HOME").unwrap_or_default()).join(".august"));
    let stored: Value = std::fs::read_to_string(home.join("credentials.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let stored = &stored["openai"];
    let base = env("OPENAI_BASE_URL").or_else(|| stored["base_url"].as_str().map(String::from)).unwrap_or_else(|| OPENAI_URL.into());
    if base != OPENAI_URL {
        return None;
    }
    env("OPENAI_API_KEY").or_else(|| stored["key"].as_str().map(String::from))
}

async fn transcribe(august: &August, http: &reqwest::Client, path: &Path, mime: &str) -> Result<String> {
    let env = |k: &str| august.env(k);
    let (base, key) = match env("AUGUST_TRANSCRIBE_URL") {
        Some(url) => (url, env("AUGUST_TRANSCRIBE_API_KEY").unwrap_or_default()),
        None => {
            let key = env("AUGUST_TRANSCRIBE_API_KEY").or_else(|| openai_key(august)).filter(|k| !k.is_empty()).context(
                "transcription is not configured (set AUGUST_TRANSCRIBE_API_KEY or OPENAI_API_KEY, \
                 or AUGUST_TRANSCRIBE_URL for another OpenAI-compatible service)",
            )?;
            (OPENAI_URL.to_string(), key)
        }
    };
    let bytes = tokio::fs::read(path).await.with_context(|| format!("could not read {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "audio".into());
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name).mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model", env("AUGUST_TRANSCRIBE_MODEL").unwrap_or_else(|| "whisper-1".into()))
        .part("file", part);
    let mut req = http.post(format!("{}/audio/transcriptions", base.trim_end_matches('/'))).multipart(form);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let res = req.send().await.context("transcription request failed")?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        bail!("transcription failed: HTTP {status}: {}", body.chars().take(300).collect::<String>());
    }
    let json: Value = serde_json::from_str(&body).context("transcription: bad response")?;
    match json["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => bail!("transcription returned no text"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    let http = reqwest::Client::builder().timeout(TIMEOUT).build().expect("http client");
    let workspace = august.workspace().clone();
    let me = august.clone();
    august.on("message_in", move |data, _| {
        let (august, http, workspace) = (me.clone(), http.clone(), workspace.clone());
        async move {
            let audio: Vec<&Value> = data["files"].as_array().into_iter().flatten().filter(|f| f["mime"].as_str().is_some_and(|m| m.starts_with("audio/"))).collect();
            if audio.is_empty() {
                return Ok(None);
            }
            // All at once: the hook has two minutes for the whole message.
            let transcripts = audio.iter().map(|f| {
                let path = Path::new(f["path"].as_str().unwrap_or_default());
                let name = path.strip_prefix(&workspace).unwrap_or(path).display();
                let kind = if f["voice"] == true { "Voice message" } else { "Audio" };
                let (august, http) = (&august, &http);
                async move {
                    match transcribe(august, http, path, f["mime"].as_str().unwrap_or_default()).await {
                        Ok(text) => format!("[{kind} transcript, {name}]\n{text}"),
                        Err(e) => format!("[No transcript of {name}: {e:#}]"),
                    }
                }
            });
            let mut parts: Vec<String> = data["text"].as_str().filter(|t| !t.is_empty()).map(String::from).into_iter().collect();
            parts.extend(futures_util::future::join_all(transcripts).await);
            Ok(Some(json!({"text": parts.join("\n")})))
        }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
