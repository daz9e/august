//! Inbound attachments: saved into `workspace/inbox`, images also shown to the model,
//! voice notes and audio files transcribed.

use super::Gateway;
use crate::channels::{Attachment, Channel};
use crate::llm::{Block, IMAGE_TYPES, MAX_IMAGE_BYTES, providers};
use crate::util::{http_client, human_size, mime_for};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub(super) const INBOX: &str = "inbox";

impl Gateway {
    /// Downloads `files` into the inbox. Returns a note for the model (where each
    /// file is, or why it is missing) and image blocks for the ones it can see.
    pub(super) async fn receive(&self, channel: &dyn Channel, files: &[Attachment]) -> (String, Vec<Block>) {
        let mut notes = Vec::new();
        let mut images = Vec::new();
        for file in files {
            let name = file_name(file);
            match self.save(channel, file, &name).await {
                Ok((path, bytes)) => {
                    let size = bytes.len() as u64;
                    let mime = file.mime.clone().unwrap_or_else(|| mime_for(&name).to_string());
                    let rel = path.strip_prefix(&self.workspace).unwrap_or(&path).display().to_string();
                    notes.push(format!("[Attached file saved to {rel} ({mime}, {})]", human_size(size)));
                    if mime.starts_with("audio/") {
                        // Voice notes carry no file name; audio files do.
                        let kind = if file.name.is_none() { "Voice message" } else { "Audio" };
                        notes.push(match transcribe(bytes, &name, &mime).await {
                            Ok(text) => format!("[{kind} transcript]\n{text}"),
                            Err(e) => format!("[No transcript: {e:#}]"),
                        });
                    } else if IMAGE_TYPES.contains(&mime.as_str()) && size as usize <= MAX_IMAGE_BYTES {
                        images.push(Block::Image { media_type: mime, path: path.display().to_string() });
                    }
                }
                Err(e) => notes.push(format!("[Attached file {name} could not be received: {e:#}]")),
            }
        }
        (notes.join("\n"), images)
    }

    async fn save(&self, channel: &dyn Channel, file: &Attachment, name: &str) -> Result<(PathBuf, Vec<u8>)> {
        let bytes = channel.download(file).await?;
        let dir = self.workspace.join(INBOX);
        tokio::fs::create_dir_all(&dir).await?;
        let path = free_path(&dir, &format!("{}-{name}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        tokio::fs::write(&path, &bytes).await?;
        Ok((path, bytes))
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Speech to text via an OpenAI-compatible `POST {base}/audio/transcriptions`:
/// `AUGUST_TRANSCRIBE_URL` (e.g. Groq) with `AUGUST_TRANSCRIBE_API_KEY`, else OpenAI
/// with that key or the OpenAI provider's key (only when it points at OpenAI itself).
async fn transcribe(bytes: Vec<u8>, name: &str, mime: &str) -> Result<String> {
    let (base, key) = match env("AUGUST_TRANSCRIBE_URL") {
        Some(url) => (url, env("AUGUST_TRANSCRIBE_API_KEY").unwrap_or_default()),
        None => {
            let openai = || {
                let c = providers::credential(providers::info("openai").ok()?).ok()??;
                (c.base_url.as_deref() == Some(providers::OPENAI_DEFAULT_URL)).then_some(c.key)
            };
            let key = env("AUGUST_TRANSCRIBE_API_KEY").or_else(openai).filter(|k| !k.is_empty()).context(
                "transcription is not configured (set AUGUST_TRANSCRIBE_API_KEY or OPENAI_API_KEY, \
                 or AUGUST_TRANSCRIBE_URL for another OpenAI-compatible service)",
            )?;
            (providers::OPENAI_DEFAULT_URL.to_string(), key)
        }
    };
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()).mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model", env("AUGUST_TRANSCRIBE_MODEL").unwrap_or_else(|| "whisper-1".into()))
        .part("file", part);
    let mut req = http_client().post(format!("{}/audio/transcriptions", base.trim_end_matches('/'))).multipart(form);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let res = req.send().await.context("transcription request failed")?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        bail!("transcription failed: HTTP {status}: {}", body.chars().take(300).collect::<String>());
    }
    let json: serde_json::Value = serde_json::from_str(&body).context("transcription: bad response")?;
    match json["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => bail!("transcription returned no text"),
    }
}

/// A safe file name: the original one, or `photo.jpg` / `file.<ext>` from the type.
fn file_name(file: &Attachment) -> String {
    let name = file.name.as_deref().map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n)).unwrap_or("");
    let clean: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let clean = clean.trim_start_matches('.');
    if !clean.is_empty() {
        return clean.to_string();
    }
    match file.mime.as_deref().unwrap_or("") {
        "image/jpeg" => "photo.jpg".into(),
        "image/png" => "image.png".into(),
        "image/webp" => "image.webp".into(),
        "audio/ogg" => "voice.ogg".into(),
        "video/mp4" => "video.mp4".into(),
        _ => "file".into(),
    }
}

/// `dir/name`, or `dir/stem-2.ext`, ... if that is taken.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (name, String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem}-{n}{ext}"))).find(|p| !p.exists()).unwrap()
}
