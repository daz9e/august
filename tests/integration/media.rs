//! Photos and files sent to the bot, and files the agent sends back.

use crate::support::*;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Text of the latest user message in a chat-completions request, and its image URLs.
fn last_user(req: &Value) -> (String, Vec<String>) {
    let msg = req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap();
    match &msg["content"] {
        Value::String(s) => (s.clone(), vec![]),
        Value::Array(parts) => (
            parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
            parts.iter().filter_map(|p| p["image_url"]["url"].as_str().map(String::from)).collect(),
        ),
        other => panic!("unexpected content: {other}"),
    }
}

fn after_tool(req: &Value) -> bool {
    req["messages"].as_array().unwrap().last().unwrap()["role"] == "tool"
}

#[tokio::test]
async fn photo_reaches_the_model_and_agent_sends_a_file_back() {
    let photo = fixture("red.png");
    let chart = b"\x89PNG fake chart bytes".to_vec();
    let update = message(
        1,
        json!({
            "caption": "what colour is this?",
            "photo": [
                {"file_id": "small", "width": 90, "height": 90, "file_size": 10},
                {"file_id": "big", "width": 800, "height": 800, "file_size": photo.len()},
            ],
        }),
    );
    let files = HashMap::from([("big".to_string(), photo.clone())]);
    let llm: Llm = Box::new(|req| {
        if after_tool(req) {
            reply_text("Done.")
        } else {
            reply_tool("send_file", json!({"path": "chart.png", "caption": "Here it is"}))
        }
    });
    let fake = Fake::start(vec![update], files, Some(llm)).await;
    let gw = spawn_gateway(&fake, LlmSetup::Fake, &[("chart.png", &chart)]);

    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("Done."))).await;

    // The largest photo size was downloaded into the inbox.
    let saved = inbox(&gw);
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(saved[0].to_string_lossy().ends_with("-photo.jpg"));
    assert_eq!(std::fs::read(&saved[0]).unwrap(), photo);
    assert!(fake.calls("getFile").iter().all(|r| r.json()["file_id"] == "big"));

    // The model got the caption, where the file is, and the image itself.
    let first = &fake.llm_requests()[0];
    let (text, images) = last_user(first);
    assert!(text.contains("what colour is this?"), "{text}");
    assert!(text.contains("inbox/") && text.contains("photo.jpg"), "{text}");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&photo);
    assert_eq!(images, vec![format!("data:image/jpeg;base64,{b64}")]);

    // send_file uploaded chart.png as a photo with its caption, before the final text.
    let upload = fake.calls("sendPhoto");
    assert_eq!(upload.len(), 1);
    let body = upload[0].body.clone();
    let body_text = String::from_utf8_lossy(&body);
    assert!(body_text.contains("name=\"photo\"; filename=\"chart.png\""));
    assert!(body_text.contains("Here it is"));
    assert!(body.windows(chart.len()).any(|w| w == chart.as_slice()));
    let order: Vec<String> = fake
        .requests()
        .iter()
        .filter(|r| r.method() == "sendPhoto" || r.text().contains("Done."))
        .map(|r| r.method().to_string())
        .collect();
    assert_eq!(order.first().map(String::as_str), Some("sendPhoto"), "{order:?}");
    // The tool result told the model it worked.
    let second = &fake.llm_requests()[1];
    let tool_msg = second["messages"].as_array().unwrap().last().unwrap();
    assert!(tool_msg["content"].as_str().unwrap().contains("sent chart.png"), "{tool_msg}");
}

#[tokio::test]
async fn document_is_saved_to_the_inbox_for_the_agent() {
    let pdf = b"%PDF-1.4 fake".to_vec();
    let update = message(
        1,
        json!({"document": {"file_id": "doc1", "file_name": "Q3 notes.pdf", "mime_type": "application/pdf", "file_size": pdf.len()}}),
    );
    let files = HashMap::from([("doc1".to_string(), pdf.clone())]);
    let llm: Llm = Box::new(|_| reply_text("Got it."));
    let fake = Fake::start(vec![update], files, Some(llm)).await;
    let gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);

    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("Got it."))).await;

    let saved = inbox(&gw);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let name = saved[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(name.ends_with("-Q3_notes.pdf"), "{name}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), pdf);

    let (text, images) = last_user(&fake.llm_requests()[0]);
    assert!(text.contains(&format!("inbox/{name}")) && text.contains("application/pdf"), "{text}");
    assert!(images.is_empty());
}

#[tokio::test]
async fn too_large_file_is_reported_to_the_agent() {
    let update = message(
        1,
        json!({"document": {"file_id": "huge", "file_name": "big.zip", "file_size": 30 * 1024 * 1024}}),
    );
    let llm: Llm = Box::new(|_| reply_text("Too big, sorry."));
    let fake = Fake::start(vec![update], HashMap::new(), Some(llm)).await;
    let gw = spawn_gateway(&fake, LlmSetup::Fake, &[]);

    fake.wait_for(TIMEOUT, |f| f.calls("sendMessage").iter().any(|r| r.text().contains("Too big"))).await;

    assert!(inbox(&gw).is_empty());
    assert!(fake.calls("getFile").is_empty(), "must not try to download");
    let (text, _) = last_user(&fake.llm_requests()[0]);
    assert!(text.contains("big.zip could not be received") && text.contains("20 MB"), "{text}");
}
