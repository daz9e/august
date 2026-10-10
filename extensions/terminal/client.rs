//! The terminal window (`august` alone runs it): one thread of the `cli` messenger. August's
//! Markdown is drawn styled as it streams in; below it stay a live area (the line still
//! streaming, a spinner while August works, the open question's options, matching slash
//! commands) and the input line, with history and editing keys. Without a terminal (piped)
//! it reads lines and prints plain text.

use super::{ToAugust, ToWindow};
use anyhow::{Result, bail};
use august_ext::Button;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, queue, terminal};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const ACCENT: &str = "\x1b[38;5;141m";
const CODE: &str = "\x1b[38;5;180m";
const WARN: &str = "\x1b[38;5;209m";
const USER: &str = "\x1b[48;5;236m\x1b[38;5;252m";
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A line as drawn: with its styling, and how many columns it takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub styled: String,
    pub width: usize,
}

impl Line {
    fn new(styled: String, plain: &str) -> Line {
        Line { styled, width: plain.width() }
    }

    fn plain(text: &str) -> Line {
        Line::new(text.to_string(), text)
    }

    fn blank() -> Line {
        Line::plain("")
    }

    /// Screen rows it takes `cols` wide.
    fn rows(&self, cols: usize) -> usize {
        self.width.max(1).div_ceil(cols.max(1))
    }
}

/// Builds a styled line, measuring what it adds.
#[derive(Default)]
struct Styled {
    styled: String,
    plain: String,
}

impl Styled {
    fn push(&mut self, style: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if style.is_empty() {
            self.styled.push_str(text);
        } else {
            self.styled.push_str(&format!("{style}{text}{RESET}"));
        }
        self.plain.push_str(text);
    }

    fn line(self) -> Line {
        Line::new(self.styled, &self.plain)
    }
}

/// Inline Markdown: `code`, **bold**, *italic*, [links](url).
fn inline(out: &mut Styled, text: &str, base: &str) {
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0;
    let flush = |out: &mut Styled, plain: &mut String| out.push(base, &std::mem::take(plain));
    let find = |from: usize, pat: &str| -> Option<usize> {
        let pat: Vec<char> = pat.chars().collect();
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == pat[..])
    };
    while i < chars.len() {
        let c = chars[i];
        let rest_starts = |p: &str| chars[i..].iter().collect::<String>().starts_with(p);
        if c == '`'
            && let Some(end) = find(i + 1, "`")
        {
            flush(out, &mut plain);
            out.push(CODE, &chars[i + 1..end].iter().collect::<String>());
            i = end + 1;
        } else if (rest_starts("**") || rest_starts("__"))
            && let Some(end) = find(i + 2, &chars[i..i + 2].iter().collect::<String>())
            && end > i + 2
        {
            flush(out, &mut plain);
            inline(out, &chars[i + 2..end].iter().collect::<String>(), &format!("{base}{BOLD}"));
            i = end + 2;
        } else if (c == '*' || c == '_')
            && chars.get(i + 1).is_some_and(|n| !n.is_whitespace())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(end) = find(i + 1, &c.to_string())
            && end > i + 1
        {
            flush(out, &mut plain);
            inline(out, &chars[i + 1..end].iter().collect::<String>(), &format!("{base}{ITALIC}"));
            i = end + 1;
        } else if c == '['
            && let Some(close) = find(i + 1, "](")
            && let Some(end) = find(close + 2, ")")
        {
            flush(out, &mut plain);
            let label: String = chars[i + 1..close].iter().collect();
            let url: String = chars[close + 2..end].iter().collect();
            out.push(&format!("{base}{UNDERLINE}"), &label);
            if url != label {
                out.push(DIM, &format!(" ({url})"));
            }
            i = end + 1;
        } else {
            plain.push(c);
            i += 1;
        }
    }
    flush(out, &mut plain);
}

