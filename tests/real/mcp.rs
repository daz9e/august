use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// The configured model uses a tool of a real MCP server (`server-everything` via npx).
/// Needs network for npx; the provider config is copied into a temp home.
#[tokio::test]
#[ignore]
async fn model_calls_a_real_mcp_server() {
    let real = std::env::var("AUGUST_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".august"));
    let home = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&real).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && name != "channels.json" && name != "mcp.json" {
            std::fs::copy(entry.path(), home.path().join(&name)).unwrap();
        }
    }
    let config = json!({"servers": {"everything": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"]}}});
    std::fs::write(home.path().join("mcp.json"), config.to_string()).unwrap();

    let ask = "Use the get-sum tool of the `everything` MCP server to add 1234 and 4321. Reply with just the result.";
    let fake = Fake::start(vec![message(1, json!({"text": ask}))], HashMap::new(), None).await;
    let _gw = spawn_gateway(&fake, LlmSetup::Real { home: home.path() }, &[]);
    fake.wait_for(Duration::from_secs(180), |f| f.sent_texts().iter().any(|t| t.contains("5555"))).await;
}
