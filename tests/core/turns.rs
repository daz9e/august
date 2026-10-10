//! Turns and their modes: visible, quiet, fork and fresh turns started by extensions,
//! messages arriving mid-turn, follow-ups, the step budget.

use crate::support::*;
use august_ext::August;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn write_tool(a: &August) {
    let ws = a.workspace().clone();
    a.register_tool("write", "Write a file", json!({"type": "object"}), move |input, _| {
        let path = ws.join(input["path"].as_str().unwrap_or("x"));
        async move {
            std::fs::write(path, "written")?;
            Ok("written".into())
        }
    });
}

fn turnkit(a: &August) {
    a.needs(&["turns", "messaging"]);
    let me = a.clone();
    let run = move |thread: august_ext::Thread, turn: serde_json::Value| {
        let me = me.clone();
        async move {
            let mut turn = turn;
            turn["source"] = json!("turnkit");
            let id = me.start_turn(&thread, turn).await?;
            me.wait_turn(id, Duration::from_secs(10)).await
        }
    };
    let r = run.clone();
    a.register_command("quiet", "", move |_, ctx| {
        let r = r.clone();
        async move {
            let out = r(ctx.thread.clone().unwrap(), json!({"text": "check quietly", "mode": "quiet"})).await?;
            Ok(Some(format!("quiet {}: {}", out["status"].as_str().unwrap(), out["reply"].as_str().unwrap())))
        }
    });
    let r = run.clone();
    a.register_command("fork", "", move |_, ctx| {
        let r = r.clone();
        async move {
            let out = r(ctx.thread.clone().unwrap(), json!({"text": "look back", "mode": "fork", "tools": ["read"]})).await?;
            let calls: Vec<String> = out["toolCalls"].as_array().unwrap().iter().map(|c| format!("{}:{}", c["name"].as_str().unwrap(), c["isError"])).collect();
            Ok(Some(format!("fork {}: {} | {}", out["status"].as_str().unwrap(), out["reply"].as_str().unwrap(), calls.join(","))))
        }
    });
    let me = a.clone();
    a.register_command("spawn", "", move |_, ctx| {
        let me = me.clone();
        async move {
            let thread = ctx.thread.clone().unwrap();
            let id = me.start_turn(&thread, json!({"text": "a long job", "mode": "fresh"})).await?;
            let (waiter, c) = (me.clone(), ctx.clone());
            tokio::spawn(async move {
                let out = waiter.wait_turn(id, Duration::from_secs(60)).await.unwrap();
                c.send(&format!("spawned {}", out["status"].as_str().unwrap())).await.ok();
            });
            let running = me.call("turns", json!({"thread": thread})).await?;
            let modes: Vec<&str> = running.as_array().unwrap().iter().filter_map(|t| t["mode"].as_str()).collect();
            Ok(Some(format!("running: {}", modes.join(","))))
        }
    });
    a.on("turn_end", |d, ctx| async move {
        if let Some(turn) = ctx.turn.as_ref().filter(|t| t.source.as_deref() == Some("turnkit")) {
            ctx.send(&format!("ended {} {}", turn.mode, d["status"].as_str().unwrap())).await?;
        }
        Ok(None)
    });
}

#[tokio::test]
async fn extensions_run_quiet_fork_and_fresh_turns() {
    let core = core()
        .model(|req| {
            let all = all_text(req);
            if all.contains("a long job") {
                return text("job done").after(Duration::from_secs(600));
            }
            if tool_result(req).is_some() {
                return text("FORK-REPLY");
            }
            match last_user_text(req) {
                t if t.contains("look back") => tool("write", json!({"path": "x.txt"})),
                t if t.contains("check quietly") => text("QUIET-REPLY"),
                _ => text(&format!("saw quiet: {}", all.contains("QUIET-REPLY"))),
            }
        })
        .ext("files", files)
        .ext("writer", write_tool)
        .ext("turnkit", turnkit)
        .start()
        .await;
    let chat = core.chat("1");

    // A quiet turn shows nothing but stays in the conversation.
    chat.ask("/quiet", "quiet ok: QUIET-REPLY").await;
    chat.wait_for("ended quiet ok").await;
    assert_eq!(chat.texts().iter().filter(|t| t.contains("QUIET-REPLY")).count(), 1, "{:?}", chat.texts());
    chat.ask("hello", "saw quiet: true").await;

    // A fork may only call what it's allowed, and leaves the conversation as it was.
    chat.ask("/fork", "fork ok: FORK-REPLY | write:true").await;
    assert!(!core.workspace.join("x.txt").exists());
    chat.ask("hello again", "saw quiet: true").await;
    let last = all_text(core.requests().last().unwrap());
    assert!(!last.contains("look back") && !last.contains("FORK-REPLY"), "the fork was kept");

    // Stopping the thread cancels its turns, sub-agents included.
    chat.ask("/spawn", "running: fresh").await;
    assert_eq!(core.call_in(&chat.thread, "stop", json!({})).await.unwrap()["cancelled"], 1);
    chat.wait_for("spawned cancelled").await;
}

