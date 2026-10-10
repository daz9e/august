//! Drawing turns is an extension's job (`takes: render`): it gets every event of a visible
//! turn; of several, the user's order picks one; without one only the outcome is sent.

use crate::support::*;
use august_ext::August;
use serde_json::json;
use std::sync::{Arc, Mutex};

fn reads_then_done() -> impl Fn(&august_ext::llm::Request) -> Reply + Send + Sync + 'static {
    |req| if tool_result(req).is_some() { text("done") } else { tool("read", json!({"path": "note.txt"})) }
}

/// Draws only a turn's outcome, in one message, tagged with its name and what it saw.
fn tidy(tag: &'static str) -> impl Fn(&August) + Send + Sync + 'static {
    move |a| {
        a.takes(&["render"]);
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        a.on("render", move |e, ctx| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(e["kind"].as_str().unwrap().to_string());
                if e["kind"] == "end" {
                    let kinds = std::mem::take(&mut *seen.lock().unwrap()).join(",");
                    ctx.send(&format!("{tag} {}: {} [{kinds}]", e["status"].as_str().unwrap(), e["reply"].as_str().unwrap())).await?;
                }
                Ok(None)
            }
        });
    }
}

#[tokio::test]
async fn an_extension_that_takes_render_draws_turns_its_own_way() {
    let core = core().model(reads_then_done()).seed("note.txt", b"hello").ext("files", files).ext("alpha", tidy("alpha")).ext("tidy", tidy("tidy")).start().await;
    let chat = core.chat("1");

    // Both take the job; the first by name draws.
    chat.ask("read the note", "alpha ok: done [start,tool,step,text,end]").await;

    // The user puts `tidy` first: it draws the next turn alone.
    core.call("config_set", json!({"path": "august.hooks.order", "value": ["tidy"]})).await.unwrap();
    let (n, idle) = (chat.messages().len(), chat.idles());
    chat.ask("again", "tidy ok: done [start,tool,step,text,end]").await;
    chat.wait_until("the turn to end", |c| c.idles() > idle).await;
    assert_eq!(chat.texts()[n..], ["tidy ok: done [start,tool,step,text,end]"], "{:?}", chat.texts());
}

#[tokio::test]
async fn without_a_renderer_only_the_outcome_is_sent() {
    let core = core().model(reads_then_done()).seed("note.txt", b"hello").ext("files", files).start().await;
    let chat = core.chat("1");
    chat.ask("read the note", "done").await;
    chat.idle(1).await;
    assert_eq!(chat.texts(), ["done"]);
}
