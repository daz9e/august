use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// The configured model actually sees an image sent to the bot.
/// Uses the real `~/.august` (credentials, config and the conversation DB).
#[tokio::test]
#[ignore]
async fn model_sees_an_image() {
    let png = fixture("red.png");
    let update = message(
        1,
        json!({
            "caption": "What single colour fills this image? Answer with one word.",
            "document": {"file_id": "img", "file_name": "square.png", "mime_type": "image/png", "file_size": png.len()},
        }),
    );
    let fake = Fake::start(vec![update], HashMap::from([("img".to_string(), png)]), None).await;
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".august");
    let home = std::env::var("AUGUST_HOME").map(Into::into).unwrap_or(home);
    let _gw = spawn_gateway(&fake, LlmSetup::Real { home: &home }, &[]);

    let said_red = |f: &Fake| {
        f.calls("sendMessage").iter().chain(f.calls("editMessageText").iter()).any(|r| {
            r.json()["text"].as_str().is_some_and(|t| t.to_lowercase().contains("red"))
        })
    };
    fake.wait_for(Duration::from_secs(120), said_red).await;
}

/// A voice note is transcribed by the configured speech-to-text service
/// (`AUGUST_TRANSCRIBE_*` or an OpenAI key) and the model acts on what was said.
#[tokio::test]
#[ignore]
async fn voice_note_is_transcribed() {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".august");
    let home = std::env::var("AUGUST_HOME").map(Into::into).unwrap_or(home);
    let configured = ["AUGUST_TRANSCRIBE_URL", "AUGUST_TRANSCRIBE_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))
        || std::fs::read_to_string(home.join("credentials.json")).is_ok_and(|c| c.contains("\"openai\""));
    if !configured {
        eprintln!("skipped: no transcription configured (AUGUST_TRANSCRIBE_URL / AUGUST_TRANSCRIBE_API_KEY / OPENAI_API_KEY)");
        return;
    }
    // Says: "Please reply with just the word banana."
    let ogg = fixture("voice.ogg");
    let update = message(
        1,
        json!({"voice": {"file_id": "v1", "duration": 2, "mime_type": "audio/ogg", "file_size": ogg.len()}}),
    );
    let fake = Fake::start(vec![update], HashMap::from([("v1".to_string(), ogg)]), None).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Real { home: &home }, &[]);

    let said_banana = |f: &Fake| {
        f.calls("sendMessage").iter().chain(f.calls("editMessageText").iter()).any(|r| {
            r.json()["text"].as_str().is_some_and(|t| t.to_lowercase().contains("banana"))
        })
    };
    fake.wait_for(Duration::from_secs(120), said_banana).await;
}
