//! Signing in is the core's mechanic: it asks for an API key (kept out of the chat and the
//! conversation, checked by the extension) or runs an extension's own sign-in script, whose
//! redirect it receives on localhost or the user pastes; it keeps the secrets.

use crate::support::*;
use august_ext::{August, Signed};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn secret(core: &Core, ext: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(core.path(&format!("secrets/{ext}.json"))).ok()?;
    serde_json::from_str::<Value>(&text).unwrap()[key].as_str().map(String::from)
}

fn acme_key(a: &August) {
    a.register_key_account("acme", "Acme", &["acme-llm"], "API key", Some("ACME_KEY"), |_, key| async move {
        anyhow::ensure!(key != "bad", "401 unauthorized");
        Ok(())
    });
    a.register_key_account("other", "Other", &[], "API key", Some("OTHER_KEY"), |_, _| async { Ok(()) });
}

fn status(accounts: &Value, id: &str) -> (String, Value) {
    let a = accounts.as_array().unwrap().iter().find(|a| a["id"] == id).unwrap();
    (a["status"].as_str().unwrap().to_string(), a["who"].clone())
}

#[tokio::test]
async fn an_api_key_signs_in_and_stays_out_of_the_chat() {
    let core = Arc::new(core().ext("acme", acme_key).env("OTHER_KEY", "from-env").start().await);
    let chat = core.chat("1");
    let login = || {
        let core = core.clone();
        tokio::spawn(async move { core.call_in("1", "login", json!({"account": "acme"})).await })
    };

    // A key the extension refuses is not kept.
    let attempt = login();
    chat.wait_for("🔑 Send your API key for Acme.").await;
    chat.say("bad");
    let refused = attempt.await.unwrap().unwrap_err().to_string();
    assert!(refused.contains("the key was not accepted") && refused.contains("401"), "{refused}");
    assert_eq!(secret(&core, "acme", "acme"), None);

    let attempt = login();
    chat.wait_until("the second question", |c| c.texts().iter().filter(|t| t.starts_with("🔑")).count() == 2).await;
    let id = chat.say("sk-test-123");
    attempt.await.unwrap().unwrap();
    assert_eq!(secret(&core, "acme", "acme").as_deref(), Some("sk-test-123"));
    // The key doesn't stay readable in the chat: the message is deleted, the question says
    // it was received.
    assert!(core.messenger.deleted().contains(&id));
    assert!(chat.history().iter().all(|t| !t.contains("sk-test-123")), "{:?}", chat.history());
    assert!(chat.texts().iter().any(|t| t.ends_with("→ received")));
    assert!(core.requests().is_empty(), "the key reached a turn");

    // Accounts tell how they stand; a variable can stand in for a key.
    let accounts = core.call("accounts", json!({})).await.unwrap();
    assert_eq!(status(&accounts, "acme").0, "connected");
    assert_eq!(status(&accounts, "other"), ("connected".into(), json!("key from $OTHER_KEY")));

    // Signing out forgets the key; /stop cancels a sign-in in progress.
    core.call("logout", json!({"account": "acme"})).await.unwrap();
    assert_eq!(secret(&core, "acme", "acme"), None);
    let attempt = login();
    chat.wait_until("a third question", |c| c.texts().iter().filter(|t| t.starts_with("🔑")).count() == 3).await;
    chat.say("/stop");
    let cancelled = attempt.await.unwrap().unwrap_err().to_string();
    assert!(cancelled.contains("sign-in cancelled"), "{cancelled}");
}

/// A sign-in of its own: a code the user types, then a redirect.
fn acme_script(a: &August) {
    let me = a.clone();
    a.register_login_account(
        "acme",
        "Acme",
        &[],
        move |_, steps| {
            let me = me.clone();
            async move {
                let code = steps.ask("Enter the code we texted you", false).await?;
                let redirect = steps.callback(0, "/acme").await?;
                steps.open(&format!("{redirect}?token=T-{code}"), "Open this link to finish:").await?;
                let q = steps.wait_callback(Duration::from_secs(20)).await?;
                me.set_secret("token", q["token"].as_str()).await?;
                Ok(Signed { who: "alice".into(), expires_at: None })
            }
        },
        {
            let me = a.clone();
            move |_| {
                let me = me.clone();
                async move { me.set_secret("token", None).await }
            }
        },
    );
}

fn link_in(text: &str) -> String {
    text.lines().find(|l| l.starts_with("http://localhost:")).unwrap().to_string()
}

#[tokio::test]
async fn an_extension_scripts_its_own_sign_in_through_august() {
    let core = Arc::new(core().ext("acme", acme_script).start().await);
    let chat = core.chat("1");
    let login = || {
        let core = core.clone();
        tokio::spawn(async move { core.call_in("1", "login", json!({"account": "acme"})).await })
    };

    // The browser on this machine lands on August's localhost receiver.
    let done = login();
    chat.wait_for("Enter the code we texted you").await;
    chat.say("4242");
    let url = link_in(&chat.wait_for("Open this link to finish").await.text);
    let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
    assert!(page.contains("Signed in"), "{page}");
    assert_eq!(done.await.unwrap().unwrap()["who"], "alice");
    assert_eq!(secret(&core, "acme", "token").as_deref(), Some("T-4242"));
    assert_eq!(status(&core.call("accounts", json!({})).await.unwrap(), "acme"), ("connected".into(), json!("alice")));

    core.call("logout", json!({"account": "acme"})).await.unwrap();
    assert_eq!(secret(&core, "acme", "token"), None);
    assert_eq!(status(&core.call("accounts", json!({})).await.unwrap(), "acme").0, "none");

    // A browser elsewhere: the user pastes the address it ended on.
    let n = chat.texts().len();
    let done = login();
    chat.wait_until("the code question", |c| c.texts()[n..].iter().any(|t| t.contains("Enter the code"))).await;
    chat.say("7");
    chat.wait_until("the paste hint", |c| c.texts()[n..].iter().any(|t| t.contains("paste that page's address here"))).await;
    let url = link_in(chat.texts()[n..].iter().find(|t| t.contains("Open this link")).unwrap());
    chat.say(&url);
    done.await.unwrap().unwrap();
    assert_eq!(secret(&core, "acme", "token").as_deref(), Some("T-7"));
}

#[tokio::test]
async fn august_starts_without_a_provider_and_points_to_login() {
    let core = core().home("config/august.json", "{}").start().await;
    core.chat("1").ask("hi", "/login").await;
    assert!(core.requests().is_empty());
}
