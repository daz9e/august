//! The default `browser` extension drives `agent-browser` (faked here by a shell script): one session
//! per chat, file paths kept inside the workspace, risky flags and commands refused.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test]
async fn browser_commands_run_in_a_per_chat_session() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let bin = dir.path().join("agent-browser");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$3\" in\n  snapshot) echo '- button \"Reveal\" [ref=e1]' ;;\n  click) echo 'boom' >&2; exit 1 ;;\nesac\n",
        log.display()
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let steps: Vec<Value> = vec![
        json!(["open", "https://example.com"]),
        json!(["snapshot", "-i"]),
        json!(["click", "@e1"]),
        json!(["screenshot", "shots/page.png"]),
        json!(["screenshot", "../../outside.png"]),
        json!(["open", "https://example.com", "--profile", "/tmp/p"]),
        json!(["connect", "9222"]),
    ];
    let llm: Llm = Box::new(move |req| {
        let msgs = req["messages"].as_array().unwrap();
        let done = msgs.iter().filter(|m| m["role"] == "tool").count();
        match steps.get(done) {
            Some(args) => reply_tool("browser", json!({"args": args})),
            None => reply_text("Browsed."),
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "look at example.com"}))], HashMap::new(), Some(llm)).await;
    let gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_BROWSER_BIN", bin.to_str().unwrap())]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Browsed."))).await;

    // Only the allowed calls reached the browser, all in this chat's session, with the
    // screenshot path made absolute inside the workspace.
    let calls = std::fs::read_to_string(&log).unwrap();
    let shot = gw.workspace.canonicalize().unwrap().join("shots/page.png");
    let session = format!("--session august-telegram-{CHAT}");
    let expected = [
        format!("{session} open https://example.com"),
        format!("{session} snapshot -i"),
        format!("{session} click @e1"),
        format!("{session} screenshot {}", shot.display()),
    ];
    assert_eq!(calls.lines().collect::<Vec<_>>(), expected, "{calls}");

    // The model saw the output, the browser's error, and why the rest were refused.
    let reqs = fake.llm_requests();
    let results: Vec<String> = reqs.last().unwrap()["messages"].as_array().unwrap().iter()
        .filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect();
    assert_eq!(results[0], "ok");
    assert!(results[1].contains("[ref=e1]"), "{results:?}");
    assert_eq!(results[2], "error: boom");
    assert!(results[4].contains("outside the workspace"), "{results:?}");
    assert!(results[5].contains("--profile is not allowed"), "{results:?}");
    assert!(results[6].contains("unsupported browser command `connect`"), "{results:?}");
}
