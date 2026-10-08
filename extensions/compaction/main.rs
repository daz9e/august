//! `compaction`: keeps the conversation inside the model's context window. Before a model
//! call whose context has grown past the limit (`AUGUST_CONTEXT_TOKENS`, else 80% of the
//! model's window, else 100k tokens) it trims old tool output and images, then summarises
//! older messages in fixed sections and hands the core the shorter history (`context` →
//! `history`); a later compaction updates that summary instead of summarising it again. A
//! context the model refused as too long is compacted and tried again (`llm_error`), and
//! `/compact` does it now. Others hook `compaction:before` (cancel, or write the summary)
//! and `compaction:after`.

use anyhow::{Result, bail};
use august_ext::llm::{Block, Message, Role};
use august_ext::{August, Ctx};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The limit when neither the setting nor the model's window says.
const DEFAULT_CONTEXT_TOKENS: usize = 100_000;
/// Messages at the end of the history that compaction leaves untouched.
const KEEP_RECENT: usize = 8;
/// Older tool results longer than this are cut down to a head and a tail.
const PRUNE_OVER: usize = 2_000;
/// Rough cost of one image in the model's context.
const IMAGE_TOKENS: usize = 1_600;
/// Characters of the summarised transcript sent to the model at most.
const TRANSCRIPT_MAX: usize = 150_000;
/// Marks the summary message at the start of a compacted history.
const SUMMARY_MARK: &str = "[Summary of the earlier conversation]";

const SUMMARY_SYSTEM: &str = "You compress conversations for an AI assistant that will continue \
    them. Write the summary in exactly these sections (omit a section only if it would be empty):\n\
    ## Goal\nWhat the user is trying to accomplish.\n\
    ## Constraints & Preferences\nWhat the user asked for or ruled out, how they want things done.\n\
    ## Progress\n### Done\nCompleted work: files, commands, results.\n### In Progress\n### Blocked\n\
    ## Key Decisions\nDecisions made and why.\n\
    ## Relevant Files\nPaths read, changed or created, with a note on each.\n\
    ## Next Steps\n\
    ## Critical Context\nExact values, names, ids, error messages still needed.\n\
    Keep names, paths, numbers and identifiers exact. Use the conversation's language. When a \
    current summary is given, update it with the new transcript instead of starting over: move \
    finished items to Done, drop what is obsolete, keep what still matters. Output only the summary.";

/// A conversation as the core hands it over.
struct Conv {
    messages: Vec<Message>,
    /// Characters of the system prompt.
    system: usize,
    /// Input tokens the provider reported last (0: none since the history last changed).
    reported: usize,
    limit: usize,
}

impl Conv {
    fn of(data: &Value, messages: &Value) -> Result<Self> {
        let messages = messages.as_array().into_iter().flatten().map(Message::from_json).collect::<Option<Vec<_>>>();
        let Some(messages) = messages else { bail!("malformed messages") };
        let set = std::env::var("AUGUST_CONTEXT_TOKENS").ok().and_then(|v| v.parse().ok());
        let window = data["window"].as_u64().map(|w| w as usize * 4 / 5);
        Ok(Self {
            messages,
            system: data["system"].as_str().map_or(0, |s| s.chars().count()),
            reported: data["tokens"].as_u64().unwrap_or(0) as usize,
            limit: set.or(window).unwrap_or(DEFAULT_CONTEXT_TOKENS),
        })
    }

    fn estimate(&self) -> usize {
        // ~3 characters per token; characters, not bytes, so Cyrillic isn't overcounted.
        let chars: usize = self.messages.iter().map(|m| m.content_json().chars().count()).sum();
        let images = self.messages.iter().flat_map(|m| &m.content).filter(|b| matches!(b, Block::Image { .. })).count();
        let estimate = (chars + self.system) / 3 + images * IMAGE_TOKENS + 2_000; // + tool definitions
        estimate.max(self.reported)
    }

