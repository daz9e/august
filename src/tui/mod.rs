//! The terminal UI. Output goes into the terminal's own scrollback (so scrolling, search and
//! copy work as usual); a small panel pinned below it shows what the agent is doing, the
//! input box and a status line. You can keep typing while the agent works: messages are fed
//! into the running turn.

mod editor;
mod render;

use crate::agent::{self, App, Cancelled, Session};
use crate::channel::{Channel, Decision, Fut, Input};
use crate::commands;
use crate::config::{self, Config};
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use editor::Editor;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use render::{ACCENT, DIM};
use std::io::{Stdout, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

const SESSION: &str = "cli";
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_INPUT_ROWS: usize = 8;
const GREEN: Color = Color::Rgb(120, 190, 120);
const RED: Color = Color::Rgb(220, 100, 100);

fn commands_list() -> Vec<(&'static str, &'static str)> {
    let mut list = commands::LIST.to_vec();
    list.push(("exit", "Quit"));
    list
}

// ---------- state shared with the running turn ----------

#[derive(Default, PartialEq, Clone, Copy)]
enum Kind {
    #[default]
    Start,
    Blank,
    User,
    Text,
    Tool,
    Note,
}

struct Approval {
    command: String,
    reason: String,
    tx: oneshot::Sender<Decision>,
}

#[derive(Default)]
struct Shared {
    /// Lines waiting to be printed above the panel.
    out: Vec<Line<'static>>,
    last: Kind,
    /// The assistant's current text segment and how many of its rendered lines are printed.
    stream: String,
    emitted: usize,
    streamed: bool,
    activity: Option<String>,
    queued: Vec<String>,
    approval: Option<Approval>,
}

impl Shared {
    fn gap(&mut self) {
        if !matches!(self.last, Kind::Start | Kind::Blank) {
            self.out.push(Line::default());
            self.last = Kind::Blank;
        }
    }

    fn block(&mut self, kind: Kind, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        if self.last != kind || matches!(kind, Kind::User | Kind::Note) {
            self.gap();
        }
        self.out.extend(lines);
        self.last = kind;
    }

    fn user(&mut self, text: &str) {
        let style = Style::new().add_modifier(Modifier::BOLD);
        let lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let mark = if i == 0 { Span::styled("› ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
                Line::from(vec![mark, Span::styled(l.to_string(), style)])
            })
            .collect();
        self.block(Kind::User, lines);
    }

    fn note(&mut self, text: &str, style: Style) {
        let lines = text.lines().map(|l| Line::styled(l.to_string(), style)).collect();
        self.block(Kind::Note, lines);
    }

    /// Prints the finished lines of the streamed text (`all` also prints the unfinished one).
    fn emit(&mut self, all: bool) {
        let text = if all {
            self.stream.clone()
        } else {
            match self.stream.rfind('\n') {
                Some(i) => self.stream[..i].to_string(),
                None => return,
            }
        };
        // A table only renders once its rows are complete.
        if !all && text.lines().last().is_some_and(|l| l.trim_start().starts_with('|')) {
            return;
        }
        let lines = render::markdown(&text);
        if lines.len() > self.emitted {
            let new = lines[self.emitted..].to_vec();
            self.emitted = lines.len();
            if self.last == Kind::Text {
                self.out.extend(new);
            } else {
                self.block(Kind::Text, new);
            }
        }
        if all {
            self.stream.clear();
            self.emitted = 0;
        }
    }

    fn tool_lines(&mut self, summary: &str, output: &str, is_error: bool) {
        let (name, arg) = summary.split_once(": ").unwrap_or((summary, ""));
        let dot = Span::styled("● ", Style::new().fg(if is_error { RED } else { GREEN }));
        let mut lines = vec![Line::from(vec![dot, Span::styled(name.to_string(), Style::new().add_modifier(Modifier::BOLD)), Span::raw(format!(" {arg}"))])];
        let out: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        let style = if is_error { Style::new().fg(RED) } else { DIM };
        let shown: Vec<String> = match name {
            _ if is_error => out.iter().take(3).map(|s| s.to_string()).collect(),
            "bash" | "$" => out.iter().take(if name == "$" { 8 } else { 3 }).map(|s| s.to_string()).collect(),
            "read" => vec![format!("{} lines", out.len())],
            "web_search" => Vec::new(),
            _ => out.iter().take(1).map(|s| s.to_string()).collect(),
        };
        for (i, l) in shown.iter().enumerate() {
            let mark = if i == 0 { "  ⎿ " } else { "    " };
            let l: String = l.chars().take(200).collect();
            lines.push(Line::styled(format!("{mark}{l}"), style));
        }
        if matches!(name, "bash" | "$") && out.len() > shown.len() {
            lines.push(Line::styled(format!("    … +{} lines", out.len() - shown.len()), DIM));
        }
        self.block(Kind::Tool, lines);
    }
}

struct TuiChannel {
    shared: Arc<Mutex<Shared>>,
    wake: Arc<Notify>,
}

impl Channel for TuiChannel {
    fn text(&self, delta: &str) {
        let mut s = self.shared.lock().unwrap();
        s.stream.push_str(delta);
        s.streamed = true;
        if delta.contains('\n') {
            s.emit(false);
        }
        self.wake.notify_one();
    }

    fn tool(&self, summary: &str) {
        let mut s = self.shared.lock().unwrap();
        s.emit(true);
        s.activity = Some(summary.to_string());
        self.wake.notify_one();
    }

    fn tool_done(&self, output: &str, is_error: bool) {
        let mut s = self.shared.lock().unwrap();
        let summary = s.activity.take().unwrap_or_default();
        s.tool_lines(&summary, output, is_error);
        self.wake.notify_one();
    }

    fn steer(&self) -> Vec<Input> {
        let mut s = self.shared.lock().unwrap();
        let queued = std::mem::take(&mut s.queued);
        for q in &queued {
            s.user(q);
        }
        queued.into_iter().map(Input::text).collect()
    }

    fn approve<'a>(&'a self, command: &'a str, reason: &'a str) -> Fut<'a, Decision> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.shared.lock().unwrap().approval = Some(Approval { command: command.into(), reason: reason.into(), tx });
            self.wake.notify_one();
            rx.await.unwrap_or(Decision::No)
        })
    }

    fn send_file<'a>(&'a self, path: &'a Path, caption: &'a str) -> Fut<'a, Result<String>> {
        Box::pin(async move {
            let mut s = self.shared.lock().unwrap();
            s.note(&format!("📎 {}  {caption}", path.display()), Style::new().fg(ACCENT));
            Ok("The user is in the terminal and sees the file path.".into())
        })
    }
}