#[tokio::test]
async fn an_extension_runs_a_visible_turn_in_another_thread_and_reports_back() {
    let core = core()
        .model(|req| {
            let t = last_user_text(req);
            if tool_result(req).is_some() {
                text("handed off")
            } else if t.contains("hand-off #") && t.contains("FILE-DONE") {
                text("the other window says FILE-DONE")
            } else if t.contains("create the file") {
                text("FILE-DONE")
            } else if let Some(thread) = t.split("other window ").nth(1) {
                tool("hand_off", json!({"thread": thread.trim(), "task": "create the file"}))
            } else {
                text("ok")
            }
        })
        .ext("handoff", |a| {
            a.needs(&["turns", "messaging"]);
            let me = a.clone();
            a.register_tool("hand_off", "Have August work on a task in another thread", json!({"type": "object"}), move |input, ctx| {
                let me = me.clone();
                async move {
                    let there = august_ext::Thread { messenger: MESSENGER.into(), id: input["thread"].as_str().unwrap().into() };
                    let id = me.start_turn(&there, json!({"text": input["task"], "mode": "visible", "parent": ctx.turn.as_ref().map(|t| t.id)})).await?;
                    let (me, here) = (me.clone(), ctx.thread.clone().unwrap());
                    tokio::spawn(async move {
                        let out = me.wait_turn(id, Duration::from_secs(10)).await.unwrap();
                        let report = format!("hand-off #{id} {}: {}", out["status"].as_str().unwrap(), out["reply"].as_str().unwrap());
                        me.call("prompt", json!({"thread": here, "text": report, "deliver": "followUp"})).await.unwrap();
                    });
                    Ok(format!("started #{id}"))
                }
            });
        })
        .start()
        .await;
    let (here, there) = (core.chat("1"), core.chat("2"));

    here.ask("ask the other window 2", "handed off").await;
    // The turn runs and shows in the other window like one of the user's…
    there.wait_for("FILE-DONE").await;
    // …and its outcome comes back to where it was asked for.
    here.wait_for("the other window says FILE-DONE").await;
}

#[tokio::test]
async fn message_during_tool_use_joins_the_running_turn() {
    let steers: Arc<std::sync::Mutex<Vec<bool>>> = Arc::default();
    let (seen, release) = (steers.clone(), gate());
    let tool_gate = release.clone();
    let core = core()
        .model(|req| match tool_result(req) {
            Some(_) => text(&format!("done, saw blue: {}", all_text(req).contains("use the blue theme"))),
            None => tool("work", json!({})),
        })
        .ext("worker", move |a| {
            let g = tool_gate.clone();
            a.register_tool("work", "Works until let go", json!({"type": "object"}), move |_, _| {
                let g = g.clone();
                async move {
                    g.acquire().await?.forget();
                    Ok("worked".into())
                }
            });
            let seen = seen.clone();
            a.on("message_in", move |d, _| {
                seen.lock().unwrap().push(d["steer"] == true);
                async { Ok(None) }
            });
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("start the job");
    core.wait_until("the tool to run", |c| c.requests().len() == 1).await;
    chat.say("use the blue theme");
    core.wait_until("the second message to arrive", |_| steers.lock().unwrap().len() == 2).await;
    release.add_permits(1);

    chat.wait_for("done, saw blue: true").await;
    // One turn: the message never started its own, and message_in knew it steers.
    assert_eq!(core.requests().len(), 2);
    assert_eq!(*steers.lock().unwrap(), [false, true]);
}

#[tokio::test]
async fn message_during_the_final_answer_runs_next_and_a_follow_up_keeps_its_own_turn() {
    let first = gate();
    let g = first.clone();
    let core = core()
        .model(move |req| {
            let reply = text(&format!("re: {}", last_user_text(req)));
            if req.messages.len() == 1 { reply.gated(&g) } else { reply }
        })
        .start()
        .await;
    let chat = core.chat("1");
    chat.say("first");
    core.wait_until("the first call", |c| c.requests().len() == 1).await;
    chat.say("second");
    core.call_in("1", "prompt", json!({"text": "third", "deliver": "followUp"})).await.unwrap();
    settle(50).await;
    first.add_permits(1);
    chat.wait_until("all three replies", |c| ["re: first", "re: second", "re: third"].iter().all(|r| c.texts().iter().any(|t| t == r))).await;
    assert_eq!(core.requests().len(), 3);
}

#[tokio::test]
async fn out_of_steps_the_agent_reports_progress() {
    let core = core()
        .model(|req| if req.tools.is_empty() { text("Did 3 steps; the rest is left for next time.") } else { tool("read", json!({"path": "x"})) })
        .ext("files", files)
        .env("AUGUST_MAX_STEPS", "3")
        .start()
        .await;
    core.chat("1").ask("loop forever", "Did 3 steps").await;
    let reqs = core.requests();
    assert_eq!(reqs.len(), 4);
    let last = reqs.last().unwrap();
    assert!(all_text(last).contains("Step limit reached") || last.system.contains("Step limit reached"));
}
