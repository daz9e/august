//! `browser` against a fake core and a fake `agent-browser` (a shell script that logs its
//! calls): one session per chat, file paths kept inside the workspace, risky flags and
//! commands refused.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::{ctx, turn_ctx};
use serde_json::json;
use std::path::PathBuf;

struct Browser {
    fake: FakeAugust,
    log: PathBuf,
    workspace: PathBuf,
    _dir: tempfile::TempDir,
}

impl Browser {
    async fn run(&self, args: serde_json::Value, ctx: serde_json::Value) -> String {
        match self.fake.tool("browser", json!({"args": args}), ctx).await {
            Ok(out) => out,
            Err(e) => format!("error: {e}"),
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(String::from).collect()
    }
}

async fn browser() -> Browser {
    let dir = tempfile::tempdir().unwrap();
    let (log, bin, workspace) = (dir.path().join("calls.log"), dir.path().join("agent-browser"), dir.path().join("workspace"));
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$3\" in\n  snapshot) echo '- button \"Reveal\" [ref=e1]' ;;\n  click) echo 'boom' >&2; exit 1 ;;\n  press) exit 3 ;;\nesac\n",
        log.display()
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (fake, august) = FakeAugust::new(&[("AUGUST_BROWSER_BIN", bin.to_str().unwrap()), ("AUGUST_WORKSPACE", workspace.to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    Browser { fake, log, workspace, _dir: dir }
}

#[tokio::test]
async fn browser_commands_run_in_a_per_chat_session() {
    let b = browser().await;
    assert_eq!(b.run(json!(["open", "https://example.com"]), ctx("1")).await, "ok");
    assert!(b.run(json!(["snapshot", "-i"]), ctx("1")).await.contains("[ref=e1]"));
    // The browser's errors reach the model, or that it failed without a word.
    assert_eq!(b.run(json!(["click", "@e1"]), ctx("1")).await, "error: boom");
    let press = b.run(json!(["press", "Enter"]), ctx("1")).await;
    assert!(press.contains("`press` failed (exit status: 3)"), "{press}");
    // Screenshots go inside the workspace, as absolute paths.
    b.run(json!(["screenshot", "shots/page.png"]), ctx("1")).await;
    // Another chat, and a sub-agent of this one, get browsers of their own.
    b.run(json!(["back"]), ctx("2")).await;
    b.run(json!(["back"]), turn_ctx("1", json!({"id": 7, "mode": "fresh"}))).await;

    let shot = b.workspace.join("shots/page.png");
    let expected = [
        "--session august-test-1 open https://example.com".to_string(),
        "--session august-test-1 snapshot -i".into(),
        "--session august-test-1 click @e1".into(),
        "--session august-test-1 press Enter".into(),
        format!("--session august-test-1 screenshot {}", shot.display()),
        "--session august-test-2 back".into(),
        "--session august-test-1-agent7 back".into(),
    ];
    assert_eq!(b.calls(), expected);
}

#[tokio::test]
async fn browser_refuses_what_would_escape_its_session() {
    let b = browser().await;
    let outside = b.run(json!(["screenshot", "../../outside.png"]), ctx("1")).await;
    assert!(outside.contains("outside the workspace"), "{outside}");
    let profile = b.run(json!(["open", "https://example.com", "--profile", "/tmp/p"]), ctx("1")).await;
    assert!(profile.contains("--profile is not allowed"), "{profile}");
    let connect = b.run(json!(["connect", "9222"]), ctx("1")).await;
    assert!(connect.contains("unsupported browser command `connect`"), "{connect}");
    assert!(b.calls().is_empty(), "{:?}", b.calls());
}