// ---------- terminal ----------

/// The panel lives in a fixed area we track ourselves; finished output is written above it
/// and scrolled up into the terminal's scrollback. (No cursor-position queries after start:
/// their replies would race with the key event stream.)
struct Ui {
    term: Terminal<CrosstermBackend<Stdout>>,
    top: u16,
    height: u16,
    enhanced: bool,
}

fn screen() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

fn restore_terminal(enhanced: bool) {
    if enhanced {
        crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags).ok();
    }
    crossterm::execute!(std::io::stdout(), DisableBracketedPaste, crossterm::cursor::Show).ok();
    crossterm::terminal::disable_raw_mode().ok();
}

impl Ui {
    fn new() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        if enhanced {
            // Lets Shift+Enter be told apart from Enter.
            crossterm::execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
        }
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal(enhanced);
            hook(info);
        }));
        let (_, row) = crossterm::cursor::position()?;
        let (w, _) = screen();
        let area = Rect::new(0, row, w, 1);
        let term = Terminal::with_options(CrosstermBackend::new(std::io::stdout()), TerminalOptions { viewport: Viewport::Fixed(area) })?;
        let mut ui = Self { term, top: row, height: 1, enhanced };
        ui.set_height(3)?;
        Ok(ui)
    }

    fn width(&self) -> usize {
        screen().0 as usize
    }

    /// Pushes everything on screen up by `n` lines (the top ones go into scrollback).
    fn scroll(&mut self, n: u16) -> Result<()> {
        let (_, h) = screen();
        let mut out = std::io::stdout();
        crossterm::queue!(out, crossterm::cursor::MoveTo(0, h.saturating_sub(1)))?;
        for _ in 0..n {
            out.write_all(b"\n")?;
        }
        out.flush()?;
        self.top = self.top.saturating_sub(n);
        Ok(())
    }

    fn clear_panel(&mut self) -> Result<()> {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::MoveTo(0, self.top),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
        )?;
        Ok(())
    }

    fn place(&mut self) -> Result<()> {
        let (w, _) = screen();
        self.term.resize(Rect::new(0, self.top, w, self.height))?;
        Ok(())
    }

    /// The panel grows and shrinks with its content.
    fn set_height(&mut self, height: u16) -> Result<()> {
        if height == self.height {
            return Ok(());
        }
        self.clear_panel()?;
        let (_, h) = screen();
        if self.top + height > h {
            self.scroll(self.top + height - h)?;
        }
        self.height = height;
        self.place()
    }

    /// After the terminal was resized.
    fn resized(&mut self) -> Result<()> {
        let (_, h) = screen();
        self.top = self.top.min(h.saturating_sub(self.height));
        self.clear_panel()?;
        self.place()
    }

    fn print(&mut self, lines: Vec<Line<'static>>) -> Result<()> {
        let (w, h) = screen();
        let rows: Vec<Line<'static>> = lines.iter().flat_map(|l| render::wrap(l, (w as usize).saturating_sub(1))).collect();
        let room = h.saturating_sub(self.height).max(1) as usize;
        for chunk in rows.chunks(room) {
            let n = chunk.len() as u16;
            self.clear_panel()?;
            let at = self.top.min(h.saturating_sub(self.height + n));
            if at < self.top {
                self.scroll(self.top - at)?;
            }
            let area = Rect::new(0, 0, w, n);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            for (i, l) in chunk.iter().enumerate() {
                buf.set_line(0, i as u16, l, w);
            }
            let empty = ratatui::buffer::Buffer::empty(area);
            let cells: Vec<(u16, u16, ratatui::buffer::Cell)> = empty.diff(&buf).into_iter().map(|(x, y, c)| (x, y + at, c.clone())).collect();
            let backend = self.term.backend_mut();
            ratatui::backend::Backend::draw(backend, cells.iter().map(|(x, y, c)| (*x, *y, c)))?;
            crossterm::execute!(std::io::stdout(), crossterm::style::ResetColor, crossterm::style::SetAttribute(crossterm::style::Attribute::Reset))?;
            self.top = at + n;
        }
        self.place()
    }

    fn close(mut self) {
        self.clear_panel().ok();
        restore_terminal(self.enhanced);
    }
}

