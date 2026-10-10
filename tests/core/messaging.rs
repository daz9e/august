//! The messenger primitives as extensions use them: messengers and their threads, one
//! general message, questions and listeners in any thread, what comes in with a message
//! (attachments, replies, places, edits, reactions), and a messenger's own actions.

use crate::support::*;
use august::messengers::{Attachment, FileKind, InboundKind, Place, PlaceKind, Quote};
use august_ext::{August, Thread};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn relay(a: &August) {
    a.needs(&["messaging"]);
    let me = a.clone();
    a.register_command("where", "", move |_, _| {
        let me = me.clone();
        async move {
            let all = me.messengers().await?;
            let list: Vec<String> = all.as_array().unwrap().iter().map(|m| {
                let threads: Vec<String> = m["threads"].as_array().unwrap().iter().map(|t| format!("{}{}", t["id"].as_str().unwrap(), if t["active"] == true { "*" } else { "" })).collect();
                format!("{}[buttons {}, {}]: {}", m["id"].as_str().unwrap(), m["capabilities"]["buttons"], m["notes"].as_str().unwrap(), threads.join(","))
            }).collect();
            Ok(Some(list.join("; ")))
        }
    });
    // Asks in another thread and reports the answer back here.
    let me = a.clone();
    a.register_command("poke", "", move |id, _| {
        let me = me.clone();
        async move {
            let there = Thread { messenger: MESSENGER.into(), id };
            Ok(Some(match me.ask(&there, "Coffee?", &["Yes".into(), "No".into()], Duration::from_secs(10)).await {
                Ok(answer) => format!("answer: {}", answer.unwrap_or_default()),
                Err(e) => format!("poke failed: {e}"),
            }))
        }
    });
    let me = a.clone();
    a.register_command("wait", "", move |_, ctx| {
        let me = me.clone();
        async move {
            let l = me.listen(ctx.thread.as_ref().unwrap(), &[], true, Duration::from_secs(10)).await?;
            Ok(Some(format!("got: {:?}", me.next(l, Duration::from_millis(300)).await?)))
        }
    });
    // A listener nobody collects stops taking messages after its ttl.
    let me = a.clone();
    a.register_command("forget", "", move |_, ctx| {
        let me = me.clone();
        async move {
            me.listen(ctx.thread.as_ref().unwrap(), &[], true, Duration::from_millis(200)).await?;
            Ok(Some("listening briefly".into()))
        }
    });
}

#[tokio::test]
async fn extensions_see_messengers_and_talk_to_any_thread() {
    let core = core().model(|_| text("the agent answered")).ext("relay", relay).start().await;
    let (first, second) = (core.chat("1"), core.chat("2"));

    // Messengers describe themselves and list their threads, the latest one active.
    second.ask("hi", "the agent answered").await;
    let wh = first.ask("/where", "test[").await;
    assert!(wh.contains("test[buttons 8, A messenger for tests.]: 1*,2") || wh.contains("test[buttons 8, A messenger for tests.]: 2,1*"), "{wh}");

    // A question asked in another thread: its buttons answer it…
    first.say("/poke 2");
    let q = second.question().await;
    assert!(q.text.contains("Coffee?"), "{}", q.text);
    second.press(&q.button("No"));
    first.wait_for("answer: No").await;
    // …or the user's own words, which don't reach the agent as a message.
    let asked = core.requests().len();
    first.say("/poke 2");
    second.question().await;
    second.say("maybe later");
    first.wait_for("answer: maybe later").await;
    assert_eq!(core.requests().len(), asked, "an answer started a turn");
    // /stop in that thread cancels the question, and it says so.
    first.say("/poke 2");
    second.question().await;
    second.ask("/stop", "Stopped 0 turn(s).").await;
    first.wait_for("poke failed: the user cancelled the question").await;
    second.wait_for("→ ⏹ cancelled").await;

    // A listener without an answer gives up after its timeout, and says so.
    first.ask("/wait", "got: Timeout").await;
    first.ask("/forget", "listening briefly").await;
    settle(400).await;
    first.ask("hello", "the agent answered").await;
}

