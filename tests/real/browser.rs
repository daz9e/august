use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

const PAGE: &str = r#"<!doctype html><title>Vault</title>
<button onclick="document.getElementById('out').textContent = 'Code: ' + (7000 + 391)">Reveal</button>
<p id="out"></p>"#;

/// The configured model opens a local page in a real headless Chrome (`agent-browser`),
/// clicks a button and reads text that only JavaScript puts on the page.
#[tokio::test]
#[ignore]
async fn model_clicks_through_a_page_in_a_real_browser() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(|| async { axum::response::Html(PAGE) });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let real_home = std::env::var("HOME").unwrap();
    let august = std::env::var("AUGUST_HOME").unwrap_or_else(|_| format!("{real_home}/.august"));
    let ask = format!("Open {url} in the browser, press the Reveal button and tell me the code it shows.");
    let fake = Fake::start(vec![message(1, json!({"text": ask}))], HashMap::new(), None).await;
    // agent-browser keeps its Chrome under the real home.
    let _gw = spawn_gateway_env(&fake, LlmSetup::Real { home: august.as_ref() }, &[], &[], &[("HOME", &real_home)]);
    fake.wait_for(Duration::from_secs(240), |f| f.sent_texts().iter().any(|t| t.contains("7391"))).await;
    std::process::Command::new("agent-browser").args(["--session", &format!("august-telegram-{CHAT}"), "close"]).output().ok();
}