/// One line of August's Markdown, indented under `lead` (`● ` on a message's first line);
/// `code` tells whether it is inside a fenced block, and is updated past a fence.
pub fn markdown_line(text: &str, lead: &str, code: &mut bool) -> Line {
    let mut out = Styled::default();
    out.push(ACCENT, lead);
    let trimmed = text.trim_start();
    if let Some(lang) = trimmed.strip_prefix("```") {
        *code = !*code;
        let label = if *code && !lang.trim().is_empty() { format!("── {} ", lang.trim()) } else { "──".into() };
        out.push(DIM, &label);
        return out.line();
    }
    if *code {
        out.push(DIM, "│ ");
        out.push(CODE, text);
        return out.line();
    }
    let indent = &text[..text.len() - trimmed.len()];
    let heading = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&heading) && trimmed[heading..].starts_with(' ') {
        inline(&mut out, trimmed[heading..].trim(), &format!("{BOLD}{ACCENT}"));
    } else if trimmed.starts_with("> ") || trimmed == ">" {
        out.push(DIM, "▎ ");
        inline(&mut out, trimmed.trim_start_matches('>').trim_start(), &format!("{DIM}{ITALIC}"));
    } else if ["---", "***", "___"].contains(&trimmed) {
        out.push(DIM, "────────────────────────");
    } else if let Some(item) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
        out.push("", indent);
        out.push(ACCENT, "• ");
        inline(&mut out, item, "");
    } else if trimmed.starts_with("🔧") {
        // A tool call, as `render` shows it.
        out.push(DIM, &trimmed.replace('`', ""));
    } else if trimmed.starts_with("⚠️") || trimmed.starts_with("⏹") {
        out.push(WARN, trimmed);
    } else {
        inline(&mut out, text, "");
    }
    out.line()
}

/// What the user typed, as it stays on screen.
fn user_line(text: &str) -> Line {
    let mut out = Styled::default();
    out.push(USER, &format!(" › {text} "));
    out.line()
}

/// A message August sent, and how much of it is drawn for good.
struct Message {
    text: String,
    /// Bytes of `text` drawn for good: whole lines.
    done: usize,
    /// Lines drawn for good.
    lines: usize,
    /// Inside a fenced code block after them.
    code: bool,
    /// It came with buttons (a question).
    asked: bool,
}

/// What the window shows.
pub struct Screen {
    /// A real terminal: styled, with the live area; else plain lines.
    tty: bool,
    /// Lines to draw for good above the live area, at the next `draw`.
    pending: Vec<Line>,
    messages: HashMap<String, Message>,
    /// The message still streaming in (its unfinished line is live).
    current: Option<String>,
    /// Questions waiting for an answer, oldest first.
    open: VecDeque<(String, Vec<Button>)>,
    pub busy: Option<Instant>,
    /// The last line drawn for good was blank.
    spaced: bool,
    /// Slash commands, for completion: `(name, description)`.
    pub commands: Vec<(String, String)>,
    pub input: Input,
}

impl Screen {
    pub fn new(tty: bool) -> Screen {
        Screen {
            tty,
            pending: Vec::new(),
            messages: HashMap::new(),
            current: None,
            open: VecDeque::new(),
            busy: None,
            spaced: true,
            commands: Vec::new(),
            input: Input::default(),
        }
    }

    /// Draws `line` for good.
    pub fn print(&mut self, line: Line) {
        self.spaced = line.width == 0;
        self.pending.push(line);
    }

    fn space(&mut self) {
        if !self.spaced {
            self.print(Line::blank());
        }
    }

    fn line(&self, text: &str, lead: &str, code: &mut bool) -> Line {
        if self.tty { markdown_line(text, lead, code) } else { Line::plain(&format!("{}{text}", " ".repeat(lead.width()))) }
    }

    /// Draws the whole lines of message `id` not drawn yet; with `all`, its last line too.
    fn advance(&mut self, id: &str, all: bool) {
        let Some(mut m) = self.messages.remove(id) else { return };
        let end = if all { m.text.len() } else { m.text.rfind('\n').map(|i| i + 1).unwrap_or(0) };
        if end > m.done {
            let chunk = m.text[m.done..end].to_string();
            for text in chunk.strip_suffix('\n').unwrap_or(&chunk).split('\n') {
                let lead = if m.lines == 0 { "● " } else { "  " };
                let line = self.line(text, lead, &mut m.code);
                self.print(line);
                m.lines += 1;
            }
            m.done = end;
        }
        self.messages.insert(id.to_string(), m);
    }

