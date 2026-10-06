use crate::support::*;
use std::collections::HashMap;
use std::time::Duration;

/// The real `claude` CLI (installed and signed in) answers in one line, calling
/// August's `read_file` tool through the text protocol on the way.
#[tokio::test]
#[ignore]
async fn claude_cli_answers_using_an_august_tool() {
    let update = message(1, serde_json::json!({
        "text": "Use the read_file tool on note.txt, then reply with only the word it contains."
    }));
    let fake = Fake::start(vec![update], HashMap::new(), None).await;
    // The CLI finds its login through the real user's HOME and USER.
    let (home, user) = (std::env::var("HOME").unwrap(), std::env::var("USER").unwrap_or_default());
    let env = [("AUGUST_PROVIDER", "claude-cli"), ("AUGUST_MODEL", "haiku"), ("HOME", &home), ("USER", &user)];
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[("note.txt", b"PINEAPPLE")], &[], &env);

    fake.wait_for(Duration::from_secs(120), |f| f.sent_texts().iter().any(|t| t.contains("PINEAPPLE"))).await;
    let texts = fake.sent_texts();
    assert!(texts.iter().all(|t| !t.contains("tool_call&gt;")), "{texts:?}");
}
