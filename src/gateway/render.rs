//! Turns an agent's event stream into sent and edited chat messages.

use crate::channels::Channel;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// What the live message should show, in order.
pub(super) enum Ui {
    Text(String),
    Step,
    Tool(String),
}

pub(super) fn tool_line(name: &str, input: &serde_json::Value) -> String {
    let arg = input
        .as_object()
        .and_then(|o| o.values().find_map(|v| v.as_str()))
        .unwrap_or("");
    let arg: String = arg.replace(['`', '\n'], " ").chars().take(80).collect();
    format!("🔧 `{name}` {}", if arg.is_empty() { String::new() } else { format!("`{arg}`") })
}

/// Turns the stream of `Ui` items into sent/edited messages, throttled to the channel's limits.
pub(super) async fn render(channel: Arc<dyn Channel>, chat: String, mut rx: mpsc::UnboundedReceiver<Ui>) {
    let limits = channel.limits();
    let mut buf = String::new();
    let mut sent: Vec<(String, String)> = Vec::new(); // (message id, markdown shown)
    let mut dirty = false;
    let mut last = Instant::now() - limits.edit_interval;

    let sep = |buf: &mut String| {
        if !buf.is_empty() && !buf.ends_with("\n\n") {
            buf.push_str(if buf.ends_with('\n') { "\n" } else { "\n\n" });
        }
    };

    loop {
        let wait = limits.edit_interval.saturating_sub(last.elapsed());
        match tokio::time::timeout(if dirty { wait } else { Duration::from_secs(3600) }, rx.recv()).await {
            Ok(Some(ui)) => {
                match ui {
                    Ui::Text(t) => buf.push_str(&t),
                    Ui::Step => sep(&mut buf),
                    Ui::Tool(line) => {
                        sep(&mut buf);
                        buf.push_str(&line);
                        buf.push_str("\n\n");
                    }
                }
                dirty = true;
                if last.elapsed() < limits.edit_interval {
                    continue;
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
        if dirty {
            flush(&*channel, &chat, &buf, &mut sent, limits.max_len).await;
            dirty = false;
            last = Instant::now();
        }
    }
    if dirty {
        flush(&*channel, &chat, &buf, &mut sent, limits.max_len).await;
    }
}

async fn flush(channel: &dyn Channel, chat: &str, buf: &str, sent: &mut Vec<(String, String)>, max: usize) {
    let text = buf.trim_end();
    if text.is_empty() {
        return;
    }
    for (i, chunk) in crate::channels::split_markdown(text, max).into_iter().enumerate() {
        if let Some((id, shown)) = sent.get_mut(i) {
            if *shown != chunk {
                match channel.edit(chat, id, &chunk, &[]).await {
                    Ok(()) => *shown = chunk,
                    Err(e) => eprintln!("{}: edit failed: {e:#}", channel.id()),
                }
            }
        } else {
            match channel.send(chat, &chunk, &[]).await {
                Ok(id) => sent.push((id, chunk)),
                Err(e) => eprintln!("{}: send failed: {e:#}", channel.id()),
            }
        }
    }
}
