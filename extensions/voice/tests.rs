//! Voice notes and audio files, against a fake core and a fake transcription service: their
//! transcripts reach the agent, and when there is none it is told why.

#[path = "../fake_http.rs"]
mod fake_http;

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use fake_http::{FakeHttp, Resp};
use serde_json::{Value, json};
use std::path::Path;

async fn voice(env: &[(&str, &str)]) -> FakeAugust {
    let (fake, august) = FakeAugust::new(env);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

/// A file the attachments hook saved: `inbox/name` in the workspace, as `files` lists it.
fn saved(workspace: &Path, name: &str, bytes: &[u8], mime: &str, voice: bool) -> Value {
    let path = workspace.join("inbox").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    json!({"path": path, "mime": mime, "voice": voice})
}

#[tokio::test]
async fn voice_notes_and_audio_are_transcribed_for_the_agent() {
    let stt = FakeHttp::start(|r| match r.text().contains("broken.ogg") {
        true => Resp::status(500, "decoder crashed"),
        false => Resp::json(json!({"text": " Buy milk. "})),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_str().unwrap();
    let fake = voice(&[
        ("AUGUST_WORKSPACE", workspace),
        ("AUGUST_TRANSCRIBE_URL", &format!("{}/v1/", stt.url)),
        ("AUGUST_TRANSCRIBE_API_KEY", "gsk-test"),
        ("AUGUST_TRANSCRIBE_MODEL", "whisper-large-v3-turbo"),
    ])
    .await;
    let files = json!([
        saved(dir.path(), "note.ogg", b"OggS voice", "audio/ogg", true),
        saved(dir.path(), "memo.mp3", b"ID3 memo", "audio/mpeg", false),
        saved(dir.path(), "notes.pdf", b"%PDF", "application/pdf", false),
    ]);
    let data = fake.event("message_in", json!({"text": "listen", "files": files}), ctx("1")).await.unwrap();
    assert_eq!(
        data["text"],
        "listen\n[Voice message transcript, inbox/note.ogg]\nBuy milk.\n[Audio transcript, inbox/memo.mp3]\nBuy milk."
    );
    let reqs = stt.to("/v1/audio/transcriptions");
    assert_eq!(reqs.len(), 2, "only the audio is sent");
    assert!(reqs.iter().all(|r| r.header("authorization") == "Bearer gsk-test" && r.text().contains("whisper-large-v3-turbo")));
    assert!(reqs.iter().any(|r| r.text().contains("OggS voice")));

    // A failed transcription is reported in its place.
    let files = json!([saved(dir.path(), "broken.ogg", b"OggS ?", "audio/ogg", true)]);
    let data = fake.event("message_in", json!({"text": "", "files": files}), ctx("1")).await.unwrap();
    let text = data["text"].as_str().unwrap();
    assert!(text.starts_with("[No transcript of inbox/broken.ogg: transcription failed: HTTP 500"), "{text}");
    assert!(text.contains("decoder crashed"), "{text}");
}

#[tokio::test]
async fn a_voice_note_without_transcription_is_still_saved() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let fake = voice(&[("AUGUST_WORKSPACE", dir.path().to_str().unwrap()), ("AUGUST_HOME", home.path().to_str().unwrap())]).await;
    let files = json!([saved(dir.path(), "20261010-120000-voice.ogg", b"OggS", "audio/ogg", true)]);
    let msg = json!({"text": "[Attached file saved to inbox/20261010-120000-voice.ogg (audio/ogg, 4 B)]", "files": files});
    let data = fake.event("message_in", msg, ctx("1")).await.unwrap();
    let text = data["text"].as_str().unwrap();
    assert!(text.starts_with("[Attached file saved to inbox/20261010-120000-voice.ogg"), "where it is stays: {text}");
    assert!(text.contains("\n[No transcript of inbox/20261010-120000-voice.ogg: transcription is not configured"), "{text}");

    // Nor is the OpenAI provider's key used when it talks to another service.
    let creds = json!({"openai": {"key": "sk-other", "base_url": "http://127.0.0.1:9/v1"}});
    std::fs::write(home.path().join("credentials.json"), creds.to_string()).unwrap();
    let data = fake.event("message_in", json!({"text": "", "files": files}), ctx("1")).await.unwrap();
    assert!(data["text"].as_str().unwrap().contains("transcription is not configured"), "{data}");

    // Messages without audio are left alone.
    let data = fake.event("message_in", json!({"text": "hi", "files": []}), ctx("1")).await.unwrap();
    assert_eq!(data["text"], "hi");
}
