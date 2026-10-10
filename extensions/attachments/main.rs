//! Files sent with a message: saved into `workspace/inbox`, the model told where each one is
//! (or why it is missing), images it can see shown to it. Hooks after this one (`voice`)
//! get the saved files as `files: [{path, mime, kind, voice}]`.

use august_ext::August;
use august_ext::llm::{IMAGE_TYPES, MAX_IMAGE_BYTES};
use serde_json::{Value, json};
use std::path::Path;

const INBOX: &str = "inbox";

/// Media type guessed from a file name's extension.
fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// `1.2 MB`, `340 KB`, `12 B`.
fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} B"),
    }
}

/// A safe file name: the original one, or `photo.jpg` / `file.<ext>` from the type.
fn file_name(file: &Value) -> String {
    let name = file["name"].as_str().map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n)).unwrap_or("");
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let clean = clean.trim_start_matches('.');
    if !clean.is_empty() {
        return clean.to_string();
    }
    match file["mime"].as_str().unwrap_or("") {
        "image/jpeg" => "photo.jpg".into(),
        "image/png" => "image.png".into(),
        "image/webp" => "image.webp".into(),
        "audio/ogg" => "voice.ogg".into(),
        "video/mp4" => "video.mp4".into(),
        _ => "file".into(),
    }
}

/// `inbox/name`, or `inbox/stem-2.ext`, ... if that is taken.
fn free_path(workspace: &Path, name: &str) -> String {
    let taken = |rel: &str| workspace.join(rel).exists();
    let first = format!("{INBOX}/{name}");
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (name, String::new()),
    };
    (2..).map(|n| format!("{INBOX}/{stem}-{n}{ext}")).find(|p| !taken(p)).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["messaging"]);
    august.describe("Attachments", "Saves files sent to August into workspace/inbox and shows images to the model.");
    let workspace = august.workspace().clone();
    august.on("message_in", move |data, ctx| {
        let workspace = workspace.clone();
        async move {
            let files = data["files"].as_array().cloned().unwrap_or_default();
            if files.is_empty() {
                return Ok(None);
            }
            let (mut notes, mut images, mut saved) = (Vec::new(), Vec::new(), Vec::new());
            for file in &files {
                let name = file_name(file);
                let rel = free_path(&workspace, &format!("{}-{name}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
                match ctx.call("download", json!({"file": file, "path": rel})).await {
                    Ok(done) => {
                        let size = done["size"].as_u64().unwrap_or(0);
                        let mime = file["mime"].as_str().unwrap_or_else(|| mime_for(&name)).to_string();
                        notes.push(format!("[Attached file saved to {rel} ({mime}, {})]", human_size(size)));
                        let path = done["path"].clone();
                        if IMAGE_TYPES.contains(&mime.as_str()) && size as usize <= MAX_IMAGE_BYTES {
                            images.push(json!({"path": path, "mime": mime}));
                        }
                        saved.push(json!({"path": path, "mime": mime, "kind": file["kind"], "voice": file["kind"] == "voice"}));
                    }
                    Err(e) => notes.push(format!("[Attached file {name} could not be received: {e:#}]")),
                }
            }
            let text = [data["text"].as_str().unwrap_or_default(), &notes.join("\n")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
            // A message with images starts its own turn: it can't join one mid-way.
            let steer = data["steer"] == true && images.is_empty();
            Ok(Some(json!({"text": text, "files": saved, "images": images, "steer": steer})))
        }
    });
    august.run().await;
}
