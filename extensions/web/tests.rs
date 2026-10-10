//! `web_fetch` and `web_search` against a fake core and a fake web: pages come back as text,
//! searches go to Brave with a key and to DuckDuckGo without one.

#[path = "../fake_http.rs"]
mod fake_http;

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use fake_http::{FakeHttp, Resp};
use serde_json::json;

const PAGE: &str = r##"<html><head><title>x</title></head><body><!-- <p>old</p> --><script>var a=1;</script>
<h1>Tea &amp; cake</h1><p>one<br>two &mdash; see <a href="/recipes?a=1&amp;b=2">recipes</a> or <a href="#top">top</a></p>
<ul><li>green</li><li>black</li></ul>
<table><tr><th>Tea</th><th>Price</th></tr><tr><td>Green</td><td>3</td></tr></table></body></html>"##;
const DDG: &str = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=zz">Example <b>Site</b></a>
    <a class="result__snippet" href="x">A <b>great</b> site.</a>"#;

fn html(body: &str) -> Resp {
    Resp::bytes("text/html", body.as_bytes().to_vec())
}

async fn site() -> FakeHttp {
    FakeHttp::start(|req| match req.path.as_str() {
        "/page" => html(PAGE),
        "/app" => html("<html><body><div id=root></div><script>render()</script></body></html>"),
        "/long" => Resp::text(&"0123456789".repeat(4_500)),
        // Russian in windows-1251, named by the header or only by the page.
        "/cp1251" => Resp::bytes("text/html; charset=windows-1251", encoding_rs::WINDOWS_1251.encode("<p>Привет, мир</p>").0.into_owned()),
        "/meta1251" => {
            let page = "<html><head><meta charset=\"windows-1251\"></head><body>Пока</body></html>";
            Resp::bytes("text/html", encoding_rs::WINDOWS_1251.encode(page).0.into_owned())
        }
        "/report.pdf" => Resp::bytes("application/pdf", b"%PDF-1.4".to_vec()),
        "/brave" => {
            let q = req.query.split('&').find_map(|p| p.strip_prefix("q=")).unwrap_or_default().to_string();
            Resp::json(json!({"web": {"results": [{"title": format!("About <strong>{q}</strong>"), "url": "https://tea.example", "description": "All about tea"}]}}))
        }
        "/ddg" if req.method == "POST" => html(DDG),
        _ => Resp::status(404, "not found"),
    })
    .await
}

async fn web(env: &[(&str, &str)]) -> FakeAugust {
    let (fake, august) = FakeAugust::new(env);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

async fn fetch(fake: &FakeAugust, url: String) -> String {
    match fake.tool("web_fetch", json!({"url": url}), ctx("1")).await {
        Ok(text) => text,
        Err(e) => format!("error: {e}"),
    }
}

#[tokio::test]
async fn web_fetch_turns_a_page_into_markdown() {
    let site = site().await;
    let fake = web(&[]).await;
    // Structure survives as light Markdown, links as absolute URLs to follow.
    let page = format!("# Tea & cake\n\none\ntwo — see [recipes]({}/recipes?a=1&b=2) or top\n\n- green\n- black\n\n| Tea | Price\n| Green | 3", site.url);
    assert_eq!(fetch(&fake, format!("{}/page", site.url)).await, page);
}

#[tokio::test]
async fn web_search_uses_brave_with_a_key() {
    let site = site().await;
    let brave = format!("{}/brave", site.url);
    let fake = web(&[("BRAVE_API_KEY", "k"), ("BRAVE_API_URL", &brave)]).await;
    let found = fake.tool("web_search", json!({"query": "tea"}), ctx("1")).await.unwrap();
    assert_eq!(found, "1. About tea\n   https://tea.example\n   All about tea");
    assert_eq!(site.to("/brave")[0].header("x-subscription-token"), "k");
    assert!(site.to("/ddg").is_empty());
}

#[tokio::test]
async fn web_search_scrapes_duckduckgo_without_a_key() {
    let site = site().await;
    let ddg = format!("{}/ddg", site.url);
    let fake = web(&[("DUCKDUCKGO_URL", &ddg)]).await;
    let found = fake.tool("web_search", json!({"query": "tea"}), ctx("1")).await.unwrap();
    assert_eq!(found, "1. Example Site\n   https://example.com/a?b=1\n   A great site.");
    assert_eq!(site.to("/ddg")[0].text(), "q=tea");
}

#[tokio::test]
async fn web_fetch_says_what_to_do_instead() {
    let site = site().await;
    let fake = web(&[]).await;
    let pdf = fetch(&fake, format!("{}/report.pdf", site.url)).await;
    assert!(pdf.contains("application/pdf") && pdf.contains("curl"), "{pdf}");
    let app = fetch(&fake, format!("{}/app", site.url)).await;
    assert!(app.contains("browser tool"), "{app}");
    let missing = fetch(&fake, format!("{}/missing", site.url)).await;
    assert!(missing.contains("HTTP 404 Not Found"), "{missing}");
    let ftp = fetch(&fake, "ftp://example.com".into()).await;
    assert!(ftp.contains("only http(s) URLs"), "{ftp}");
}

#[tokio::test]
async fn web_fetch_reads_a_long_page_in_parts() {
    let site = site().await;
    let fake = web(&[]).await;
    let url = format!("{}/long", site.url);
    let first = fetch(&fake, url.clone()).await;
    assert!(first.starts_with("0123") && first.ends_with("\n\n[characters 0-30000 of 45000; call web_fetch with offset 30000 for the rest]"), "{}", &first[29_990..]);
    let rest = fake.tool("web_fetch", json!({"url": url, "offset": 30_000}), ctx("1")).await.unwrap();
    assert_eq!(rest.len(), 15_000 + "\n\n[characters 30000-45000 of 45000]".len());
    assert!(rest.ends_with("[characters 30000-45000 of 45000]"));
    let past = fake.tool("web_fetch", json!({"url": url, "offset": 50_000}), ctx("1")).await.unwrap_err();
    assert!(past.to_string().contains("past the end of the page (45000 characters)"), "{past}");
}

#[tokio::test]
async fn web_fetch_decodes_pages_in_their_charset() {
    let site = site().await;
    let fake = web(&[]).await;
    assert_eq!(fetch(&fake, format!("{}/cp1251", site.url)).await, "Привет, мир");
    assert_eq!(fetch(&fake, format!("{}/meta1251", site.url)).await, "Пока");
}