    /// Draws for good what is left of the message streaming in.
    fn finish_current(&mut self) {
        if let Some(id) = self.current.take() {
            self.advance(&id, true);
        }
    }

    pub fn show(&mut self, msg: ToWindow) {
        match msg {
            ToWindow::Hello { .. } => {}
            ToWindow::Busy => {
                self.busy.get_or_insert_with(Instant::now);
            }
            ToWindow::Idle => {
                self.finish_current();
                self.busy = None;
            }
            ToWindow::Send { id, text, buttons, files } => {
                self.finish_current();
                self.space();
                let buttons: Vec<Button> = buttons.into_iter().flatten().collect();
                let asked = !buttons.is_empty();
                if asked {
                    self.open.push_back((id.clone(), buttons));
                }
                let files: Vec<String> = files.iter().map(|f| format!("📎 {f}")).collect();
                let text = [text, files.join("\n")].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
                self.messages.insert(id.clone(), Message { text, done: 0, lines: 0, code: false, asked });
                self.current = Some(id.clone());
                self.advance(&id, false);
            }
            ToWindow::Edit { id, text, buttons } => self.edit(id, text, buttons.into_iter().flatten().collect()),
        }
    }

    fn edit(&mut self, id: String, text: String, buttons: Vec<Button>) {
        // An edit without buttons settles a question (answered or timed out); one with
        // buttons asks (again).
        self.open.retain(|(open, _)| *open != id);
        let settled = buttons.is_empty();
        if !settled {
            self.open.push_back((id.clone(), buttons));
        }
        let Some(m) = self.messages.get_mut(&id) else { return };
        let old = std::mem::replace(&mut m.text, text.clone());
        let grew = text.starts_with(&old[..m.done]);
        if self.current.as_deref() == Some(id.as_str()) && grew {
            return self.advance(&id, false);
        }
        if m.asked && settled {
            // A settled question shows its outcome: what was added, else the new first line.
            let added = text.strip_prefix(old.as_str()).map(str::trim).filter(|a| !a.is_empty());
            let outcome = added.unwrap_or_else(|| text.lines().next().unwrap_or_default()).to_string();
            m.done = text.len();
            let line = if self.tty { markdown_line(&outcome, "  ", &mut false) } else { Line::plain(&format!("  {outcome}")) };
            return self.print(line);
        }
        // Rewritten: drawn again from where it differs (whole lines already drawn stay).
        let same = old.bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count().min(m.done);
        let start = text[..same].rfind('\n').map(|i| i + 1).unwrap_or(0);
        m.done = start;
        m.lines = text[..start].matches('\n').count();
        if self.current.as_deref() != Some(id.as_str()) {
            return self.advance(&id, true);
        }
        self.advance(&id, false);
    }

    /// The lines below what is drawn for good, above the input: the unfinished line, the
    /// spinner, the open question's options, matching commands.
    fn live(&self, now: Instant) -> Vec<Line> {
        let mut live = Vec::new();
        if let Some(m) = self.current.as_ref().and_then(|id| self.messages.get(id)) {
            let tail = &m.text[m.done..];
            if !tail.is_empty() {
                let lead = if m.lines == 0 { "● " } else { "  " };
                live.push(markdown_line(tail, lead, &mut m.code.clone()));
            }
        }
        if let Some(since) = self.busy {
            let frame = SPINNER[(now.duration_since(since).as_millis() / 80) as usize % SPINNER.len()];
            let mut s = Styled::default();
            s.push(ACCENT, &format!("{frame} "));
            s.push(DIM, &format!("working · {}s · esc to stop", since.elapsed().as_secs()));
            live.push(s.line());
        }
        if let Some((_, buttons)) = self.open.front() {
            let mut s = Styled::default();
            s.push(DIM, "  ");
            for (i, b) in buttons.iter().enumerate() {
                s.push(&format!("{BOLD}{ACCENT}"), &format!("{}", i + 1));
                s.push("", &format!(" {}   ", b.label));
            }
            s.push(DIM, "type a number");
            live.push(s.line());
        }
        let typed = self.input.text();
        if typed.starts_with('/') && !typed.contains(' ') {
            for (name, about) in self.commands.iter().filter(|(n, _)| format!("/{n}").starts_with(&typed)).take(6) {
                let mut s = Styled::default();
                s.push(ACCENT, &format!("  /{name:<14}"));
                s.push(DIM, about);
                live.push(s.line());
            }
        }
        live
    }

