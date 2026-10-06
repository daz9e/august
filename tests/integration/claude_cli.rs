//! The `claude-cli` provider: the gateway drives a fake `claude` executable
//! (`AUGUST_CLAUDE_BIN`) that speaks stream-json, and August's own tools still run.

use crate::support::*;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Saves each call's stdin and args next to itself. Without a tool result in the
/// transcript it asks for `read_file` (split across deltas); with one, it answers.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
[ "$1" = "--version" ] && { echo "0.0.0 (fake)"; exit 0; }
input=$(cat)
i=$(ls "$dir" | grep -c '^stdin-')
printf '%s' "$input" > "$dir/stdin-$i.json"
printf '%s\n' "$@" > "$dir/args-$i.txt"
delta() { printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"%s"}}}\n' "$1"; }
echo '{"type":"system","subtype":"init"}'
if printf '%s' "$input" | grep -q '<tool_result>'; then
  delta 'The note says '
  delta 'PINEAPPLE.'
  echo '{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"The note says PINEAPPLE.","usage":{"input_tokens":20,"output_tokens":5}}'
else
  delta 'Let me look.<tool'
  delta '_call>{\"name\":\"read_file\",\"input\":{\"path\":\"note.txt\"}}</tool_call>'
  echo '{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"Let me look.<tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"note.txt\"}}</tool_call>","usage":{"input_tokens":10,"output_tokens":5}}'
fi
"#;

#[tokio::test]
async fn answers_through_the_claude_cli_and_runs_august_tools() {
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("claude");
    std::fs::write(&bin, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let update = message(1, serde_json::json!({"text": "what does note.txt say?"}));
    let fake = Fake::start(vec![update], HashMap::new(), None).await;
    let bin_path = bin.to_string_lossy().into_owned();
    let env = [("AUGUST_PROVIDER", "claude-cli"), ("AUGUST_MODEL", "sonnet"), ("AUGUST_CLAUDE_BIN", bin_path.as_str())];
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[("note.txt", b"PINEAPPLE")], &[], &env);

    fake.wait_for(TIMEOUT, |f| f.sent_texts().iter().any(|t| t.contains("The note says PINEAPPLE."))).await;

    // The tool-call markup was never shown in the chat, even mid-stream.
    let texts = fake.sent_texts();
    assert!(texts.iter().all(|t| !t.contains("tool_call") && !t.contains("<tool")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("Let me look.")), "{texts:?}");

    // Two CLI runs: tools and model went in as flags, August's tool result came back in.
    let read = |name: &str| std::fs::read_to_string(bin_dir.path().join(name)).unwrap();
    let args = read("args-0.txt");
    let args: Vec<&str> = args.lines().collect();
    let flag = |f: &str| args.iter().position(|a| *a == f).map(|i| args[i + 1]);
    assert_eq!(flag("--model"), Some("sonnet"));
    assert_eq!(flag("--tools"), Some(""));
    assert!(args.contains(&"--system-prompt") && read("args-0.txt").contains(r#"{"name":"read_file""#), "{args:?}");

    let first: Value = serde_json::from_str(&read("stdin-0.json")).unwrap();
    assert!(first["message"]["content"].to_string().contains("what does note.txt say?"));
    let second = read("stdin-1.json");
    assert!(second.contains("<tool_result>\\nPINEAPPLE"), "{second}");
    assert!(second.contains(r#"<tool_call>{\"name\":\"read_file\""#), "{second}");
}
