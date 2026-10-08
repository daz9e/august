//! Keeping the history inside the model's context window: trimming old tool
//! output and summarising older messages.

use super::Agent;
use crate::llm::{Block, Message, Role};
use anyhow::Result;

/// Estimated context size that triggers compaction (override: `AUGUST_CONTEXT_TOKENS`).
pub(super) const DEFAULT_CONTEXT_TOKENS: usize = 100_000;
/// Messages at the end of the history that compaction leaves untouched.
const KEEP_RECENT: usize = 8;
/// Older tool results longer than this are cut down to a head and a tail.
const PRUNE_OVER: usize = 2_000;
/// Rough cost of one image in the model's context.
const IMAGE_TOKENS: usize = 1_600;
/// Characters of the summarised transcript sent to the model at most.
const TRANSCRIPT_MAX: usize = 150_000;

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

/// Marks the summary message at the start of a compacted history.
const SUMMARY_MARK: &str = "[Summary of the earlier conversation]";

impl Agent {
    pub(super) fn estimate_tokens(&self) -> usize {
        // ~3 characters per token; characters, not bytes, so Cyrillic isn't overcounted.
        let chars: usize = self.history.iter().map(|m| m.content_json().chars().count()).sum();
        let images = self.history.iter().flat_map(|m| &m.content).filter(|b| matches!(b, Block::Image { .. })).count();
        let estimate = (chars + self.system.chars().count()) / 3 + images * IMAGE_TOKENS + 2_000; // + tool definitions
        estimate.max(self.last_input_tokens)
    }

    /// Frees up context when the history has grown past the limit (or `force`):
    /// first trims old tool output and images, then summarises older messages.
    /// Returns the estimated token count before and after, if anything changed.
    pub async fn compact(&mut self, force: bool) -> Result<Option<(usize, usize)>> {
        let before = self.estimate_tokens();
        if !force && before < self.context_limit {
            return Ok(None);
        }
        let mut changed = self.prune_old_blocks();
        if force || self.estimate_tokens() >= self.context_limit {
            changed |= self.summarise_old().await?;
        }
        if !changed {
            return Ok(None);
        }
        self.db.replace_live(&self.session, &self.history)?;
        self.stored = self.history.len();
        self.last_input_tokens = 0;
        self.snapshot = None; // the cached prefix is gone anyway; pick up new facts
        let after = self.estimate_tokens();
        self.log("compaction", serde_json::json!({"before": before, "after": after}), None);
        self.notify_ext("compaction", serde_json::json!({"before": before, "after": after}), self.chat_ref());
        Ok(Some((before, after)))
    }

    fn prune_old_blocks(&mut self) -> bool {
        let end = self.history.len().saturating_sub(KEEP_RECENT);
        let mut changed = false;
        for m in &mut self.history[..end] {
            for b in &mut m.content {
                // Old images stop being re-sent; the file stays in the workspace.
                if let Block::Image { path, .. } = b {
                    *b = Block::Text(format!("[image: {path}]"));
                    changed = true;
                    continue;
                }
                if let Block::ToolResult { content, .. } = b {
                    if content.len() > PRUNE_OVER {
                        let head: String = content.chars().take(1_200).collect();
                        let tail: Vec<char> = content.chars().rev().take(400).collect();
                        let tail: String = tail.into_iter().rev().collect();
                        *content = format!("{head}\n... [old output trimmed] ...\n{tail}");
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Index where the kept tail may start: an assistant message, or a user message
    /// that doesn't answer a tool call.
    fn cut_point(&self) -> Option<usize> {
        let mut i = self.history.len().checked_sub(KEEP_RECENT)?;
        while i >= 1 {
            let m = &self.history[i];
            let answers_tool = m.content.iter().any(|b| matches!(b, Block::ToolResult { .. }));
            if m.role == Role::Assistant || !answers_tool {
                return Some(i);
            }
            i -= 1;
        }
        None
    }

    async fn summarise_old(&mut self) -> Result<bool> {
        let Some(cut) = self.cut_point() else {
            return Ok(false);
        };
        if self.history.len() < self.compact_retry_at {
            return Ok(false);
        }
        // A summary from an earlier compaction is updated, not summarised again.
        let mut old = self.history[..cut].to_vec();
        let mut previous = None;
        if let Some(Block::Text(t)) = old.first_mut().and_then(|m| m.content.first_mut()) {
            if let Some(prev) = t.strip_prefix(SUMMARY_MARK) {
                previous = Some(prev.trim().to_string());
                t.clear();
            }
        }
        let transcript = transcript(&old);
        let ask = match previous {
            Some(p) => format!("Current summary:\n\n{p}\n\nNew transcript to fold into it:\n\n{transcript}"),
            None => format!("Transcript to summarise:\n\n{transcript}"),
        };
        let ask = vec![Message::user_text(ask)];
        let reply = self.provider.complete(&self.session, SUMMARY_SYSTEM, &ask, &[]).await;
        if let Ok(c) = &reply {
            self.record_usage(&c.usage);
        }
        let summary = match reply {
            Ok(c) if !c.message.text().trim().is_empty() => c.message.text(),
            Ok(_) => anyhow::bail!("the model returned an empty summary"),
            Err(e) => {
                self.compact_retry_at = self.history.len() + 10;
                return Err(e);
            }
        };
        let note = format!("{SUMMARY_MARK}\n{}", summary.trim());
        let mut tail = self.history.split_off(cut);
        let separate = tail[0].role != Role::User;
        if separate {
            tail.insert(0, Message::user_text(note));
        } else {
            tail[0].content.insert(0, Block::Text(note)); // keeps roles alternating
        }
        // Keep the rollback point on the same message (or at the start if it was summarised).
        self.turn_start = match self.turn_start.checked_sub(cut) {
            Some(n) => n + usize::from(separate),
            None => 0,
        };
        self.history = tail;
        if self.estimate_tokens() >= self.context_limit {
            // The kept tail alone is too big; don't pay for a summary on every step.
            self.compact_retry_at = self.history.len() + 4;
        }
        Ok(true)
    }
}

/// Plain-text rendering of messages for the summariser.
fn transcript(msgs: &[Message]) -> String {
    let clip = |s: &str, n: usize| -> String {
        if s.chars().count() > n {
            format!("{}…", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
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
    if text.len() > TRANSCRIPT_MAX {
        let mut from = text.len() - TRANSCRIPT_MAX;
        while !text.is_char_boundary(from) {
            from += 1;
        }
        format!("[earlier part omitted]\n{}", &text[from..])
    } else {
        text
    }
}