    /// What a typed line means: a number presses a button of the oldest open question.
    pub fn submit(&mut self, line: &str) -> ToAugust {
        let pick = self.open.front().and_then(|(_, buttons)| buttons.get(line.parse::<usize>().ok()?.checked_sub(1)?)).cloned();
        self.finish_current();
        self.space();
        if let Some(b) = pick {
            self.open.pop_front();
            self.print(user_line(&b.label));
            return ToAugust::Press { button: b.id };
        }
        self.print(if self.tty { user_line(line) } else { Line::plain(&format!("> {line}")) });
        self.busy.get_or_insert_with(Instant::now);
        ToAugust::Text { text: line.to_string(), reply_to: None }
    }

    /// Completes a slash command being typed.
    fn complete(&mut self) {
        let typed = self.input.text();
        let names: Vec<String> = self.commands.iter().map(|(n, _)| format!("/{n}")).filter(|n| n.starts_with(&typed)).collect();
        let Some(first) = names.first() else { return };
        let common = names.iter().fold(first.clone(), |acc, n| acc.chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect());
        let done = if names.len() == 1 { format!("{common} ") } else { common };
        if done.len() > typed.len() {
            self.input.set(&done);
        }
    }

    /// Takes the lines to draw for good (plain mode prints them as they come).
    pub fn take_pending(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.pending)
    }
}

/// The line being typed, with its history.
#[derive(Default)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Where in the history Up/Down is (`history.len()`: the line being typed).
    back: usize,
    draft: String,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.back = self.history.len();
        self.set("");
        text
    }

    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn recall(&mut self, up: bool) {
        if self.back == self.history.len() {
            self.draft = self.text();
        }
        let back = if up { self.back.checked_sub(1) } else { Some(self.back + 1).filter(|b| *b <= self.history.len()) };
        let Some(back) = back else { return };
        self.back = back;
        let text = self.history.get(back).cloned().unwrap_or_else(|| self.draft.clone());
        self.set(&text);
    }

    /// Applies an editing key; `true` if it was one.
    fn key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.chars.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                let start = self.word_start();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Backspace if alt => {
                let start = self.word_start();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Enter if alt => self.insert("\n"),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Up => self.recall(true),
            KeyCode::Down => self.recall(false),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// The prompt lines, and the cursor's `(line, column)` among them.
    fn lines(&self) -> (Vec<Line>, (usize, usize)) {
        let before: String = self.chars[..self.cursor].iter().collect();
        let row = before.matches('\n').count();
        let col = 2 + before.rsplit('\n').next().unwrap_or_default().width();
        let lines = self
            .text()
            .split('\n')
            .enumerate()
            .map(|(i, text)| {
                let mut s = Styled::default();
                s.push(&format!("{BOLD}{ACCENT}"), if i == 0 { "❯ " } else { "  " });
                s.push("", text);
                s.line()
            })
            .collect();
        (lines, (row, col))
    }
}

/// Draws a `Screen` on a real terminal: what is drawn for good scrolls up; the live area
/// and the input are redrawn in place below it.
struct Painter {
    /// Rows from the top of the live area to the cursor.
    cursor_row: usize,
}