#[tokio::test]
async fn one_message_carries_text_button_rows_files_and_a_reply() {
    let core = core()
        .seed("note.txt", b"hi")
        .ext("card", |a| {
            let ws = a.workspace().clone();
            a.register_command("card", "", move |_, ctx| {
                let note = ws.join("note.txt");
                async move {
                    let message = json!({"text": "Pick one", "buttons": [[{"id": "a", "label": "Alpha"}], [{"id": "b", "label": "Beta"}]], "files": [note], "reply_to": "in1"});
                    ctx.call("send", json!({"message": message})).await?;
                    Ok(None)
                }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("/card");
    let card = chat.wait_for("Pick one").await;
    let labels: Vec<&str> = card.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["Alpha", "Beta"]);
    assert!(card.files[0].ends_with("note.txt"), "{card:?}");
    assert_eq!(card.reply_to.as_deref(), Some("in1"));
}

/// Records what `message_in` gets.
fn recorder(seen: Arc<Mutex<Vec<Value>>>) -> impl Fn(&August) + Send + Sync + 'static {
    move |a| {
        let seen = seen.clone();
        a.on("message_in", move |d, _| {
            seen.lock().unwrap().push(d);
            async { Ok(None) }
        });
    }
}

#[tokio::test]
async fn what_comes_with_a_message_reaches_message_in_and_unaddressed_chatter_runs_nothing() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let core = core().ext("rec", recorder(seen.clone())).start().await;
    let chat = core.chat("g");
    let group = Place { kind: PlaceKind::Group, parent: None, title: Some("Family".into()) };
    let quote = Quote { id: "m1".into(), text: "Paris is the capital.".into(), mine: true };
    chat.send_at(group.clone(), InboundKind::Message { id: "u1".into(), text: "why?".into(), files: vec![], reply_to: Some(quote), addressed: true });
    chat.wait_for("ok").await;
    let d = seen.lock().unwrap()[0].clone();
    assert_eq!((d["reply_to"]["text"].as_str(), d["reply_to"]["mine"].as_bool(), d["place"]["kind"].as_str(), d["place"]["title"].as_str()), (Some("Paris is the capital."), Some(true), Some("group"), Some("Family")));
    assert_eq!((d["source"].as_str(), d["id"].as_str()), (Some("user"), Some("u1")));

    // Chatter not addressed to August reaches hooks, but runs nothing.
    chat.send_at(group, InboundKind::Message { id: "u2".into(), text: "lunch?".into(), files: vec![], reply_to: None, addressed: false });
    core.wait_until("the hook", |_| seen.lock().unwrap().len() == 2).await;
    assert_eq!(seen.lock().unwrap()[1]["addressed"], false);
    settle(100).await;
    assert_eq!(core.requests().len(), 1);

    // The thread's place shows in `messengers`.
    let all = core.call("messengers", json!({})).await.unwrap();
    let thread = all[0]["threads"].as_array().unwrap().iter().find(|t| t["id"] == "g").unwrap().clone();
    assert_eq!(thread["place"]["title"], "Family");
}

#[tokio::test]
async fn a_hook_can_have_an_unaddressed_message_answered() {
    let core = core()
        .model(|req| text(&format!("re: {}", last_user_text(req))))
        .ext("mentions", |a| a.on("message_in", |d, _| async move { Ok(d["text"].as_str().unwrap().contains("august").then(|| json!({"addressed": true}))) }))
        .start()
        .await;
    let chat = core.chat("g");
    chat.send(InboundKind::Message { id: "u1".into(), text: "lunch?".into(), files: vec![], reply_to: None, addressed: false });
    chat.send(InboundKind::Message { id: "u2".into(), text: "august, lunch?".into(), files: vec![], reply_to: None, addressed: false });
    chat.wait_for("re: august, lunch?").await;
    settle(100).await;
    assert_eq!(core.requests().len(), 1);
}

#[tokio::test]
async fn attachments_are_downloaded_by_hooks_and_images_reach_the_model() {
    let photo = fixture("red.png");
    let core = core()
        .ext("attach", |a| {
            a.needs(&["messaging"]);
            a.on("message_in", |d, ctx| async move {
                let Some(file) = d["files"].as_array().and_then(|f| f.first()).cloned() else { return Ok(None) };
                let saved = ctx.call("download", json!({"file": file, "path": "inbox/photo.png"})).await?;
                let path = saved["path"].as_str().unwrap().to_string();
                Ok(Some(json!({"text": format!("{} [saved {} bytes]", d["text"].as_str().unwrap(), saved["size"]), "images": [{"path": path, "mime": "image/png"}]})))
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    let file = Attachment { id: "f1".into(), kind: FileKind::Image, name: None, mime: Some("image/png".into()), size: Some(photo.len() as u64) };
    chat.say_with("what colour?", vec![(file, photo.clone())]);
    chat.wait_for("ok").await;
    assert_eq!(std::fs::read(core.workspace.join("inbox/photo.png")).unwrap(), photo);
    let req = &core.requests()[0];
    assert_eq!(last_user_text(req), format!("what colour? [saved {} bytes]", photo.len()));
    let images: Vec<_> = req.messages.last().unwrap().content.iter().filter(|b| matches!(b, august_ext::llm::Block::Image { .. })).collect();
    assert_eq!(images.len(), 1);
}

#[tokio::test]
async fn edits_and_reactions_reach_extensions_and_august_reacts_back() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let core = core()
        .ext("watch", move |a| {
            a.needs(&["messaging"]);
            let log2 = log.clone();
            a.on("message_edited", move |d, _| {
                log2.lock().unwrap().push(format!("edited {} {}", d["id"].as_str().unwrap(), d["text"].as_str().unwrap()));
                async { Ok(None) }
            });
            let log = log.clone();
            a.on("reaction", move |d, ctx| {
                log.lock().unwrap().push(format!("reaction {} {}", d["message"].as_str().unwrap(), d["emoji"].as_str().unwrap()));
                async move {
                    ctx.call("react", json!({"id": d["message"], "emoji": "👀"})).await?;
                    Ok(None)
                }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.send(InboundKind::Edited { id: "u1".into(), text: "fixed".into() });
    chat.send(InboundKind::Reaction { message: "m1".into(), emoji: "👍".into() });
    core.wait_until("both events", |_| seen.lock().unwrap().len() == 2).await;
    let mut got = seen.lock().unwrap().clone();
    got.sort();
    assert_eq!(got, ["edited u1 fixed", "reaction m1 👍"]);
    core.wait_until("the reaction back", |c| !c.messenger.reactions().is_empty()).await;
    assert_eq!(core.messenger.reactions()[0], ("1".into(), "m1".into(), "👀".into()));
    // Nothing of it started a turn.
    assert!(core.requests().is_empty());
}

#[tokio::test]
async fn a_messengers_own_actions_run_with_checked_arguments_and_threads_open() {
    let core = core().start().await;
    let refused = core.call_in("1", "action", json!({"action": "pin", "args": {}})).await.unwrap_err().to_string();
    assert!(refused.contains("missing `message`"), "{refused}");
    let missing = core.call_in("1", "action", json!({"action": "poll", "args": {}})).await.unwrap_err().to_string();
    assert!(missing.contains("no action `poll`"), "{missing}");
    assert_eq!(core.call_in("1", "action", json!({"action": "pin", "args": {"message": "m1"}})).await.unwrap(), "pinned");
    let opened = core.call_in("1", "open_thread", json!({"title": "Trip"})).await.unwrap();
    assert_eq!(opened, json!({"messenger": MESSENGER, "id": "1/topic"}));
    assert_eq!(core.messenger.actions(), [("pin".into(), "1".into(), json!({"message": "m1"})), ("open_thread".into(), "1".into(), json!({"title": "Trip"}))]);
    let all = core.call("messengers", json!({})).await.unwrap();
    assert_eq!(all[0]["actions"][0]["name"], "pin");
}
