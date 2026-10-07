//! The default `web` extension: `web_fetch` turns a page into text, `web_search` uses the
//! Brave API with a key and scrapes DuckDuckGo without one (both faked locally).

use crate::support::*;
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;

const PAGE: &str = r##"<html><head><title>x</title></head><body><!-- <p>old</p> --><script>var a=1;</script>
<h1>Tea &amp; cake</h1><p>one<br>two &mdash; see <a href="/recipes?a=1&amp;b=2">recipes</a> or <a href="#top">top</a></p>
<ul><li>green</li><li>black</li></ul>
<table><tr><th>Tea</th><th>Price</th></tr><tr><td>Green</td><td>3</td></tr></table></body></html>"##;
const DDG: &str = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=zz">Example <b>Site</b></a>
    <a class="result__snippet" href="x">A <b>great</b> site.</a>"#;

async fn web() -> String {
    let app = axum::Router::new()
        .route("/page", get(|| async { axum::response::Html(PAGE) }))
        .route("/app", get(|| async { axum::response::Html("<html><body><div id=root></div><script>render()</script></body></html>") }))
        .route("/report.pdf", get(|| async { ([("content-type", "application/pdf")], "%PDF-1.4") }))
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

/// Runs the tool calls `steps` in one turn and returns the tool results the model saw.
async fn tool_results(env: &[(&str, &str)], steps: Vec<(&'static str, Value)>) -> Vec<String> {
    let llm: Llm = Box::new(move |req| {
        let done = req["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count();
        match steps.get(done) {
            Some((tool, args)) => reply_tool(tool, args.clone()),
            None => reply_text("Searched."),
        }
    });
    let fake = Fake::llm(llm).await;
    let gw = august(&fake, Setup { env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.say("find tea").await;
    chat.wait_for("Searched.").await;
    let reqs = fake.llm_requests();
    let msgs: &Vec<Value> = reqs.last().unwrap()["messages"].as_array().unwrap();
    msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn web_tools_fetch_pages_and_search() {
    let base = web().await;
    let (brave, ddg) = (format!("{base}/brave"), format!("{base}/ddg"));
    let steps = || vec![("web_fetch", json!({"url": format!("{base}/page")})), ("web_search", json!({"query": "tea"}))];

    let with_key = tool_results(&[("BRAVE_API_KEY", "k"), ("BRAVE_API_URL", &brave)], steps()).await;
    // Structure survives as light Markdown, links as absolute URLs to follow.
    let page = format!("# Tea & cake\n\none\ntwo — see [recipes]({base}/recipes?a=1&b=2) or top\n\n- green\n- black\n\n| Tea | Price\n| Green | 3");
    assert_eq!(with_key[0], page);
    assert_eq!(with_key[1], "1. About tea\n   https://tea.example\n   All about tea");

    let keyless = tool_results(&[("DUCKDUCKGO_URL", &ddg)], steps()).await;
    assert_eq!(keyless[1], "1. Example Site\n   https://example.com/a?b=1\n   A great site.");
}

#[tokio::test]
async fn web_fetch_says_what_to_do_instead() {
    let base = web().await;
    let fetch = |path: &str| ("web_fetch", json!({"url": format!("{base}{path}")}));
    let results = tool_results(&[], vec![fetch("/report.pdf"), fetch("/app"), fetch("/missing")]).await;
    assert!(results[0].contains("application/pdf") && results[0].contains("curl"), "{results:?}");
    assert!(results[1].contains("browser tool"), "{results:?}");
    assert!(results[2].contains("HTTP 404 Not Found"), "{results:?}");
}
