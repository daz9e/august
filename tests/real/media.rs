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
