//! `render`: draws the user's visible turns. The reply streams into a message edited in place
//! (at most as often as the messenger allows), with a line per tool call; long replies go on
//! in more messages. Take `render` in another extension to draw differently.

use august_ext::{August, Ctx, split_markdown};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One thread's turn being drawn.
struct Draw {
    /// Sent messages can be edited, so the reply streams in place.
    live: bool,
    interval: Duration,
    max_len: usize,
    /// Everything shown since the last break, as Markdown.
    buf: String,
    /// `(message id, Markdown shown)` of each message the buffer spans.
    sent: Vec<(String, String)>,
    last: Instant,
    /// Reply text arrived (else the outcome's reply is shown at the end).
    streamed: bool,
}

impl Draw {
    fn new(caps: &Value) -> Self {
        let interval = Duration::from_millis(caps["edit_interval_ms"].as_u64().unwrap_or(1000));
        Self {
            live: caps["edit"] == true,
            interval,
            max_len: caps["max_len"].as_u64().unwrap_or(4000) as usize,
            buf: String::new(),
            sent: Vec::new(),
            last: Instant::now() - interval,
            streamed: false,
        }
    }

    /// Starts a new paragraph unless one just started.
    fn sep(&mut self) {
        if !self.buf.is_empty() && !self.buf.ends_with("\n\n") {
            self.buf.push_str(if self.buf.ends_with('\n') { "\n" } else { "\n\n" });
        }
    }

    fn line(&mut self, line: &str) {
        self.sep();
        self.buf.push_str(line);
        self.buf.push_str("\n\n");
    }

    /// Shows the buffer: edits the messages that changed, sends the pieces that are new.
    async fn flush(&mut self, ctx: &Ctx) {
        let text = self.buf.trim_end();
        if text.is_empty() {
            return;
        }
        for (i, chunk) in split_markdown(text, self.max_len).into_iter().enumerate() {
            if let Some((id, shown)) = self.sent.get_mut(i) {
                if *shown != chunk {
                    match ctx.call("edit", json!({"id": id, "message": chunk})).await {
                        Ok(_) => *shown = chunk,
                        Err(e) => eprintln!("edit failed: {e:#}"),
                    }
                }
            } else {
                match ctx.send(&chunk).await {
                    Ok(id) => self.sent.push((id, chunk)),
                    Err(e) => eprintln!("send failed: {e:#}"),
                }
            }
        }
        self.last = Instant::now();
    }
}

/// `🔧 `name` `first argument``: the tool and its first string argument (or list of strings).
fn tool_line(name: &str, input: &Value) -> String {
    let values = || input.as_object().into_iter().flat_map(|o| o.values());
    let arg = values().find_map(|v| v.as_str().map(String::from)).or_else(|| {
        values().find_map(|v| Some(v.as_array()?.iter().map(|s| s.as_str()).collect::<Option<Vec<_>>>()?.join(" ")))
    });
    let arg: String = arg.unwrap_or_default().replace(['`', '\n'], " ").chars().take(80).collect();
    format!("🔧 `{name}` {}", if arg.is_empty() { String::new() } else { format!("`{arg}`") })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.takes(&["render"]);
    // Waiting out the messenger's edit interval, then sending a long reply in pieces.
    august.hook_timeout("render", Duration::from_secs(60));
    // The core hands one thread's events over one at a time, so each handler owns its draw.
    let draws: Arc<Mutex<HashMap<String, Draw>>> = Arc::default();
    august.on("render", move |e, ctx| {
        let draws = draws.clone();
        async move {
            let key = ctx.key();
            let taken = draws.lock().unwrap().remove(&key);
            let mut d = match (e["kind"].as_str(), taken) {
                (Some("start"), _) | (_, None) => Draw::new(&e["capabilities"]),
                (_, Some(d)) => d,
            };
            match e["kind"].as_str().unwrap_or_default() {
                "start" => {}
                "text" => {
                    d.buf.push_str(e["text"].as_str().unwrap_or_default());
                    d.streamed = true;
                }
                "step" => d.sep(),
                "tool" => d.line(&tool_line(e["tool"].as_str().unwrap_or_default(), &e["input"])),
                "compacted" => d.line("🗜 Older messages summarised to free up context"),
                "break" => {
                    d.flush(&ctx).await;
                    d.buf.clear();
                    d.sent.clear();
                }
                "end" => {
                    let reply = e["reply"].as_str().unwrap_or_default();
                    match e["status"].as_str() {
                        Some("ok") if d.streamed => {}
                        Some("ok") => d.buf.push_str(if reply.is_empty() { "(empty reply)" } else { reply }),
                        Some("cancelled") => d.line("⏹ Stopped."),
                        _ => d.line(&format!("⚠️ Error: {}", e["error"].as_str().unwrap_or_default())),
                    }
                    d.flush(&ctx).await;
                    return Ok(None);
                }
                _ => {}
            }
            // Text that arrives while this waits comes merged in the next event.
            if d.live {
                tokio::time::sleep(d.interval.saturating_sub(d.last.elapsed())).await;
                d.flush(&ctx).await;
            }
            draws.lock().unwrap().insert(key, d);
            Ok(None)
        }
    });
    august.run().await;
}
