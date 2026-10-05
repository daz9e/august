//! `august serve` / `august stop` against the real launchd, under a test label so
//! the developer's own service is untouched. The gateway it runs talks to the fake
//! Bot API and LLM through a `.env` in its working directory.

use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn august(dir: &Path, cmd: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_august")).arg(cmd).current_dir(dir).output().unwrap()
}

fn loaded(label: &str) -> bool {
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let target = format!("gui/{}/{label}", uid.trim());
    Command::new("launchctl").args(["print", &target]).output().unwrap().status.success()
}

/// Removes the test service even if an assertion fails.
struct Installed<'a>(&'a Path);

impl Drop for Installed<'_> {
    fn drop(&mut self) {
        august(self.0, "stop");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore]
async fn serve_installs_restarts_and_stops_the_service() {
    let update = message(1, json!({"text": "ping"}));
    // The first turn leaves a long-running shell command behind, as a real turn
    // (a screenshot, a build) may be doing when the service is reloaded.
    let llm: Llm = Box::new(|req| {
        let last = req["messages"].as_array().unwrap().last().unwrap().clone();
        if last["role"] == "user" {
            reply_tool("shell", json!({"command": "tail -f /dev/null"}))
        } else {
            reply_text("pong")
        }
    });
    let fake = Fake::start(vec![update], HashMap::new(), Some(llm)).await;

    let dir = tempfile::tempdir().unwrap();
    let label = format!("dev.august.test-{}", std::process::id());
    let env = [
        ("AUGUST_SERVICE_LABEL", label.clone()),
        ("AUGUST_HOME", dir.path().join("home").display().to_string()),
        ("AUGUST_WORKSPACE", dir.path().join("workspace").display().to_string()),
        ("AUGUST_PROVIDER", "openai".into()),
        ("AUGUST_MODEL", "fake-model".into()),
        ("OPENAI_API_KEY", "test".into()),
        ("OPENAI_BASE_URL", format!("{}/v1", fake.url)),
        ("TELEGRAM_API_BASE", fake.url.clone()),
        ("TELEGRAM_BOT_TOKEN", TOKEN.into()),
        ("TELEGRAM_ALLOWED_USERS", OWNER.to_string()),
    ];
    let dotenv: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(dir.path().join(".env"), dotenv).unwrap();

    // First install: the service starts and works on a message.
    let out = august(dir.path(), "serve");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let _installed = Installed(dir.path());
    let timeout = Duration::from_secs(20);
    fake.wait_for(timeout, |f| f.llm_requests().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await; // the shell command is running

    // Running `serve` again reloads the busy service and a new gateway connects.
    let connects = fake.calls("getMe").len();
    let out = august(dir.path(), "serve");
    assert!(out.status.success(), "reload failed: {}", String::from_utf8_lossy(&out.stderr));
    fake.wait_for(timeout, |f| f.calls("getMe").len() > connects).await;
    assert!(loaded(&label));

    let out = august(dir.path(), "stop");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!loaded(&label));
}