// ---------- the app ----------

struct Running {
    handle: tokio::task::JoinHandle<Result<String>>,
    cancel: CancellationToken,
    started: Instant,
    /// A slash command (its output is printed as a note) rather than a turn.
    command: Option<&'static str>,
    /// The command loads choices for this picker instead of printing.
    pick: Option<&'static str>,
    /// A `!command` the user runs themselves.
    shell: Option<String>,
}

/// A list to choose from (model, effort, conversation), filtered by typing.
struct Picker {
    kind: &'static str,
    /// (label, value)
    items: Vec<(String, String)>,
    filter: String,
    sel: usize,
}

impl Picker {
    fn visible(&self) -> Vec<&(String, String)> {
        let f = self.filter.to_lowercase();
        self.items.iter().filter(|(label, _)| label.to_lowercase().contains(&f)).collect()
    }
}

struct Tui {
    app: Arc<App>,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<Notify>,
    ui: Ui,
    editor: Editor,
    running: Option<Running>,
    popup: usize,
    picker: Option<Picker>,
    /// Output of `!commands`, handed to the agent with the next message.
    shell_context: Vec<String>,
    ctrl_c: Option<Instant>,
    cfg: Config,
    quit: bool,
}

pub async fn run(app: Arc<App>) -> Result<()> {
    let mut t = Tui {
        app,
        shared: Default::default(),
        wake: Arc::new(Notify::new()),
        ui: Ui::new()?,
        editor: Editor::new(),
        running: None,
        popup: 0,
        picker: None,
        shell_context: Vec::new(),
        ctrl_c: None,
        cfg: Config::load()?,
        quit: false,
    };
    t.banner();
    let res = t.main_loop().await;
    if let Some(r) = t.running.take() {
        r.cancel.cancel();
        let _ = r.handle.await;
    }
    t.draw().ok();
    t.ui.close();
    res
}

async fn wait(running: &mut Option<Running>) -> Result<String> {
    match running {
        Some(r) => (&mut r.handle).await.unwrap_or_else(|e| Err(anyhow::anyhow!("task failed: {e}"))),
        None => std::future::pending().await,
    }
}