impl Painter {
    fn draw(&mut self, screen: &mut Screen) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80).max(10);
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveUp(self.cursor_row as u16))?;
        }
        write!(out, "\r")?;
        queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
        for line in screen.take_pending() {
            write!(out, "{}\r\n", line.styled)?;
        }
        let mut live = screen.live(Instant::now());
        let (input, (row, col)) = screen.input.lines();
        let above: usize = live.iter().map(|l| l.rows(cols)).sum::<usize>() + input[..row].iter().map(|l| l.rows(cols)).sum::<usize>();
        live.extend(input);
        let total: usize = live.iter().map(|l| l.rows(cols)).sum();
        let shown: Vec<&str> = live.iter().map(|l| l.styled.as_str()).collect();
        write!(out, "{}", shown.join("\r\n"))?;
        // From the end of what was written to the cursor.
        if total > 1 {
            queue!(out, cursor::MoveUp((total - 1) as u16))?;
        }
        write!(out, "\r")?;
        self.cursor_row = above + col / cols;
        if self.cursor_row > 0 {
            queue!(out, cursor::MoveDown(self.cursor_row as u16))?;
        }
        queue!(out, cursor::MoveToColumn((col % cols) as u16))?;
        out.flush()
    }
}

/// Leaves the terminal as it was, however the window ends.
struct Raw;

impl Raw {
    fn on() -> std::io::Result<Raw> {
        terminal::enable_raw_mode()?;
        queue!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        Ok(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        queue!(std::io::stdout(), crossterm::event::DisableBracketedPaste).ok();
        terminal::disable_raw_mode().ok();
        println!();
    }
}

/// What the window knows from the core (`AUGUST_SOCKET`): `(model, workspace, commands)`.
async fn about() -> Option<(String, String, Vec<(String, String)>)> {
    let mut core = august_ext::client::Client::from_env().await.ok()?;
    let status = core.call("status", json!({})).await.ok()?;
    let model = match (status["provider"].as_str(), status["model"].as_str()) {
        (Some(p), Some(m)) if !p.is_empty() => format!("{p} · {m}"),
        _ => "no model yet · /login".into(),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let workspace = status["workspace"].as_str().unwrap_or_default();
    let workspace = match workspace.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => workspace.to_string(),
    };
    let commands = core.call("commands", json!({})).await.unwrap_or_default();
    let commands = commands.as_array().into_iter().flatten();
    let commands = commands.map(|c| (c["name"].as_str().unwrap_or_default().to_string(), c["description"].as_str().unwrap_or_default().to_string()));
    Some((model, workspace, commands.collect()))
}

/// A window on the terminal socket at `socket`; `first` is sent first, as if typed.
pub async fn run(socket: &Path, first: Option<&str>) -> Result<()> {
    let stream = UnixStream::connect(socket).await.map_err(|e| anyhow::anyhow!("August's terminal is not listening at {}: {e}", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let send = |msg: &ToAugust| serde_json::to_string(msg).unwrap_or_default() + "\n";
    write.write_all(send(&ToAugust::Hello).as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    let thread = match lines.next_line().await?.map(|l| serde_json::from_str::<ToWindow>(&l)) {
        Some(Ok(ToWindow::Hello { thread })) => thread,
        _ => bail!("August did not greet this terminal"),
    };
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut screen = Screen::new(tty);
    let (model, workspace, commands) = about().await.unwrap_or_default();
    screen.commands = commands;
    let mut head = Styled::default();
    head.push(&format!("{BOLD}{ACCENT}"), "◆ august");
    head.push(DIM, &format!("  terminal {thread}"));
    if !model.is_empty() {
        head.push(DIM, &format!(" · {model} · {workspace}"));
    }
    screen.print(if tty { head.line() } else { Line::plain(&format!("august · terminal {thread}")) });
    let mut hint = Styled::default();
    hint.push(DIM, "  /help commands · esc stops August · ctrl-d quits · alt-enter new line");
    screen.print(if tty { hint.line() } else { Line::plain("/help lists commands, /exit quits.") });
    screen.print(Line::blank());
    if let Some(line) = first.filter(|l| !l.trim().is_empty()) {
        let msg = screen.submit(line.trim());
        write.write_all(send(&msg).as_bytes()).await?;
    }

    let (from_august, mut august) = mpsc::unbounded_channel::<Option<ToWindow>>();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(msg) = serde_json::from_str::<ToWindow>(&line) {
                from_august.send(Some(msg)).ok();
            }
        }
        from_august.send(None).ok();
    });
    if !tty {
        return plain(screen, august, write).await;
    }

    let _raw = Raw::on()?;
    let (keys_tx, mut keys) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if keys_tx.send(ev).is_err() {
                break;
            }
        }
    });
    let mut painter = Painter { cursor_row: 0 };
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    loop {
        painter.draw(&mut screen)?;
        tokio::select! {
            msg = august.recv() => match msg.flatten() {
                Some(msg) => screen.show(msg),
                None => {
                    screen.finish_current();
                    screen.busy = None;
                    let mut s = Styled::default();
                    s.push(WARN, "August stopped.");
                    screen.print(s.line());
                    painter.draw(&mut screen)?;
                    return Ok(());
                }
            },
            _ = tick.tick(), if screen.busy.is_some() => {}
            ev = keys.recv() => {
                let Some(ev) = ev else { return Ok(()) };
                let key = match ev {
                    Event::Key(key) if key.kind != KeyEventKind::Release => key,
                    Event::Paste(text) => {
                        screen.input.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                        continue;
                    }
                    _ => continue,
                };
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
                        let line = screen.input.take();
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        if line == "/exit" || line == "/quit" {
                            return Ok(());
                        }
                        let msg = screen.submit(line);
                        write.write_all(send(&msg).as_bytes()).await?;
                    }
                    KeyCode::Tab => screen.complete(),
                    KeyCode::Esc if screen.busy.is_some() => {
                        write.write_all(send(&ToAugust::Text { text: "/stop".into(), reply_to: None }).as_bytes()).await?;
                    }
                    KeyCode::Char('c') if ctrl => {
                        if !screen.input.text().is_empty() {
                            screen.input.set("");
                        } else if screen.busy.is_some() {
                            write.write_all(send(&ToAugust::Text { text: "/stop".into(), reply_to: None }).as_bytes()).await?;
                        } else {
                            return Ok(());
                        }
                    }
                    KeyCode::Char('d') if ctrl && screen.input.text().is_empty() => return Ok(()),
                    KeyCode::Char('l') if ctrl => {
                        queue!(std::io::stdout(), terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0))?;
                        painter.cursor_row = 0;
                    }
                    _ => {
                        screen.input.key(&key);
                    }
                }
            }
        }
    }
}

