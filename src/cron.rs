//! Scheduled jobs from `cron.json` (written by the agent itself). Each run is a fresh session;
//! the final reply goes to Telegram unless it is `[SILENT]`.

use crate::agent::{self, App};
use crate::config::{self, Config};
use crate::channel::{Input, Silent};
use crate::telegram::Bot;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use croner::Cron;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
struct Job {
    id: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    at: Option<String>,
    prompt: String,
    #[serde(default)]
    chat: Option<i64>,
}

fn jobs_path() -> std::path::PathBuf {
    config::home().join("cron.json")
}

fn state_path() -> std::path::PathBuf {
    config::home().join("cron-state.json")
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Last run per job id.
    last: HashMap<String, DateTime<Local>>,
}

/// Whether a job should fire now. Recurring jobs that never ran count from `since`.
fn due(job: &Job, last: Option<DateTime<Local>>, since: DateTime<Local>, now: DateTime<Local>) -> Result<bool> {
    if let Some(at) = &job.at {
        let at = DateTime::parse_from_rfc3339(at).with_context(|| format!("job {}: bad `at` time", job.id))?;
        return Ok(last.is_none() && at <= now);
    }
    let expr = job.schedule.as_deref().with_context(|| format!("job {}: needs `schedule` or `at`", job.id))?;
    let cron = Cron::from_str(expr).with_context(|| format!("job {}: bad cron expression", job.id))?;
    let next = cron.find_next_occurrence(&last.unwrap_or(since), false)?;
    Ok(next <= now)
}

pub async fn run(app: Arc<App>, bot: Option<Arc<Bot>>) {
    let started = Local::now();
    let running: Arc<Mutex<HashSet<String>>> = Default::default();
    let mut last_error = String::new();
    loop {
        if let Err(e) = tick(&app, &bot, started, &running).await {
            let msg = format!("{e:#}");
            if msg != last_error {
                agent::log(&format!("cron: {msg}"));
                last_error = msg;
            }
        } else {
            last_error.clear();
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn tick(app: &Arc<App>, bot: &Option<Arc<Bot>>, started: DateTime<Local>, running: &Arc<Mutex<HashSet<String>>>) -> Result<()> {
    let text = match std::fs::read_to_string(jobs_path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let jobs: Vec<Job> = serde_json::from_str(&text).context("cron.json is not valid")?;
    let mut state: State = std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let now = Local::now();
    let mut fired = false;
    for job in jobs {
        if running.lock().unwrap().contains(&job.id) || !due(&job, state.last.get(&job.id).copied(), started, now)? {
            continue;
        }
        state.last.insert(job.id.clone(), now);
        fired = true;
        running.lock().unwrap().insert(job.id.clone());
        let (app, bot, running) = (app.clone(), bot.clone(), running.clone());
        tokio::spawn(async move {
            let id = job.id.clone();
            if let Err(e) = fire(&app, bot, &job).await {
                agent::log(&format!("cron {id}: {e:#}"));
            }
            running.lock().unwrap().remove(&id);
        });
    }
    if fired {
        std::fs::write(state_path(), serde_json::to_string_pretty(&state)?)?;
    }
    Ok(())
}

async fn fire(app: &Arc<App>, bot: Option<Arc<Bot>>, job: &Job) -> Result<()> {
    agent::log(&format!("cron {}: running", job.id));
    let safe: String = job.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let session = format!("cron-{safe}");
    agent::archive(&session)?;
    let prompt = format!(
        "[Scheduled job \"{}\". Your final reply is sent to the user; reply exactly [SILENT] if there is nothing worth sending.]\n\n{}",
        job.id, job.prompt
    );
    let chat = job.chat.or_else(|| Config::load().ok()?.telegram.allowed.first().copied());
    let cancel = CancellationToken::new();
    // Approvals and files go to the chat the reply goes to.
    let result = match (&bot, chat) {
        (Some(bot), Some(chat)) => agent::turn(app, &session, Input::text(prompt), &cancel, &bot.quiet(chat)).await,
        _ => agent::turn(app, &session, Input::text(prompt), &cancel, &Silent).await,
    };
    if job.at.is_some() {
        remove(&job.id)?;
    }
    let reply = result?;
    if reply.trim() == "[SILENT]" {
        return Ok(());
    }
    match (bot, chat) {
        (Some(bot), Some(chat)) => bot.api.send(chat, &reply).await,
        _ => {
            agent::log(&format!("cron {}: no Telegram chat to deliver to; reply: {reply}", job.id));
            Ok(())
        }
    }
}

/// Drops a finished one-shot job, leaving the rest of the file as the agent wrote it.
fn remove(id: &str) -> Result<()> {
    let mut jobs: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(jobs_path())?)?;
    jobs.retain(|j| j["id"] != id);
    std::fs::write(jobs_path(), serde_json::to_string_pretty(&jobs)? + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn job(schedule: Option<&str>, at: Option<&str>) -> Job {
        Job { id: "j".into(), schedule: schedule.map(Into::into), at: at.map(Into::into), prompt: String::new(), chat: None }
    }

    #[test]
    fn due_rules() {
        let t = |h, m| Local.with_ymd_and_hms(2026, 10, 19, h, m, 0).unwrap();
        let daily9 = job(Some("0 9 * * *"), None);
        // Never ran, started before 9:00: due at 9:00, not before.
        assert!(!due(&daily9, None, t(8, 0), t(8, 59)).unwrap());
        assert!(due(&daily9, None, t(8, 0), t(9, 0)).unwrap());
        // Ran at 9:00 today: not again until tomorrow.
        assert!(!due(&daily9, Some(t(9, 0)), t(8, 0), t(23, 0)).unwrap());
        // One-shot: due once its time passed, never after it ran.
        let at = job(None, Some(&t(10, 0).to_rfc3339()));
        assert!(!due(&at, None, t(8, 0), t(9, 59)).unwrap());
        assert!(due(&at, None, t(8, 0), t(10, 1)).unwrap());
        assert!(!due(&at, Some(t(10, 1)), t(8, 0), t(11, 0)).unwrap());
        assert!(due(&job(None, None), None, t(8, 0), t(9, 0)).is_err());
    }
}
