use crate::support::*;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

/// The configured model writes a working extension from the built-in guide.
/// Copies the provider config from the real `~/.august` into a temp home, so the
/// extension is not installed for real.
#[tokio::test]
#[ignore]
async fn model_writes_a_working_extension() {
    let real = std::env::var("AUGUST_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".august"));
    let home = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&real).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && name != "channels.json" {
            std::fs::copy(entry.path(), home.path().join(&name)).unwrap();
        }
    }
    let ask = "Create and install an August extension named `dice` that adds a slash command /dice \
               which replies with the text `rolled N`, where N is a random integer from 1 to 6.";
    let fake = Fake::start(vec![message(1, json!({"text": ask}))], HashMap::new(), None).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Real { home: home.path() }, &[]);

    // Approve whatever the agent asks for, until the extension is installed.
    let pressed = Mutex::new(HashSet::new());
    let next_update = Mutex::new(100);
    let approve_all = |f: &Fake| {
        for req in f.calls("sendMessage") {
            let data = req.json()["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().map(String::from);
            if let Some(data) = data.filter(|d| !d.is_empty()) {
                if pressed.lock().unwrap().insert(data.clone()) {
                    let mut id = next_update.lock().unwrap();
                    *id += 1;
                    f.push_updates(vec![button_press(*id, &data)]);
                }
            }
        }
    };
    let installed = home.path().join("extensions/dice/index.ts");
    fake.wait_for(Duration::from_secs(300), |f| {
        approve_all(f);
        installed.exists() && f.sent_texts().iter().any(|t| t.contains("dice"))
    })
    .await;
    // Let the turn finish (the agent may fix and re-save).
    fake.wait_for(Duration::from_secs(300), |f| {
        approve_all(f);
        f.calls("sendChatAction").len() > 0 && f.sent_texts().len() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    fake.push_updates(vec![message(2, json!({"text": "/dice"}))]);
    fake.wait_for(Duration::from_secs(60), |f| {
        f.sent_texts().iter().any(|t| (1..=6).any(|n| t.contains(&format!("rolled {n}"))))
    })
    .await;
}
