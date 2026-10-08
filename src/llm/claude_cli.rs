//! Claude subscription through the locally installed Claude Code CLI (`claude -p`).
//!
//! Each completion runs one `claude` process with its built-in tools, MCP servers,
//! skills and session files turned off, so it makes exactly one model call. The
//! conversation goes in as a transcript; August's tools are described in the system
//! prompt and called with `<tool_call>` text blocks that this provider turns back
//! into `ToolUse`, so every tool still runs through August (hooks, rendering,
//! extensions). The CLI keeps no state between calls.

use super::*;
use anyhow::{Context, bail};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";

/// `AUGUST_CLAUDE_BIN`, else `claude` from PATH.
pub fn bin() -> String {
    std::env::var("AUGUST_CLAUDE_BIN").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "claude".into())
}

/// Fails unless the CLI can be started.
pub async fn check_installed() -> Result<()> {
    let out = tokio::process::Command::new(bin())
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("`{}` not found: install Claude Code or set AUGUST_CLAUDE_BIN", bin()))?;
    if !out.status.success() {
        bail!("`{} --version` failed: {}", bin(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub struct ClaudeCli {
    pub model: Option<String>,
    pub effort: String,
}

#[async_trait]
impl LlmProvider for ClaudeCli {
    fn context_window(&self) -> Option<usize> {
        Some(200_000)
    }

    fn name(&self) -> &str {
        "claude-cli"
    }

    async fn complete(&self, session: &str, system: &str, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion> {
        self.complete_stream(session, system, messages, tools, &mut |_| {}).await
    }

    async fn complete_stream(
        &self,
        _session: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Completion> {
        let mut cmd = tokio::process::Command::new(bin());
        cmd.args(["-p", "--verbose", "--input-format", "stream-json", "--output-format", "stream-json"])
            .args(["--include-partial-messages", "--no-session-persistence", "--strict-mcp-config"])
            .args(["--disable-slash-commands", "--setting-sources", "", "--tools", ""])
            .arg("--system-prompt")
            .arg(system_prompt(system, tools));
        if let Some(m) = &self.model {
            cmd.args(["--model", m]);
        }
        if ["low", "medium", "high", "xhigh", "max"].contains(&self.effort.as_str()) {
            cmd.args(["--effort", &self.effort]);
        }
        // Not the caller's directory: keeps project CLAUDE.md files out of the prompt.
        cmd.current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("`{}` not found: install Claude Code or set AUGUST_CLAUDE_BIN", bin()))?;

        let input = json!({"type": "user", "message": {"role": "user", "content": transcript(messages)}});
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(format!("{input}\n").as_bytes()).await?;
        drop(stdin);
        let mut stderr = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut s = String::new();
            stderr.read_to_string(&mut s).await.ok();
            s
        });

        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut text = String::new();
        let mut shown = 0;
        let mut result = None;
        while let Some(line) = lines.next_line().await? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match v["type"].as_str() {
                Some("stream_event") if v["event"]["delta"]["type"] == "text_delta" => {
                    text.push_str(v["event"]["delta"]["text"].as_str().unwrap_or(""));
                    let safe = visible_len(&text);
                    if safe > shown {
                        on_text(&text[shown..safe]);
                        shown = safe;
                    }
                }
                Some("result") => result = Some(v),
                _ => {}
            }
        }
        let status = child.wait().await?;
        let stderr = stderr.await.unwrap_or_default();
        let Some(r) = result else {
            bail!("{} exited ({status}) without a result: {}", bin(), stderr.trim());
        };
        if r["is_error"].as_bool().unwrap_or(false) {
            let msg = r["result"].as_str().unwrap_or("unknown error");
            match r["api_error_status"].as_u64() {
                Some(code) => {
                    let kind = super::error::ErrorKind::of_http(code as u16, msg);
                    return Err(super::error::ProviderError { kind, message: format!("claude: HTTP {code}: {msg}") }.into());
                }
                None => bail!("claude: {msg}"),
            }
        }
        let mut full = if text.is_empty() { r["result"].as_str().unwrap_or("").to_string() } else { text };
        // Anything after a made-up tool result is the model talking to itself.
        if let Some(i) = full.find("<tool_result") {
            full.truncate(i);
        }
        let visible = full[..visible_len(&full)].to_string();
        if visible.len() > shown {
            on_text(&visible[shown..]);
        }

        let mut content = Vec::new();
        if !visible.trim().is_empty() {
            content.push(Block::Text(visible.trim_end().to_string()));
        }
        content.extend(tool_calls(&full));
        let u = &r["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let stop_reason = if content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
            StopReason::ToolUse
        } else {
            match r["stop_reason"].as_str() {
                Some("max_tokens") => StopReason::MaxTokens,
                Some("refusal") => StopReason::Refusal,
                Some("end_turn") | None => StopReason::EndTurn,
                Some(other) => StopReason::Other(other.into()),
            }
        };
        Ok(Completion {
            message: Message { role: Role::Assistant, content },
            stop_reason,
            usage: Usage {
                input_tokens: n("input_tokens"),
                output_tokens: n("output_tokens"),
                cache_read_tokens: n("cache_read_input_tokens"),
                cache_write_tokens: n("cache_creation_input_tokens"),
            },
        })
    }
}

fn system_prompt(system: &str, tools: &[ToolSpec]) -> String {
    let mut s = format!(
        "{system}\n\n# Conversation format\nThe conversation so far arrives as a transcript of <user> and \
         <assistant> turns. Write only the assistant's next message, without any tags around it."
    );
    if tools.is_empty() {
        return s;
    }
    s.push_str(&format!(
        "\n\n# Tools\nYou can call the tools listed below, and no others. To call one, write a single \
         JSON object with the keys \"name\" and \"input\" (the arguments, matching the tool's input schema) \
         between tags, like this:\n\
         {CALL_OPEN}{{\"name\": \"some_tool\", \"input\": {{\"arg\": \"value\"}}}}{CALL_CLOSE}\n\
         Several calls in one message run in parallel. After your calls, stop: the results arrive in \
         the next user turn as <tool_result> blocks, in call order. Never write <tool_result> yourself, \
         never claim a tool ran before you see its result, and don't repeat a call whose result you \
         already have.\n<tools>\n"
    ));
    for t in tools {
        s.push_str(&json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}).to_string());
        s.push('\n');
    }
    s.push_str("</tools>");
    s
}

