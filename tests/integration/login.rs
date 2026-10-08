//! Signing in through August: `/login` asks for an API key (and keeps it out of the chat and
//! the conversation), or runs an extension's own sign-in script, whose redirect August
//! receives on localhost or the user pastes.

use crate::support::*;
use serde_json::Value;

fn secret(gw: &Gateway, ext: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(gw.home.join(format!("secrets/{ext}.json"))).ok()?;
    serde_json::from_str::<Value>(&text).unwrap()[key].as_str().map(String::from)
}

#[tokio::test]
async fn an_api_key_signs_in_and_switches_to_the_provider() {
    let fake = Fake::llm(Box::new(|_| reply_text("hello from claude"))).await;
    let base = format!("{}/v1", fake.url);
    let env = [("ANTHROPIC_BASE_URL", base.as_str())];
    let gw = august(&fake, Setup { env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // A key the API refuses is not kept.
    chat.ask("/login anthropic", "Send your API key").await;
    chat.ask("bad", "Sign-in failed: the key was not accepted").await;
    assert_eq!(secret(&gw, "anthropic", "anthropic"), None);

    chat.ask("/login anthropic", "Send your API key").await;
    chat.ask("sk-test-123", "Now using `anthropic · claude-opus-5-5`").await;
    assert_eq!(secret(&gw, "anthropic", "anthropic").as_deref(), Some("sk-test-123"));

    chat.ask("hi", "hello from claude").await;
    let call = fake.requests().into_iter().find(|r| r.path == "/v1/messages").unwrap();
    assert_eq!(call.key, "sk-test-123");
    // The key never reached the model, nor stays readable in the chat.
    assert!(!call.text().contains("sk-test-123"));
    assert!(chat.texts().iter().all(|t| !t.contains("sk-test-123")), "{:?}", chat.texts());

    // /login lists it as signed in; /logout forgets the key.
    chat.say("/login").await;
    let q = chat.question().await;
    chat.press(&q.button("Anthropic (API key) ✓")).await;
    chat.wait_until("a second key question", |c| c.texts().iter().filter(|t| t.contains("Send your API key")).count() == 3).await;
    chat.ask("/stop", "Sign-in failed: sign-in cancelled").await;
    chat.ask("/logout anthropic", "Signed out of Anthropic").await;
    assert_eq!(secret(&gw, "anthropic", "anthropic"), None);
}

/// A sign-in of its own: a code the user types, then a redirect.
const ACME: &str = r#"
export default function (august) {
  august.registerAccount({
    id: "acme",
    label: "Acme",
    async login(steps) {
      const code = await steps.ask("Enter the code we texted you");
      const redirect = await steps.callback({ path: "/acme" });
      await steps.open(`${redirect}?token=T-${code}`, "Open this link to finish:");
      const q = await steps.waitCallback({ timeout: 20000 });
      await august.secrets.set("token", q.token);
      return { who: "alice" };
    },
    async logout() {
      await august.secrets.set("token", null);
    },
  });
}
"#;

#[tokio::test]
async fn an_extension_scripts_its_own_sign_in_through_august() {
    let fake = Fake::llm(Box::new(|_| reply_text("ok"))).await;
    let gw = august(&fake, Setup { home: &[("extensions/acme/index.ts", ACME)], ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // The browser on this machine lands on August's localhost receiver.
    chat.ask("/login acme", "Enter the code we texted you").await;
    let link = chat.ask("4242", "Open this link to finish").await;
    let url = link.lines().find(|l| l.starts_with("http://localhost:")).unwrap().to_string();
    let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
    assert!(page.contains("Signed in"), "{page}");
    chat.wait_for("Signed in to Acme as alice.").await;
    assert_eq!(secret(&gw, "acme", "token").as_deref(), Some("T-4242"));

    chat.ask("/logout acme", "Signed out of Acme").await;
    assert_eq!(secret(&gw, "acme", "token"), None);

    // A browser elsewhere: the user pastes the address it ended on.
    let n = chat.texts().len();
    chat.ask("/login acme", "Enter the code we texted you").await;
    chat.ask("7", "paste that page's address here").await;
    let link = chat.texts()[n..].iter().find(|t| t.contains("Open this link")).unwrap().clone();
    let url = link.lines().find(|l| l.starts_with("http://localhost:")).unwrap().to_string();
    chat.ask(&url, "Signed in to Acme as alice.").await;
    assert_eq!(secret(&gw, "acme", "token").as_deref(), Some("T-7"));
}

#[tokio::test]
async fn august_starts_without_a_provider_and_points_to_login() {
    let fake = Fake::llm(Box::new(|_| reply_text("unused"))).await;
    let env = [("AUGUST_PROVIDER", ""), ("AUGUST_MODEL", "")];
    let gw = august(&fake, Setup { env: &env, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("hi", "/login").await;
    assert!(fake.llm_requests().is_empty());
}
