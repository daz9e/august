//! Drawing turns is an extension's job: the default `render` streams the reply around what
//! else lands in the chat, another extension can take the job, and without one only the
//! outcome is sent.

use crate::support::*;
use serde_json::{Value, json};

fn have_bun() -> bool {
    let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bun").is_file()));
    if !found {
        eprintln!("skipping: bun is not installed");
    }
    found
}

fn after_tool(req: &Value) -> bool {
    req["messages"].as_array().unwrap().last().unwrap()["role"] == "tool"
}

#[tokio::test]
async fn a_message_sent_mid_reply_lands_in_its_place_and_the_reply_goes_on_below() {
    let fake = Fake::llm(Box::new(|req| {
        if after_tool(req) {
            reply_text("Going with it.")
        } else {
            reply_tool("clarify", json!({"question": "Which one?", "options": ["A", "B"]}))
        }
    }))
    .await;
    let gw = august(&fake, Setup::default()).await;
    let mut chat = gw.chat().await;
    chat.say("pick for me").await;
    let question = chat.question().await;
    chat.press(&question.buttons[0].0).await;
    chat.wait_for("Going with it.").await;

    let texts = chat.texts();
    let at = |needle: &str| texts.iter().position(|t| t.contains(needle)).unwrap_or_else(|| panic!("no {needle:?} in {texts:?}"));
    let (tool, asked, reply) = (at("🔧 `clarify`"), at("❓ Which one?"), at("Going with it."));
    assert!(tool < asked && asked < reply, "{texts:?}");
    assert!(!texts[tool].contains("Going with it."), "the reply went on in a new message: {texts:?}");
}

// Sees every event of a turn and draws only its outcome, in one message.
const TIDY: &str = r#"
export default function (august) {
  august.takes("render");
  const seen = [];
  august.on("render", async (e, ctx) => {
    seen.push(e.kind);
    if (e.kind === "end") await ctx.send(`${e.status}: ${e.reply} [${seen.splice(0).join(",")}]`);
  });
}
"#;

#[tokio::test]
async fn an_extension_that_takes_render_draws_turns_its_own_way() {
    if !have_bun() {
        return;
    }
    let fake = Fake::llm(Box::new(|req| if after_tool(req) { reply_text("done") } else { reply_tool("read", json!({"path": "note.txt"})) })).await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { home: &[("extensions/tidy/index.ts", TIDY)], seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;

    // Both take the job; the default `render` comes first by name and streams as usual.
    chat.ask("read the note", "done").await;
    assert!(chat.texts().iter().any(|t| t.contains("🔧 `read`")), "{:?}", chat.texts());

    // The user puts `tidy` first: it draws the next turn alone.
    chat.ask(r#"/config august.hooks.order ["tidy"]"#, "tidy").await;
    let (n, idle) = (chat.messages().len(), chat.idles());
    chat.ask("again", "ok: done [start,tool,step,text,end]").await;
    chat.wait_until("the turn to end", |c| c.idles() > idle).await;
    assert_eq!(chat.texts()[n..], ["ok: done [start,tool,step,text,end]"], "{:?}", chat.texts());
}

#[tokio::test]
async fn without_a_renderer_only_the_outcome_is_sent() {
    let fake = Fake::llm(Box::new(|req| if after_tool(req) { reply_text("done") } else { reply_tool("read", json!({"path": "note.txt"})) })).await;
    let seed: &[(&str, &[u8])] = &[("note.txt", b"hello")];
    let gw = august(&fake, Setup { seed, ..Default::default() }).await;
    let mut chat = gw.chat().await;
    chat.ask("/extensions disable render", "render").await;
    let (n, idle) = (chat.messages().len(), chat.idles());
    chat.ask("read the note", "done").await;
    chat.wait_until("the turn to end", |c| c.idles() > idle).await;
    assert_eq!(chat.texts()[n..], ["done"], "{:?}", chat.texts());
}
