//! Files sent with a message, against a fake core: each is downloaded into the workspace's
//! inbox, the model told where it is (or why it is missing) and shown the images it can see.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The attachments extension over `workspace`, whose messenger holds `files` (`id` -> bytes,
/// or `Err` if it can't hand that file over) and reports `size` for those it can.
async fn attachments(workspace: &Path, files: &[(&str, Result<&[u8], &str>)]) -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[("AUGUST_WORKSPACE", workspace.to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    let files: Vec<(String, Result<Vec<u8>, String>)> =
        files.iter().map(|(id, f)| (id.to_string(), f.map(<[u8]>::to_vec).map_err(String::from))).collect();
    let workspace = workspace.to_path_buf();
    fake.on("download", move |p| {
        let (_, file) = files.iter().find(|(id, _)| p["file"]["id"] == id.as_str()).expect("a known file");
        let bytes = file.clone().map_err(anyhow::Error::msg)?;
        let path = workspace.join(p["path"].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, &bytes)?;
        Ok(json!({"path": path, "size": bytes.len()}))
    });
    fake
}

fn text(data: &Value) -> &str {
    data["text"].as_str().unwrap()
}

/// The path a saved file got, read from `[Attached file saved to <path> (`.
fn saved_path(workspace: &Path, note: &str) -> PathBuf {
    let rel = note.split("[Attached file saved to ").nth(1).and_then(|r| r.split_once(" (")).unwrap().0;
    assert!(rel.starts_with("inbox/"), "{note}");
    workspace.join(rel)
}

#[tokio::test]
async fn a_photo_is_saved_to_the_inbox_and_shown_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let fake = attachments(dir.path(), &[("p1", Ok(b"\xff\xd8 a photo"))]).await;
    let msg = json!({"text": "what colour is this?", "steer": true, "files": [{"id": "p1", "kind": "photo", "mime": "image/jpeg"}]});
    let data = fake.event("message_in", msg, ctx("1")).await.unwrap();

    let (said, note) = text(&data).split_once('\n').unwrap();
    assert_eq!(said, "what colour is this?");
    assert!(note.ends_with("-photo.jpg (image/jpeg, 10 B)]"), "{note}");
    let path = saved_path(dir.path(), note);
    assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xd8 a photo");
    assert_eq!(data["images"], json!([{"path": path, "mime": "image/jpeg"}]));
    assert_eq!(data["files"], json!([{"path": path, "mime": "image/jpeg", "kind": "photo", "voice": false}]));
    assert_eq!(data["steer"], false, "a message with images starts its own turn");
}

#[tokio::test]
async fn a_document_is_saved_under_its_own_name_for_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let fake = attachments(dir.path(), &[("d1", Ok(b"%PDF one")), ("d2", Ok(b"%PDF two"))]).await;
    let doc = |id| json!({"id": id, "kind": "document", "name": "../Q3 notes.pdf"});
    let msg = json!({"text": "", "steer": true, "files": [doc("d1"), doc("d2")]});
    let data = fake.event("message_in", msg, ctx("1")).await.unwrap();

    let notes: Vec<&str> = text(&data).lines().collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes.iter().all(|n| n.contains("Q3_notes") && n.contains("(application/pdf, 8 B)]")), "{notes:?}");
    let (one, two) = (saved_path(dir.path(), notes[0]), saved_path(dir.path(), notes[1]));
    assert_eq!(std::fs::read(one).unwrap(), b"%PDF one");
    assert_eq!(std::fs::read(two).unwrap(), b"%PDF two", "a file of the same name doesn't overwrite the first");
    assert_eq!(data["images"], json!([]));
    assert_eq!(data["files"][1]["kind"], "document");
    assert_eq!(data["steer"], true, "without images it can join the running turn");
}

#[tokio::test]
async fn a_file_that_cannot_be_received_is_reported_to_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let fake = attachments(dir.path(), &[("z1", Err("file is too big (20 MB)")), ("p1", Ok(b"photo"))]).await;
    let files = json!([{"id": "z1", "name": "big.zip"}, {"id": "p1", "mime": "image/png"}]);
    let data = fake.event("message_in", json!({"text": "here", "files": files}), ctx("1")).await.unwrap();
    let lines: Vec<&str> = text(&data).lines().collect();
    assert_eq!(lines[..2], ["here", "[Attached file big.zip could not be received: file is too big (20 MB)]"]);
    assert!(lines[2].ends_with("-image.png (image/png, 5 B)]"), "the others still arrive: {lines:?}");
    assert_eq!(data["files"].as_array().unwrap().len(), 1);

    // An image too large for the model is saved but not shown.
    fake.on("download", |p| Ok(json!({"path": p["path"], "size": 6 << 20})));
    let photo = json!({"text": "", "files": [{"id": "p2", "mime": "image/jpeg"}]});
    let data = fake.event("message_in", photo, ctx("1")).await.unwrap();
    assert!(text(&data).contains("(image/jpeg, 6.0 MB)]"), "{data}");
    assert_eq!(data["images"], json!([]));

    // A message without files is left alone.
    let n = fake.calls("download").len();
    let data = fake.event("message_in", json!({"text": "hi"}), ctx("1")).await.unwrap();
    assert_eq!(data, json!({"text": "hi"}));
    assert_eq!(fake.calls("download").len(), n);
}
