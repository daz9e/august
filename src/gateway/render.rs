//! Turns an agent's event stream into sent and edited chat messages.

use crate::messengers::{Messenger, OutMessage};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// What the live message should show, in order.
pub(super) enum Ui {
    Text(String),
    Step,
    Tool(String),
    /// Send a file; text after it continues in a new message below.
    File { path: PathBuf, caption: String, done: oneshot::Sender<anyhow::Result<()>> },
}

pub(super) fn tool_line(name: &str, input: &serde_json::Value) -> String {
    // The first string argument, else the first list of strings (e.g. a command line).
    let values = || input.as_object().into_iter().flat_map(|o| o.values());
    let arg = values().find_map(|v| v.as_str().map(String::from)).or_else(|| {
        values().find_map(|v| Some(v.as_array()?.iter().map(|s| s.as_str()).collect::<Option<Vec<_>>>()?.join(" ")))
    });
    let arg: String = arg.unwrap_or_default().replace(['`', '\n'], " ").chars().take(80).collect();
    format!("🔧 `{name}` {}", if arg.is_empty() { String::new() } else { format!("`{arg}`") })
}

/// Turns the stream of `Ui` items into sent/edited messages, throttled to the channel's limits.
pub(super) async fn render(channel: Arc<dyn Messenger>, chat: String, mut rx: mpsc::UnboundedReceiver<Ui>) {
    let caps = channel.describe().capabilities;
    let interval = Duration::from_millis(caps.edit_interval_ms);
    // Without edits the reply goes out once, when it is complete.
    let live = caps.edit;
    let mut buf = String::new();
    let mut sent: Vec<(String, String)> = Vec::new(); // (message id, markdown shown)
    let mut dirty = false;
    let mut last = Instant::now() - interval;

    let sep = |buf: &mut String| {
        if !buf.is_empty() && !buf.ends_with("\n\n") {
            buf.push_str(if buf.ends_with('\n') { "\n" } else { "\n\n" });
        }
    };

    loop {
        let wait = interval.saturating_sub(last.elapsed());
        match tokio::time::timeout(if dirty && live { wait } else { Duration::from_secs(3600) }, rx.recv()).await {
            Ok(Some(ui)) => {
                match ui {
                    Ui::Text(t) => buf.push_str(&t),
                    Ui::Step => sep(&mut buf),
                    Ui::Tool(line) => {
                        sep(&mut buf);
                        buf.push_str(&line);
                        buf.push_str("\n\n");
                    }
                    Ui::File { path, caption, done } => {
                        if dirty {
                            flush(&*channel, &chat, &buf, &mut sent, caps.max_len).await;
                        }
                        let file = OutMessage { text: caption, files: vec![path], ..Default::default() };
                        done.send(channel.send(&chat, &file).await.map(drop)).ok();
                        buf.clear();
                        sent.clear();
                        dirty = false;
                        last = Instant::now();
                        continue;
                    }
                }
                dirty = true;
                if !live || last.elapsed() < interval {
                    continue;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
        if dirty {
            flush(&*channel, &chat, &buf, &mut sent, caps.max_len).await;
            dirty = false;
            last = Instant::now();
        }
    }
    if dirty {
        flush(&*channel, &chat, &buf, &mut sent, caps.max_len).await;
    }
}

async fn flush(channel: &dyn Messenger, chat: &str, buf: &str, sent: &mut Vec<(String, String)>, max: usize) {
    let text = buf.trim_end();
    if text.is_empty() {
        return;
    }
    for (i, chunk) in crate::messengers::split_markdown(text, max).into_iter().enumerate() {
        if let Some((id, shown)) = sent.get_mut(i) {
            if *shown != chunk {
                match channel.edit(chat, id, &OutMessage::text(chunk.clone())).await {
                    Ok(()) => *shown = chunk,
                    Err(e) => eprintln!("{}: edit failed: {e:#}", channel.id()),
                }
            }
        } else {
            match channel.send(chat, &OutMessage::text(chunk.clone())).await {
                Ok(id) => sent.push((id, chunk)),
                Err(e) => eprintln!("{}: send failed: {e:#}", channel.id()),
            }
        }
    }
}