impl Tui {
    fn banner(&mut self) {
        let count = Session::load(SESSION).map(|s| s.messages.len()).unwrap_or(0);
        let home = config::home().display().to_string().replace(&config::user_home().display().to_string(), "~");
        let convo = if count == 0 { "new conversation".to_string() } else { format!("continuing a conversation of {count} message{} · /new to start over", if count == 1 { "" } else { "s" }) };
        let mut s = self.shared.lock().unwrap();
        s.out.push(Line::from(vec![
            Span::styled("✻ August", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} · {}", self.cfg.model, self.cfg.provider), DIM),
        ]));
        s.out.push(Line::styled(format!("  {home} · {convo}"), DIM));
        s.last = Kind::Note;
    }

    async fn main_loop(&mut self) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        while !self.quit {
            self.draw()?;
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => self.on_key(k),
                    Some(Ok(Event::Paste(text))) => {
                        let text = text.replace("\r\n", "\n").replace('\r', "\n");
                        self.editor.insert(&text);
                    }
                    Some(Ok(Event::Resize(..))) => self.ui.resized()?,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
                _ = tick.tick() => {}
                _ = self.wake.notified() => {}
                res = wait(&mut self.running) => self.finish(res),
            }
        }
        Ok(())
    }

    fn popup_items(&self) -> Vec<(&'static str, &'static str)> {
        let t = &self.editor.text;
        if !t.starts_with('/') || t.contains(char::is_whitespace) || self.shared.lock().unwrap().approval.is_some() {
            return Vec::new();
        }
        commands_list().into_iter().filter(|(n, _)| n.starts_with(&t[1..])).collect()
    }

    fn cancel(&mut self) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
        if let Some(a) = self.shared.lock().unwrap().approval.take() {
            let _ = a.tx.send(Decision::No);
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if !(ctrl && k.code == KeyCode::Char('c')) {
            self.ctrl_c = None;
        }

        // A pending approval takes the keyboard.
        let approval = self.shared.lock().unwrap().approval.take();
        if let Some(a) = approval {
            let decision = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Decision::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
                KeyCode::Char('c') if ctrl => Some(Decision::No),
                _ => None,
            };
            match decision {
                Some(d) => {
                    let label = match d {
                        Decision::Yes => "allowed",
                        Decision::Always => "allowed for this conversation",
                        Decision::No => "denied",
                    };
                    let mut s = self.shared.lock().unwrap();
                    s.note(&format!("  {label}: {}", a.command.lines().next().unwrap_or_default()), DIM);
                    let _ = a.tx.send(d);
                }
                None => self.shared.lock().unwrap().approval = Some(a),
            }
            return;
        }

        if self.picker.is_some() {
            return self.on_picker_key(k);
        }

        let popup = self.popup_items();
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                } else if self.running.is_some() {
                    self.cancel();
                } else if self.ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.ctrl_c = Some(Instant::now());
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.editor.is_empty() {
                    self.cancel();
                    self.quit = true;
                } else {
                    self.editor.delete();
                }
            }
            KeyCode::Char('j') if ctrl => self.editor.insert("\n"),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('u') if ctrl => self.editor.delete_to_start(),
            KeyCode::Char('k') if ctrl => self.editor.delete_to_end(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Char(c) if !ctrl => {
                self.editor.insert(c.encode_utf8(&mut [0; 4]));
                self.popup = 0;
            }
            KeyCode::Enter if k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => self.editor.insert("\n"),
            KeyCode::Enter => self.submit(&popup),
            KeyCode::Tab => {
                if let Some((name, _)) = popup.get(self.popup) {
                    self.editor.set(&format!("/{name}"));
                }
            }
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word(),
            KeyCode::Backspace => {
                self.editor.backspace();
                self.popup = 0;
            }
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left if alt || ctrl => self.editor.word_left(),
            KeyCode::Right if alt || ctrl => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Up if !popup.is_empty() => self.popup = (self.popup + popup.len() - 1) % popup.len(),
            KeyCode::Down if !popup.is_empty() => self.popup = (self.popup + 1) % popup.len(),
            KeyCode::Up => self.editor.up(),
            KeyCode::Down => self.editor.down(),
            KeyCode::Esc => {
                if self.running.is_some() {
                    self.cancel();
                } else if !popup.is_empty() {
                    self.editor.clear();
                }
            }
            _ => {}
        }
    }

    fn on_picker_key(&mut self, k: KeyEvent) {
        let Some(p) = &mut self.picker else { return };
        let n = p.visible().len();
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.picker = None,
            KeyCode::Up if n > 0 => p.sel = (p.sel + n - 1) % n,
            KeyCode::Down if n > 0 => p.sel = (p.sel + 1) % n,
            KeyCode::Backspace => {
                p.filter.pop();
                p.sel = 0;
            }
            KeyCode::Char(c) => {
                p.filter.push(c);
                p.sel = 0;
            }
            KeyCode::Enter => {
                let choice = p.visible().get(p.sel).map(|(_, v)| v.clone());
                let kind = p.kind;
                self.picker = None;
                if let Some(choice) = choice {
                    self.apply_choice(kind, &choice);
                }
            }
            _ => {}
        }
    }

    fn apply_choice(&mut self, kind: &str, choice: &str) {
        if kind == "resume" {
            return self.resume(choice);
        }
        let res = Config::load().and_then(|mut cfg| {
            if kind == "model" {
                cfg.model = choice.to_string();
            } else {
                cfg.effort = choice.to_string();
            }
            cfg.save().map(|_| cfg)
        });
        let mut s = self.shared.lock().unwrap();
        match res {
            Ok(cfg) => {
                s.note(&format!("{} set to {choice}", if kind == "model" { "Model" } else { "Effort" }), DIM);
                self.cfg = cfg;
            }
            Err(e) => s.note(&format!("✗ {e:#}"), Style::new().fg(RED)),
        }
    }

    fn current(&self, kind: &str) -> &str {
        match kind {
            "model" => &self.cfg.model,
            "effort" => &self.cfg.effort,
            _ => "",
        }
    }

    fn open_picker(&mut self, kind: &'static str, items: Vec<(String, String)>) {
        let sel = items.iter().position(|(_, v)| v == self.current(kind)).unwrap_or(0);
        self.picker = Some(Picker { kind, items, filter: String::new(), sel });
    }

    /// Switches to an archived conversation and shows where it left off.
    fn resume(&mut self, file: &str) {
        let mut s = self.shared.lock().unwrap();
        if let Err(e) = agent::restore(SESSION, file) {
            return s.note(&format!("✗ {e:#}"), Style::new().fg(RED));
        }
        self.app.reset(SESSION);
        let session = Session::load(SESSION);
        let msgs = session.as_ref().map(|s| s.messages.as_slice()).unwrap_or_default();
        s.note(&format!("Resumed a conversation of {} messages. Where it left off:", msgs.len()), DIM);
        let texts: Vec<&crate::llm::Message> = msgs.iter().filter(|m| !m.text().is_empty()).collect();
        for m in texts.iter().skip(texts.len().saturating_sub(2)) {
            let text = m.text();
            if m.role == crate::llm::Role::User {
                let text = text.split_once("] ").filter(|_| text.starts_with('[')).map(|(_, r)| r).unwrap_or(&text);
                s.user(text);
            } else {
                s.block(Kind::Text, render::markdown(&text));
            }
        }
        s.gap();
    }

    fn submit(&mut self, popup: &[(&'static str, &'static str)]) {
        if let Some((name, _)) = popup.get(self.popup) {
            self.editor.set(&format!("/{name}"));
        }
        self.popup = 0;
        let text = self.editor.submit();
        let text = text.trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        if !text.contains('\n')
            && let Some((name, args)) = commands::parse(&text)
        {
            return self.command(name, args);
        }
        if let Some(cmd) = text.strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
            return self.shell(cmd.to_string());
        }
        if self.running.is_some() {
            self.shared.lock().unwrap().queued.push(text);
        } else {
            self.shared.lock().unwrap().user(&text);
            let mut input = text;
            if !self.shell_context.is_empty() {
                input = format!("{}\n\n{input}", std::mem::take(&mut self.shell_context).join("\n\n"));
            }
            self.start_turn(Input::text(input));
        }
    }

    fn command(&mut self, name: &str, args: &str) {
        let note = |t: &Self, text: &str| t.shared.lock().unwrap().note(text, DIM);
        match name {
            "exit" | "quit" => self.quit = true,
            "stop" if self.running.is_some() => self.cancel(),
            "stop" => note(self, "Nothing is running."),
            _ if self.running.is_some() => note(self, "Wait for the reply to finish, or press esc to stop it."),
            "effort" if args.is_empty() => {
                let items = crate::llm::efforts(&self.cfg.provider).iter().map(|s| (s.to_string(), s.to_string())).collect();
                self.open_picker("effort", items);
            }
            "resume" => {
                let items: Vec<(String, String)> = agent::archived(SESSION, 30).into_iter().map(|a| (a.label, a.file)).collect();
                if items.is_empty() {
                    note(self, "No earlier conversations yet.");
                } else {
                    self.open_picker("resume", items);
                }
            }
            "model" if args.is_empty() => {
                let (app, cfg) = (self.app.clone(), self.cfg.clone());
                let handle = tokio::spawn(async move { app.llm.models(&cfg).await.map(|m| m.join("\n")) });
                self.running = Some(Running { handle, cancel: CancellationToken::new(), started: Instant::now(), command: Some("Loading models"), pick: Some("model"), shell: None });
            }
            _ => {
                self.shared.lock().unwrap().note(&format!("/{name} {args}"), DIM);
                let label: &'static str = match name {
                    "compact" => "Compacting the conversation",
                    "update" => "Updating",
                    _ => "Working",
                };
                let (app, name, args) = (self.app.clone(), name.to_string(), args.to_string());
                let handle = tokio::spawn(async move { commands::run(&app, SESSION, &name, &args).await });
                self.running = Some(Running { handle, cancel: CancellationToken::new(), started: Instant::now(), command: Some(label), pick: None, shell: None });
            }
        }
    }

    /// `!command`: runs in the workspace; the output goes to the agent with the next message.
    fn shell(&mut self, cmd: String) {
        if self.running.is_some() {
            return self.shared.lock().unwrap().note("Wait for the reply to finish, or press esc to stop it.", DIM);
        }
        let c = cmd.clone();
        let handle = tokio::spawn(async move { crate::tools::run("bash", &serde_json::json!({"command": c}), crate::tools::Scope::Full).await });
        self.running = Some(Running { handle, cancel: CancellationToken::new(), started: Instant::now(), command: Some("Running"), pick: None, shell: Some(cmd) });
    }

    fn start_turn(&mut self, input: Input) {
        {
            let mut s = self.shared.lock().unwrap();
            s.streamed = false;
            s.activity = None;
        }
        let ch = TuiChannel { shared: self.shared.clone(), wake: self.wake.clone() };
        let cancel = CancellationToken::new();
        let (app, c) = (self.app.clone(), cancel.clone());
        let handle = tokio::spawn(async move { agent::turn(&app, SESSION, input, &c, &ch).await });
        self.running = Some(Running { handle, cancel, started: Instant::now(), command: None, pick: None, shell: None });
    }

    fn finish(&mut self, res: Result<String>) {
        let Some(running) = self.running.take() else { return };
        self.cfg = Config::load().unwrap_or_else(|_| self.cfg.clone());
        let next = {
            let mut s = self.shared.lock().unwrap();
            s.emit(true);
            s.activity = None;
            s.approval = None;
            match (&res, running.command) {
                (_, Some(_)) if running.shell.is_some() => {
                    let cmd = running.shell.clone().unwrap_or_default();
                    let (out, failed) = match &res {
                        Ok(o) => (o.clone(), false),
                        Err(e) => (format!("error: {e:#}"), true),
                    };
                    s.tool_lines(&format!("$: {cmd}"), &out, failed);
                    let shown: String = out.chars().take(8000).collect();
                    self.shell_context.push(format!("[The user ran `{cmd}` in the shell themselves; output:]\n{shown}"));
                }
                (Ok(out), Some(_)) if running.pick.is_some() => {
                    let items: Vec<String> = out.lines().map(str::to_string).collect();
                    if items.is_empty() {
                        s.note("The provider listed no models; set one with /model <id>.", DIM);
                    }
                }
                (Ok(out), Some(_)) => {
                    let lines = render::markdown(out);
                    s.block(Kind::Note, lines);
                }
                (Ok(reply), None) => {
                    if !s.streamed && !reply.trim().is_empty() {
                        let lines = render::markdown(reply);
                        s.block(Kind::Text, lines);
                    }
                }
                (Err(e), _) if e.downcast_ref::<Cancelled>().is_some() => s.note("◼ Interrupted", DIM),
                (Err(e), _) => s.note(&format!("✗ {e:#}"), Style::new().fg(RED)),
            }
            s.gap();
            let queued = std::mem::take(&mut s.queued);
            for q in &queued {
                s.user(q);
            }
            queued
        };
        if let (Some(kind), Ok(out)) = (running.pick, &res) {
            let items: Vec<(String, String)> = out.lines().map(|l| (l.to_string(), l.to_string())).collect();
            if !items.is_empty() {
                self.open_picker(kind, items);
            }
        }
        if !next.is_empty() {
            self.start_turn(Input::merge(next.into_iter().map(Input::text).collect()));
        }
    }

    fn draw(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.shared.lock().unwrap().out);
        if !pending.is_empty() {
            self.ui.print(pending)?;
        }
        let width = self.ui.width();
        let inner = width.saturating_sub(1);
        let popup = self.popup_items();
        let busy = self.running.is_some();

        // Above the box: what the agent is doing, or a question for the user.
        let mut live: Vec<Line> = Vec::new();
        let approval_open;
        {
            let s = self.shared.lock().unwrap();
            approval_open = s.approval.is_some();
            if let Some(a) = &s.approval {
                live.push(Line::from(vec![
                    Span::styled("⚠ Allow this command? ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("({})", a.reason), DIM),
                ]));
                for l in a.command.lines().take(6) {
                    live.extend(render::wrap(&Line::styled(format!("  {l}"), Style::new().fg(render::CODE)), inner));
                }
                live.push(Line::from(vec![
                    Span::styled("  y", Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(" allow   ", DIM),
                    Span::styled("a", Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(" always in this conversation   ", DIM),
                    Span::styled("n", Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(" deny", DIM),
                ]));
            } else if let Some(r) = &self.running {
                let partial = s.stream.rsplit('\n').next().unwrap_or_default().trim();
                if !partial.is_empty() {
                    let rows = render::wrap(&Line::raw(partial.to_string()), inner);
                    let skip = rows.len().saturating_sub(2);
                    live.extend(rows.into_iter().skip(skip));
                }
                for q in s.queued.iter().take(3) {
                    let first: String = q.lines().next().unwrap_or_default().chars().take(width.saturating_sub(16)).collect();
                    live.push(Line::styled(format!("↳ queued: {first}"), DIM));
                }
                let spin = SPINNER[(r.started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
                let what = match (&s.activity, r.command) {
                    (_, Some(label)) => label.to_string(),
                    (Some(a), None) => a.chars().take(width.saturating_sub(30)).collect(),
                    (None, None) => "Thinking".to_string(),
                };
                live.push(Line::from(vec![
                    Span::styled(format!("{spin} "), Style::new().fg(ACCENT)),
                    Span::raw(what),
                    Span::styled(format!(" · {}s · esc to interrupt", r.started.elapsed().as_secs()), DIM),
                ]));
            }
        }

        // The input box.
        let text_cols = width.saturating_sub(4);
        let (rows, (cur_row, cur_col)) = self.editor.layout(text_cols);
        let first = (cur_row + 1).saturating_sub(MAX_INPUT_ROWS);
        let visible: Vec<&String> = rows.iter().skip(first).take(MAX_INPUT_ROWS).collect();
        let placeholder = self.editor.is_empty();
        let input: Vec<Line> = visible
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let prompt = if first + i == 0 { Span::styled(" › ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)) } else { Span::raw("   ") };
                if placeholder {
                    let hint = if busy { "Add to this turn…" } else { "Message August · / commands · ! shell" };
                    Line::from(vec![prompt, Span::styled(hint, DIM)])
                } else {
                    Line::from(vec![prompt, Span::raw(row.to_string())])
                }
            })
            .collect();
        let border = Style::new().fg(if busy || approval_open { ACCENT } else { Color::DarkGray });
        let (top, bottom) = self.borders(width, border);

        // Below the box: a picker, or command suggestions.
        let mut below: Vec<Line> = Vec::new();
        if let Some(p) = &self.picker {
            let current = self.current(p.kind);
            let items = p.visible();
            let title = match p.kind {
                "model" => "Choose a model",
                "effort" => "Choose reasoning effort",
                _ => "Go back to a conversation",
            };
            let filter = if p.filter.is_empty() { String::new() } else { format!(" · filter: {}", p.filter) };
            below.push(Line::from(vec![Span::styled(format!("  {title}"), Style::new().add_modifier(Modifier::BOLD)), Span::styled(filter, DIM)]));
            let start = (p.sel + 1).saturating_sub(10);
            for (i, (label, value)) in items.iter().enumerate().skip(start).take(10) {
                let selected = i == p.sel;
                let (mark, style) = if selected { ("❯ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)) } else { ("  ", Style::new()) };
                let check = if value == current { " ✓" } else { "" };
                let label: String = label.chars().take(width.saturating_sub(6)).collect();
                below.push(Line::from(vec![Span::styled(format!("{mark}{label}"), style), Span::styled(check, Style::new().fg(GREEN))]));
            }
            if items.is_empty() {
                below.push(Line::styled("  no match", DIM));
            }
        } else if !popup.is_empty() {
            self.popup = self.popup.min(popup.len() - 1);
            let start = (self.popup + 1).saturating_sub(8);
            for (i, (name, desc)) in popup.iter().enumerate().skip(start).take(8) {
                let selected = i == self.popup;
                let (mark, style) = if selected { ("❯ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)) } else { ("  ", Style::new()) };
                below.push(Line::from(vec![Span::styled(format!("{mark}/{name:<10}"), style), Span::styled(desc.to_string(), DIM)]));
            }
        }

        let screen = screen().1 as usize;
        let fixed = input.len() + 2;
        let below_rows = below.len().min(screen.saturating_sub(fixed + 1));
        let live_rows = live.len().min(screen.saturating_sub(fixed + below_rows + 1));
        let height = (live_rows + fixed + below_rows) as u16;
        self.ui.set_height(height)?;

        let skip = live.len() - live_rows;
        let live: Vec<Line> = live.into_iter().skip(skip).collect();
        let show_cursor = !approval_open && self.picker.is_none();
        self.ui.term.draw(|f| {
            let area = f.area();
            let w = area.width;
            let mut y = area.y;
            f.render_widget(Paragraph::new(live.clone()), Rect::new(area.x, y, w, live_rows as u16));
            y += live_rows as u16;
            f.render_widget(Paragraph::new(top.clone()), Rect::new(area.x, y, w, 1));
            y += 1;
            f.render_widget(Paragraph::new(input.clone()), Rect::new(area.x, y, w, input.len() as u16));
            if show_cursor {
                f.set_cursor_position((area.x + 3 + cur_col as u16, y + (cur_row - first) as u16));
            }
            y += input.len() as u16;
            f.render_widget(Paragraph::new(bottom.clone()), Rect::new(area.x, y, w, 1));
            y += 1;
            f.render_widget(Paragraph::new(below.clone()), Rect::new(area.x, y, w, below_rows as u16));
        })?;
        Ok(())
    }

    /// The box's top border carries the status, the bottom one the key hints.
    fn borders(&self, width: usize, border: Style) -> (Line<'static>, Line<'static>) {
        let st = self.app.stats(SESSION);
        let left = format!(" ✻ {} · {} ", self.cfg.model, self.cfg.effort);
        let mut right = String::new();
        if st.last.context() > 0 {
            let pct = st.last.context() as f64 * 100.0 / self.cfg.context_tokens.max(1) as f64;
            right += &format!(" {:.1}k context ({pct:.0}%)", st.last.context() as f64 / 1000.0);
            if let Some(c) = st.cost {
                right += &format!(" · ${c:.2}");
            }
            right += " ";
        }
        let fill = width.saturating_sub(left.width() + right.width() + 5);
        let top = Line::from(vec![
            Span::styled("╭─", border),
            Span::styled(left, Style::new().fg(ACCENT)),
            Span::styled("─".repeat(fill), border),
            Span::styled(right, DIM),
            Span::styled("──╮", border),
        ]);

        let hint = if self.picker.is_some() {
            "type to filter · ↑↓ choose · ⏎ select · esc cancel".to_string()
        } else if self.shared.lock().unwrap().approval.is_some() {
            "y allow · a always · n deny".to_string()
        } else if self.ctrl_c.is_some() {
            "press ctrl+c again to quit".to_string()
        } else if self.running.is_some() {
            "esc interrupt · ⏎ add to this turn".to_string()
        } else {
            format!("⏎ send · {} newline · / commands · ctrl+d quit", if self.ui.enhanced { "⇧⏎" } else { "⌥⏎" })
        };
        let hint = format!(" {hint} ");
        let fill = width.saturating_sub(hint.width() + 5);
        let bottom = Line::from(vec![
            Span::styled("╰─", border),
            Span::styled(hint, DIM),
            Span::styled("─".repeat(fill), border),
            Span::styled("──╯", border),
        ]);
        (top, bottom)
    }

}