    fn prune_old_blocks(&mut self) -> bool {
        let end = self.messages.len().saturating_sub(KEEP_RECENT);
        let mut changed = false;
        for b in self.messages[..end].iter_mut().flat_map(|m| &mut m.content) {
            match b {
                // Old images stop being re-sent; the file stays in the workspace.
                Block::Image { path, .. } => {
                    *b = Block::Text(format!("[image: {path}]"));
                    changed = true;
                }
                Block::ToolResult { content, .. } if content.len() > PRUNE_OVER => {
                    let head: String = content.chars().take(1_200).collect();
                    let tail: Vec<char> = content.chars().rev().take(400).collect();
                    let tail: String = tail.into_iter().rev().collect();
                    *content = format!("{head}\n... [old output trimmed] ...\n{tail}");
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    /// Index where the kept tail may start: an assistant message, or a user message that
    /// doesn't answer a tool call.
    fn cut_point(&self) -> Option<usize> {
        let mut i = self.messages.len().checked_sub(KEEP_RECENT)?;
        while i >= 1 {
            let m = &self.messages[i];
            let answers_tool = m.content.iter().any(|b| matches!(b, Block::ToolResult { .. }));
            if m.role == Role::Assistant || !answers_tool {
                return Some(i);
            }
            i -= 1;
        }
        None
    }

    /// Replaces the messages before `cut` with `summary`.
    fn replace_with_summary(&mut self, cut: usize, summary: &str) {
        let note = format!("{SUMMARY_MARK}\n{}", summary.trim());
        let mut tail = self.messages.split_off(cut);
        if tail[0].role == Role::User {
            tail[0].content.insert(0, Block::Text(note)); // keeps roles alternating
        } else {
            tail.insert(0, Message::user_text(note));
        }
        self.messages = tail;
    }
}

/// Per thread: after a failed or useless summary, don't try again until the history has
/// grown to this length.
type RetryAt = Arc<Mutex<HashMap<String, usize>>>;

/// Frees up context when it has grown past the limit (`threshold`), or always (`manual`,
/// `overflow`): trims old tool output and images, then summarises older messages. Returns
/// the estimated tokens before and after, if anything changed.
async fn compact(ctx: &Ctx, retry_at: &RetryAt, conv: &mut Conv, reason: &str) -> Result<Option<(usize, usize)>> {
    let force = reason != "threshold";
    let before = conv.estimate();
    if !force && before < conv.limit {
        return Ok(None);
    }
    let mut changed = conv.prune_old_blocks();
    let mut from_ext = None;
    if force || conv.estimate() >= conv.limit {
        from_ext = summarise_old(ctx, retry_at, conv, reason).await?;
        changed |= from_ext.is_some();
    }
    if !changed {
        return Ok(None);
    }
    conv.reported = 0;
    let after = conv.estimate();
    if after >= conv.limit {
        // The kept tail alone is too big; don't pay for a summary on every step.
        retry_at.lock().unwrap().insert(ctx.key(), conv.messages.len() + 4);
    }
    let data = json!({"before": before, "after": after, "reason": reason, "fromExtension": from_ext});
    if let Err(e) = ctx.call("journal_append", json!({"type": "compaction", "data": data})).await {
        eprintln!("journal: {e:#}");
    }
    ctx.emit("after", data).await?;
    Ok(Some((before, after)))
}

/// Summarises the messages before the kept tail; `Some(from an extension)` if it did.
async fn summarise_old(ctx: &Ctx, retry_at: &RetryAt, conv: &mut Conv, reason: &str) -> Result<Option<bool>> {
    let Some(cut) = conv.cut_point() else { return Ok(None) };
    if conv.messages.len() < retry_at.lock().unwrap().get(&ctx.key()).copied().unwrap_or(0) {
        return Ok(None);
    }
    // A summary from an earlier compaction is updated, not summarised again.
    let mut old = conv.messages[..cut].to_vec();
    let mut previous = None;
    if let Some(Block::Text(t)) = old.first_mut().and_then(|m| m.content.first_mut())
        && let Some(prev) = t.strip_prefix(SUMMARY_MARK)
    {
        previous = Some(prev.trim().to_string());
        t.clear();
    }
    // Another extension may cancel the summary or write it (another template, a cheaper model).
    let messages: Vec<Value> = old.iter().map(Message::to_json).collect();
    let asked = json!({
        "reason": reason, "tokens": conv.estimate(), "messages": messages,
        "previousSummary": previous, "kept": conv.messages.len() - cut,
    });
    let answer = ctx.emit("before", asked).await?;
    if answer["cancel"] == true {
        return Ok(None);
    }
    if let Some(s) = answer["summary"].as_str().filter(|s| !s.trim().is_empty()) {
        conv.replace_with_summary(cut, s);
        return Ok(Some(true));
    }
    let transcript = transcript(&old);
    let ask = match previous {
        Some(p) => format!("Current summary:\n\n{p}\n\nNew transcript to fold into it:\n\n{transcript}"),
        None => format!("Transcript to summarise:\n\n{transcript}"),
    };
    let summary = match ctx.llm(&ask, Some(SUMMARY_SYSTEM)).await {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => bail!("the model returned an empty summary"),
        Err(e) => {
            retry_at.lock().unwrap().insert(ctx.key(), conv.messages.len() + 10);
            return Err(e);
        }
    };
    conv.replace_with_summary(cut, &summary);
    Ok(Some(false))
}

/// Plain-text rendering of messages for the summariser.
fn transcript(msgs: &[Message]) -> String {
    let clip = |s: &str, n: usize| -> String {
        if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
    };
    let mut lines = Vec::new();
    for m in msgs {
        let who = if m.role == Role::User { "User" } else { "Assistant" };
        for b in &m.content {
            match b {
                Block::Text(t) if t.is_empty() => {}
                Block::Text(t) => lines.push(format!("{who}: {}", clip(t, 4_000))),
                Block::ToolUse { name, input, .. } => lines.push(format!("[tool call {name} {}]", clip(&input.to_string(), 500))),
                Block::ToolResult { content, is_error, .. } => {
                    lines.push(format!("[tool {} {}]", if *is_error { "error" } else { "result" }, clip(content, 600)))
                }
                Block::Image { path, .. } => lines.push(format!("{who}: [image {path}]")),
                Block::Opaque(_) => {}
            }
        }
    }
    let text = lines.join("\n");
    if text.len() <= TRANSCRIPT_MAX {
        return text;
    }
    let mut from = text.len() - TRANSCRIPT_MAX;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    format!("[earlier part omitted]\n{}", &text[from..])
}

fn history(conv: &Conv) -> Value {
    Value::Array(conv.messages.iter().map(Message::to_json).collect())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.needs(&["sessions", "llm"]);
    august.define_event(
        "before",
        "Older messages are about to be summarised; return {cancel: true} or {summary}",
        json!({"type": "object", "required": ["reason", "messages", "kept"]}),
        false,
    );
    august.define_event("after", "Older messages were summarised (estimated tokens before and after)", json!({"type": "object"}), true);
    // Summarising a long conversation takes a while.
    august.hook_timeout("context", Duration::from_secs(600));
    let retry_at: RetryAt = Arc::default();

    let retry = retry_at.clone();
    august.on("context", move |data, ctx| {
        let retry = retry.clone();
        async move {
            let mut conv = Conv::of(&data, &data["messages"])?;
            // Refused as too long: compact whatever the estimate says.
            let reason = if data["error"]["kind"] == "context_too_long" { "overflow" } else { "threshold" };
            match compact(&ctx, &retry, &mut conv, reason).await {
                Ok(Some(_)) => Ok(Some(json!({"history": history(&conv), "note": "🗜 Older messages summarised to free up context"}))),
                Ok(None) => Ok(None),
                Err(e) => {
                    eprintln!("context compaction failed: {e:#}");
                    Ok(None)
                }
            }
        }
    });
    // Too long for the model after all: once more, compacted (see `context`).
    august.on("llm_error", |data, _| async move {
        let too_long = data["error"]["kind"] == "context_too_long" && data["attempt"] == 1;
        Ok(too_long.then(|| json!({"retry": true})))
    });
    let retry = retry_at.clone();
    august.on("session_changed", move |_, ctx| {
        retry.lock().unwrap().remove(&ctx.key());
        async { Ok(None) }
    });
    let retry = retry_at.clone();
    august.register_command("compact", "Summarise older messages to free up context", move |_, ctx| {
        let retry = retry.clone();
        async move {
            let live = ctx.call("messages", json!({})).await?;
            let mut conv = Conv::of(&live, &live["messages"])?;
            Ok(Some(match compact(&ctx, &retry, &mut conv, "manual").await {
                Ok(None) => "Nothing to compact yet.".into(),
                Ok(Some((before, after))) => {
                    ctx.call("messages_set", json!({"messages": history(&conv)})).await?;
                    format!("Compacted: ~{before} → ~{after} tokens.")
                }
                Err(e) => format!("Could not compact: {e:#}"),
            }))
        }
    });
    august.run().await;
}