/// Without a terminal: lines in, plain text out.
async fn plain(mut screen: Screen, mut august: mpsc::UnboundedReceiver<Option<ToWindow>>, mut write: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    let flush = |screen: &mut Screen| {
        let mut out = std::io::stdout().lock();
        for line in screen.take_pending() {
            writeln!(out, "{}", line.styled).ok();
        }
        out.flush().ok();
    };
    flush(&mut screen);
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    // Input ended: the window closes once August has answered what was sent.
    let mut ended = false;
    loop {
        if ended && screen.busy.is_none() {
            return Ok(());
        }
        tokio::select! {
            msg = august.recv() => match msg.flatten() {
                Some(msg) => screen.show(msg),
                None => {
                    screen.finish_current();
                    screen.print(Line::plain("August stopped."));
                    flush(&mut screen);
                    return Ok(());
                }
            },
            line = input.next_line(), if !ended => {
                let Some(line) = line? else {
                    ended = true;
                    continue;
                };
                let line = line.trim();
                match line {
                    "" => continue,
                    "/exit" | "/quit" => return Ok(()),
                    _ => {
                        let msg = screen.submit(line);
                        write.write_all((serde_json::to_string(&msg)? + "\n").as_bytes()).await?;
                    }
                }
            }
        }
        flush(&mut screen);
    }
}

