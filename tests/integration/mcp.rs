//! MCP servers from `AUGUST_HOME/mcp.json`: a stdio server (a small Python script), a
//! streamable HTTP server and one that can't start. Skipped without python3.

use crate::support::*;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};


const SERVER_PY: &str = r#"
import json, os, sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    msg = json.loads(line)
    method, id = msg.get("method"), msg.get("id")
    if id is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [
            {"name": "echo", "description": "Echo text", "inputSchema": {"type": "object",
             "properties": {"text": {"type": "string"}}, "required": ["text"]}},
            {"name": "fail.hard", "description": "Always fails", "inputSchema": {"type": "object"}},
            {"name": "fail hard", "description": "Also fails", "inputSchema": {"type": "object"}},
            {"name": "crash", "description": "Kills the server", "inputSchema": {"type": "object"}},
        ]}
    elif method == "tools/call" and msg["params"]["name"] == "crash":
        sys.exit("crashed on purpose")
    elif method == "tools/call" and msg["params"]["name"] == "echo":
        text = os.environ["GREETING"] + " " + msg["params"]["arguments"]["text"]
        result = {"content": [{"type": "text", "text": text}, {"type": "image", "data": "", "mimeType": "image/png"}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "disk on fire"}], "isError": True}
    else:
        send({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}})
        continue
    send({"jsonrpc": "2.0", "id": id, "result": result})
"#;

fn have_python() -> bool {
    let found = std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipping: python3 is not installed");
    }
    found
}

/// A streamable HTTP MCP server: JSON for the handshake, SSE for tool calls, and a
/// session id it insists on after `initialize`. The first session ends before the first
/// tool call, as when the server restarts.
async fn http_server() -> String {
    type Inits = Arc<AtomicUsize>;
    async fn handle(State(inits): State<Inits>, headers: HeaderMap, body: axum::body::Bytes) -> axum::response::Response {
        let msg: Value = serde_json::from_slice(&body).unwrap();
        let session = headers.get("mcp-session-id").and_then(|v| v.to_str().ok());
        let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let method = msg["method"].as_str().unwrap();
        if method == "initialize" {
            let n = inits.fetch_add(1, Ordering::SeqCst) + 1;
            let r = reply(json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "web"}}));
            return ([("mcp-session-id", format!("s{n}"))], axum::Json(r)).into_response();
        }
        let current = format!("s{}", inits.load(Ordering::SeqCst));
        match session {
            None => return StatusCode::BAD_REQUEST.into_response(),
            Some(s) if s != current || (s == "s1" && method == "tools/call") => return StatusCode::NOT_FOUND.into_response(),
            _ => {}
        }
        if method == "tools/list" {
            return axum::Json(reply(json!({"tools": [{"name": "time", "inputSchema": {"type": "object"}}]}))).into_response();
        }
        let r = reply(json!({"content": [{"type": "text", "text": "noon over http"}]}));
        ([("content-type", "text/event-stream")], format!("event: message\ndata: {r}\n\n")).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(handle).with_state(Inits::default());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn messages(req: &Value) -> &Vec<Value> {
    req["messages"].as_array().unwrap()
}

#[tokio::test]
async fn mcp_server_tools_become_agent_tools() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(&script, SERVER_PY).unwrap();
    let config = json!({"servers": {
        "fake": {"command": "python3", "args": [script], "env": {"GREETING": "hello"}},
        "web": {"url": http_server().await},
        "ghost": {"command": "/nonexistent/mcp-server"},
        "dies": {"command": "python3", "args": ["-c", "import sys; sys.stdin.readline(); print('bye', file=sys.stderr)"]},
    }});

    let llm: Llm = Box::new(|req| {
        let last = messages(req).last().unwrap();
        if last["role"] == "tool" {
            return reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")));
        }
        let text = last["content"].as_str().unwrap_or("");
        if text.contains("echo") {
            reply_tool("mcp_fake_echo", json!({"text": "world"}))
        } else if text.contains("break") {
            reply_tool("mcp_fake_fail_hard", json!({}))
        } else {
            reply_tool("mcp_web_time", json!({}))
        }
    });
    let fake = Fake::llm(llm).await;
    let config = config.to_string();
    let gw = august(&fake, Setup { home: &[("mcp.json", &config)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // One message at a time, so each runs as its own turn.
    chat.ask("echo please", "Result: hello world").await;
    chat.ask("break it", "disk on fire").await;
    // The HTTP server ended its session first: August starts a new one and retries.
    chat.ask("what time", "Result: noon over http").await;
    let status = chat.ask("/mcp", "ghost").await;

    // The tools were offered under prefixed, sanitized names.
    let reqs = fake.llm_requests();
    let tools: Vec<&str> = reqs[0]["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    for name in ["mcp_fake_echo", "mcp_fake_fail_hard", "mcp_web_time", "shell"] {
        assert!(tools.contains(&name), "{tools:?}");
    }
    // Non-text content is noted; an MCP error result reaches the model as a tool error.
    let tool_msgs: Vec<String> = reqs.iter().flat_map(messages).filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(String::from)).collect();
    assert!(tool_msgs.iter().any(|m| m.contains("hello world") && m.contains("[image content (image/png) not shown]")), "{tool_msgs:?}");
    assert!(tool_msgs.iter().any(|m| m == "error: disk on fire"), "{tool_msgs:?}");

    // /mcp lists every server, including the one that could not start.
    // Names that clean up to the same one are told apart.
    assert!(status.contains("fake — tools: mcp_fake_echo, mcp_fake_fail_hard, mcp_fake_fail_hard_2, mcp_fake_crash"), "{status}");
    assert!(status.contains("web — tools: mcp_web_time"), "{status}");
    assert!(status.contains("could not run /nonexistent/mcp-server"), "{status}");
    assert!(status.contains("dies — the server exited: bye"), "{status}");
}

#[tokio::test]
async fn crashed_server_is_started_again() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(&script, SERVER_PY).unwrap();
    let config = json!({"servers": {"fake": {"command": "python3", "args": [script], "env": {"GREETING": "hello"}}}}).to_string();
    let llm: Llm = Box::new(|req| {
        let last = messages(req).last().unwrap();
        if last["role"] == "tool" {
            return reply_text(&format!("Result: {}", last["content"].as_str().unwrap_or("")));
        }
        let text = last["content"].as_str().unwrap_or("");
        if text.contains("crash") { reply_tool("mcp_fake_crash", json!({})) } else { reply_tool("mcp_fake_echo", json!({"text": "again"})) }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { home: &[("mcp.json", &config)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    let crashed = chat.ask("crash it", "Result:").await;
    assert!(crashed.contains("crashed on purpose") && crashed.contains("August restarts it"), "{crashed}");
    // Its tools come back once it runs again.
    let start = std::time::Instant::now();
    while !chat.ask("/mcp", "fake").await.contains("✅ fake") {
        assert!(start.elapsed() < TIMEOUT, "the server was not started again");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    chat.ask("echo please", "Result: hello again").await;
}
