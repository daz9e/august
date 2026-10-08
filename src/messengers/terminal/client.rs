//! `august` without arguments: a terminal window connected to the running August (started
//! if it isn't) as one thread of the terminal messenger. It prints what August sends as
//! it streams in (an edit prints only what it adds), numbers buttons (a number presses the
//! button of the oldest open question) and shows `> ` when the thread is idle.

use super::{ToAugust, ToClient, WireButton, socket_path};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long to wait for a freshly started August to listen.
const START_WAIT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct Screen {
    /// What each printed message shows so far.
    shown: HashMap<String, String>,
    /// Messages that came with buttons (an edit of one shows only its first line, the verdict).
    asked: HashSet<String>,
    /// Messages with buttons waiting for an answer, oldest first.
    open: VecDeque<(String, Vec<WireButton>)>,
    /// A prompt (`> `, or the options of an open question) is the last thing on screen.
    at_prompt: bool,
    /// The cursor is not at the start of a line.
    mid_line: bool,
}

impl Screen {
    /// What to type to answer the oldest open question.
    fn question_prompt(&self) -> Option<String> {
        let (_, buttons) = self.open.front()?;
        let options: Vec<String> = buttons.iter().enumerate().map(|(i, b)| format!("{}. {}", i + 1, b.label)).collect();
        Some(format!("   {} → ", options.join("   ")))
    }

    /// Prints `text` (`fresh`: on a new line) above the prompt. While a question is open,
    /// its options stay the last line.
    fn print(&mut self, text: &str, fresh: bool) {
        let mut out = std::io::stdout().lock();
        if self.at_prompt {
            // ponytail: also wipes what the user was typing from view (not from the input); a line editor would keep it.
            write!(out, "{}", if out.is_terminal() { "\r\x1b[2K" } else { "\n" }).ok();
            self.at_prompt = false;
            self.mid_line = false;
        }
        if fresh && self.mid_line {
            writeln!(out).ok();
        }
        write!(out, "{text}").ok();
        if !text.is_empty() {
            self.mid_line = !text.ends_with('\n');
        }
        if let Some(prompt) = self.question_prompt() {
            write!(out, "{}{prompt}", if self.mid_line { "\n" } else { "" }).ok();
            self.at_prompt = true;
            self.mid_line = false;
        }
        out.flush().ok();
    }

    /// Shows `> ` unless a prompt is already there.
    fn prompt(&mut self) {
        if self.at_prompt {
            return;
        }
        let mut out = std::io::stdout().lock();
        write!(out, "{}> ", if self.mid_line { "\n\n" } else { "\n" }).ok();
        out.flush().ok();
        self.at_prompt = true;
        self.mid_line = false;
    }

    fn show(&mut self, msg: ToClient) {
        match msg {
            ToClient::Hello { .. } => {}
            ToClient::Send { id, text, buttons, files } => {
                self.shown.insert(id.clone(), text.clone());
                let buttons: Vec<WireButton> = buttons.into_iter().flatten().collect();
                if !buttons.is_empty() {
                    self.asked.insert(id.clone());
                    self.open.push_back((id, buttons));
                }
                let files: Vec<String> = files.iter().map(|f| format!("📎 {f}")).collect();
                let text = [text, files.join("\n")].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
                self.print(&text, true);
            }
            ToClient::Edit { id, text, buttons } => {
                let buttons: Vec<WireButton> = buttons.into_iter().flatten().collect();
                // An edit without buttons settles a question (answered or timed out); one with
                // buttons asks (again).
                self.open.retain(|(open, _)| *open != id);
                if !buttons.is_empty() {
                    self.asked.insert(id.clone());
                    self.open.push_back((id.clone(), buttons));
                    self.shown.insert(id, text.clone());
                    return self.print(&text, true);
                }
                let old = self.shown.insert(id.clone(), text.clone()).unwrap_or_default();
                let added = text.strip_prefix(old.as_str());
                if self.asked.contains(&id) {
                    // A settled question shows its outcome: what was added, else the new first line.
                    let outcome = added.map(str::trim).filter(|a| !a.is_empty());
                    let outcome = outcome.unwrap_or_else(|| text.lines().next().unwrap_or_default()).to_string();
                    self.print(&outcome, true);
                } else {
                    match added {
                        Some(added) => self.print(added, false),
                        None => self.print(&text, true),
                    }
                }
            }
            ToClient::Idle => self.prompt(),
        }
    }

    /// What a typed line means: a number presses a button of the oldest open question.
    fn input(&mut self, line: &str) -> ToAugust {
        self.at_prompt = false;
        self.mid_line = false;
        let pick = self.open.front().and_then(|(_, buttons)| buttons.get(line.parse::<usize>().ok()?.checked_sub(1)?)).map(|b| b.id.clone());
        match pick {
            Some(button) => {
                self.open.pop_front();
                ToAugust::Press { button }
            }
            None => ToAugust::Text { text: line.to_string() },
        }
    }
}

/// Connects to August, starting it first if nothing listens.
async fn connect() -> Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }
    start_august()?;
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
        if started.elapsed() > START_WAIT {
            bail!("August did not start; see {}", crate::cli::service::log_path().display());
        }
    }
}

/// Starts the installed service, or else August in the background on its own.
fn start_august() -> Result<()> {
    if crate::cli::service::installed() {
        eprintln!("starting the August service…");
        return crate::cli::service::start();
    }
    eprintln!("starting August in the background (`august serve` keeps it running at login)…");
    let log = crate::cli::service::log_path();
    std::fs::create_dir_all(log.parent().unwrap_or(&log))?;
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("gateway")
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        // Its own process group, so closing this terminal doesn't stop it.
        .process_group(0)
        .spawn()
        .context("could not start August")?;
    Ok(())
}

pub async fn run() -> Result<()> {
    run_with(None).await
}

/// A terminal window that first sends `first` (a command, say) as if typed.
pub async fn run_with(first: Option<&str>) -> Result<()> {
    let stream = connect().await?;
    let (read, mut write) = stream.into_split();
    let send = |msg: &ToAugust| serde_json::to_string(msg).unwrap_or_default() + "\n";
    write.write_all(send(&ToAugust::Hello).as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    let thread = match lines.next_line().await?.map(|l| serde_json::from_str::<ToClient>(&l)) {
        Some(Ok(ToClient::Hello { thread })) => thread,
        _ => bail!("August did not greet this terminal"),
    };
    println!("august · terminal {thread} · /help lists commands, /exit quits.");
    let screen = Arc::new(Mutex::new(Screen::default()));
    match first {
        Some(line) => write.write_all(send(&ToAugust::Text { text: line.into() }).as_bytes()).await?,
        None => screen.lock().unwrap().prompt(),
    }

    let shown = screen.clone();
    let mut output = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(msg) = serde_json::from_str::<ToClient>(&line) {
                shown.lock().unwrap().show(msg);
            }
        }
    });
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            _ = &mut output => {
                println!("\nAugust stopped.");
                return Ok(());
            }
            line = input.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                let line = line.trim();
                match line {
                    "" => screen.lock().unwrap().prompt(),
                    "/exit" | "/quit" => return Ok(()),
                    _ => {
                        let msg = screen.lock().unwrap().input(line);
                        write.write_all(send(&msg).as_bytes()).await?;
                    }
                }
            }
        }
    }
}