/// The conversation as one user message: transcript text with images in place.
fn transcript(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut text = String::new();
    for m in messages {
        let role = m.role.as_str();
        text.push_str(&format!("<{role}>\n"));
        for b in &m.content {
            match b {
                Block::Text(t) => text.push_str(&format!("{t}\n")),
                Block::ToolUse { name, input, .. } => {
                    text.push_str(&format!("{CALL_OPEN}{}{CALL_CLOSE}\n", json!({"name": name, "input": input})))
                }
                Block::ToolResult { content, is_error, .. } => {
                    let err = if *is_error { " error=\"true\"" } else { "" };
                    text.push_str(&format!("<tool_result{err}>\n{content}\n</tool_result>\n"));
                }
                Block::Image { media_type, path } => match image_base64(path) {
                    Some(data) => {
                        out.push(json!({"type": "text", "text": std::mem::take(&mut text)}));
                        out.push(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}));
                    }
                    None => text.push_str(&format!("{}\n", missing_image(path))),
                },
                Block::Opaque(_) => {}
            }
        }
        text.push_str(&format!("</{role}>\n"));
        // One block per message: earlier blocks stay byte-identical across calls, so
        // the CLI's prompt cache keeps hitting as the conversation grows.
        out.push(json!({"type": "text", "text": std::mem::take(&mut text)}));
    }
    out
}

/// How much of `text` is plain reply: everything before the first tool call, minus a
/// tail that could be the start of one.
fn visible_len(text: &str) -> usize {
    if let Some(i) = text.find(CALL_OPEN) {
        return i;
    }
    (1..CALL_OPEN.len())
        .rev()
        .find(|&k| text.ends_with(&CALL_OPEN[..k]))
        .map_or(text.len(), |k| text.len() - k)
}

/// `<tool_call>` blocks in a reply. A call that isn't valid JSON still becomes a
/// `ToolUse` (named `invalid_tool_call`) so the model gets an error back and can retry.
fn tool_calls(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(CALL_OPEN) {
        let after = &rest[start + CALL_OPEN.len()..];
        let Some(end) = after.find(CALL_CLOSE) else { break };
        let body = after[..end].trim();
        // Lenient: ignore trailing junk after the object, and accept arguments written
        // next to `name` instead of under `input`.
        let parsed = serde_json::Deserializer::from_str(body).into_iter::<Value>().next();
        let (name, input) = match parsed {
            Some(Ok(Value::Object(mut v))) if v.get("name").is_some_and(Value::is_string) => {
                let name = v.remove("name").unwrap().as_str().unwrap().to_string();
                let input = match v.remove("input") {
                    Some(i @ Value::Object(_)) => i,
                    _ => Value::Object(v),
                };
                (name, input)
            }
            _ => ("invalid_tool_call".to_string(), json!({"raw": body})),
        };
        out.push(Block::ToolUse { id: format!("call_{}", crate::util::new_uuid()), name, input });
        rest = &after[end + CALL_CLOSE.len()..];
    }
    out
}
