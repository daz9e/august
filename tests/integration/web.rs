//! The default `web` extension: `web_fetch` turns a page into text, `web_search` uses the
//! Brave API with a key and scrapes DuckDuckGo without one (both faked locally).

use crate::support::*;
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const PAGE: &str = "<html><head><title>x</title></head><body><script>var a=1;</script><h1>Tea &amp; cake</h1><p>one<br>two</p></body></html>";
const DDG: &str = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=zz">Example <b>Site</b></a>
    <a class="result__snippet" href="x">A <b>great</b> site.</a>"#;

async fn web() -> String {
    let app = axum::Router::new()
        .route("/page", get(|| async { axum::response::Html(PAGE) }))
        .route("/brave", get(|q: axum::extract::Query<HashMap<String, String>>, h: axum::http::HeaderMap| async move {
            assert_eq!(h["x-subscription-token"], "k");
            axum::Json(json!({"web": {"results": [{"title": format!("About <strong>{}</strong>", q["q"]), "url": "https://tea.example", "description": "All about tea"}]}}))
        }))
        .route("/ddg", post(|| async { axum::response::Html(DDG) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// Runs `web_fetch` of the page, then `web_search`, and returns the tool results the model saw.
async fn tool_results(base: &str, env: &[(&str, &str)]) -> Vec<String> {
    let page = format!("{base}/page");
    let llm: Llm = Box::new(move |req| {
        let done = req["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count();
        match done {
            0 => reply_tool("web_fetch", json!({"url": page})),
            1 => reply_tool("web_search", json!({"query": "tea"})),
            _ => reply_text("Searched."),
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "find tea"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], env);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Searched."))).await;
    let reqs = fake.llm_requests();
    let msgs: &Vec<Value> = reqs.last().unwrap()["messages"].as_array().unwrap();
    msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn web_tools_fetch_pages_and_search() {
    let base = web().await;
    let (brave, ddg) = (format!("{base}/brave"), format!("{base}/ddg"));

    let with_key = tool_results(&base, &[("BRAVE_API_KEY", "k"), ("BRAVE_API_URL", &brave)]).await;
    assert_eq!(with_key[0], "Tea & cake\n\none\ntwo");
    assert_eq!(with_key[1], "1. About tea\n   https://tea.example\n   All about tea");

    let keyless = tool_results(&base, &[("DUCKDUCKGO_URL", &ddg)]).await;
    assert_eq!(keyless[1], "1. Example Site\n   https://example.com/a?b=1\n   A great site.");
}
