//! Inbound attachments: saved into `workspace/inbox`, images also shown to the model.
//! Extensions get the saved files with `message_in` (the default `voice` one transcribes audio).

use super::Gateway;
use crate::messengers::{Attachment, Messenger};
use crate::llm::{Block, IMAGE_TYPES, MAX_IMAGE_BYTES};
use crate::util::{human_size, mime_for};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(super) const INBOX: &str = "inbox";

impl Gateway {
    /// Downloads `files` into the inbox. Returns a note for the model (where each
    /// file is, or why it is missing), image blocks for the ones it can see, and the
    /// saved files for `message_in` (`{path, mime, voice}`).
    pub(super) async fn receive(&self, channel: &dyn Messenger, files: &[Attachment]) -> (String, Vec<Block>, Vec<Value>) {
        let mut notes = Vec::new();
        let mut images = Vec::new();
        let mut saved = Vec::new();
        for file in files {
            let name = file_name(file);
            match self.save(channel, file, &name).await {
                Ok((path, size)) => {
                    let mime = file.mime.clone().unwrap_or_else(|| mime_for(&name).to_string());
                    let rel = path.strip_prefix(&self.workspace).unwrap_or(&path).display().to_string();
                    notes.push(format!("[Attached file saved to {rel} ({mime}, {})]", human_size(size)));
                    // Voice notes carry no file name; audio files do.
                    let voice = mime.starts_with("audio/") && file.name.is_none();
                    saved.push(json!({"path": path, "mime": mime, "voice": voice}));
                    if IMAGE_TYPES.contains(&mime.as_str()) && size as usize <= MAX_IMAGE_BYTES {
                        images.push(Block::Image { media_type: mime, path: path.display().to_string() });
                    }
                }
                Err(e) => notes.push(format!("[Attached file {name} could not be received: {e:#}]")),
            }
        }
        (notes.join("\n"), images, saved)
    }

    async fn save(&self, channel: &dyn Messenger, file: &Attachment, name: &str) -> Result<(PathBuf, u64)> {
        let bytes = channel.download(file).await?;
        let dir = self.workspace.join(INBOX);
        tokio::fs::create_dir_all(&dir).await?;
        let path = free_path(&dir, &format!("{}-{name}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        tokio::fs::write(&path, &bytes).await?;
        Ok((path, bytes.len() as u64))
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
