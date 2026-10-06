//! SQLite storage in `~/.august/august.db`: conversations (with FTS5 search),
//! token usage, long-term facts and scheduled tasks. Calls are short, so one mutex-guarded
//! connection is enough.

use crate::llm::{Block, Message, Usage};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub role: String,
    pub text: String,
    pub at: i64,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: i64,
    pub channel: String,
    pub chat: String,
    pub schedule: String,
    pub prompt: String,
    pub next_run: Option<i64>,
    pub last_run: Option<i64>,
    /// Skills whose instructions come with the prompt.
    pub skills: Vec<String>,
    /// Shell command run before the task; its output comes with the prompt.
    pub script: Option<String>,
    /// Runs in a fresh session of its own instead of the chat's conversation.
    pub isolated: bool,
}

/// Extra settings of a scheduled task.
#[derive(Debug, Clone, Default)]
pub struct TaskOptions {
    pub skills: Vec<String>,
    pub script: Option<String>,
    pub isolated: bool,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    chat_key TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_chat ON sessions(chat_key, created_at);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, id);
CREATE VIRTUAL TABLE IF NOT EXISTS msg_fts USING fts5(
    text, session_id UNINDEXED, role UNINDEXED, at UNINDEXED
);
CREATE TABLE IF NOT EXISTS usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    input INTEGER NOT NULL,
    output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_session ON usage(session_id);
CREATE INDEX IF NOT EXISTS usage_time ON usage(created_at);
CREATE TABLE IF NOT EXISTS facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel TEXT NOT NULL,
    chat TEXT NOT NULL,
    schedule TEXT NOT NULL,
    prompt TEXT NOT NULL,
    next_run INTEGER,
    last_run INTEGER,
    created_at INTEGER NOT NULL
);
";

impl Db {
    pub fn open() -> Result<Arc<Self>> {
        let home = crate::config::home();
        std::fs::create_dir_all(&home)?;
        Self::open_at(&home.join("august.db"))
    }

    pub fn open_at(path: &Path) -> Result<Arc<Self>> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Arc<Self> {
        Self::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn init(conn: Connection) -> Result<Arc<Self>> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Columns added after the first release; "duplicate column" means already there.
        for col in ["skills TEXT NOT NULL DEFAULT ''", "script TEXT", "isolated INTEGER NOT NULL DEFAULT 0"] {
            if let Err(e) = conn.execute(&format!("ALTER TABLE tasks ADD COLUMN {col}"), []) {
                if !e.to_string().contains("duplicate column") {
                    return Err(e.into());
                }
            }
        }
        Ok(Arc::new(Self { conn: Mutex::new(conn) }))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- conversations -------------------------------------------------

    /// The newest session of `chat_key` (created if there is none) and its live messages.
    pub fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)> {
        let id = match self.latest_session(chat_key)? {
            Some(id) => id,
            None => return Ok((self.new_session(chat_key)?, Vec::new())),
        };
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT role, content FROM messages WHERE session_id = ?1 AND archived = 0 ORDER BY id")?;
        let msgs = stmt
            .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter_map(|(role, content)| Message::from_parts(&role, &content))
            .collect();
        Ok((id, msgs))
    }

    /// The chat's current session (the newest one).
    fn latest_session(&self, chat_key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id FROM sessions WHERE chat_key = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [chat_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn new_session(&self, chat_key: &str) -> Result<String> {
        let id = crate::util::new_uuid();
        self.conn().execute(
            "INSERT INTO sessions (id, chat_key, created_at) VALUES (?1, ?2, ?3)",
            params![id, chat_key, now()],
        )?;
        Ok(id)
    }

    /// Stores messages at the end of a session. `index` makes their text searchable.
    pub fn append(&self, session: &str, msgs: &[Message], index: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        insert_messages(&tx, session, msgs, index)?;
        tx.commit()?;
        Ok(())
    }

    /// After a compaction: archives the session's live messages (they stay searchable)
    /// and stores `msgs` as the new live history.
    pub fn replace_live(&self, session: &str, msgs: &[Message]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("UPDATE messages SET archived = 1 WHERE session_id = ?1", [session])?;
        insert_messages(&tx, session, msgs, false)?;
        tx.commit()?;
        Ok(())
    }

    /// Full-text search over everything ever said (user and assistant text).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT session_id, role, text, at FROM msg_fts WHERE msg_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![q, limit as i64], |r| {
                Ok(Hit { role: r.get(1)?, text: r.get(2)?, at: r.get::<_, String>(3)?.parse().unwrap_or(0) })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(hits)
    }

    // ---- token usage ---------------------------------------------------

    pub fn record_usage(&self, session: &str, u: &Usage) -> Result<()> {
        self.conn().execute(
            "INSERT INTO usage (session_id, input, output, cache_read, cache_write, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session,
                u.input_tokens as i64,
                u.output_tokens as i64,
                u.cache_read_tokens as i64,
                u.cache_write_tokens as i64,
                now()
            ],
        )?;
        Ok(())
    }

    /// Number of model calls and their summed usage, for one session or (`None`) all
    /// sessions, since `since` (unix seconds).
    fn usage_total(&self, session: Option<&str>, since: i64) -> Result<(u64, Usage)> {
        let row = self.conn().query_row(
            "SELECT COUNT(*), TOTAL(input), TOTAL(output), TOTAL(cache_read), TOTAL(cache_write) FROM usage
             WHERE (?1 IS NULL OR session_id = ?1) AND created_at >= ?2",
            params![session, since],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?, r.get::<_, f64>(4)?)),
        )?;
        let usage = Usage {
            input_tokens: row.1 as u64,
            output_tokens: row.2 as u64,
            cache_read_tokens: row.3 as u64,
            cache_write_tokens: row.4 as u64,
        };
        Ok((row.0 as u64, usage))
    }

    /// `/usage`: totals of the chat's current session and today's across all chats.
    pub fn usage_report(&self, chat_key: &str) -> Result<String> {
        let session = self.latest_session(chat_key)?.unwrap_or_default();
        let midnight = chrono::Local::now()
            .date_naive()
            .and_time(chrono::NaiveTime::MIN)
            .and_local_timezone(chrono::Local)
            .earliest()
            .map_or(0, |t| t.timestamp());
        let line = |(calls, u): (u64, Usage)| {
            format!(
                "{calls} calls · in {} · cache read {} · cache write {} · out {}",
                u.input_tokens, u.cache_read_tokens, u.cache_write_tokens, u.output_tokens
            )
        };
        Ok(format!(
            "This session: {}\nToday, all chats: {}",
            line(self.usage_total(Some(&session), 0)?),
            line(self.usage_total(None, midnight)?)
        ))
    }

    // ---- facts ---------------------------------------------------------

    /// Deletes the facts `remove` and adds `text`, in one transaction. Fails, changing
    /// nothing, if one of the ids doesn't exist.
    pub fn replace_facts(&self, remove: &[i64], text: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in remove {
            if tx.execute("DELETE FROM facts WHERE id = ?1", [id])? == 0 {
                anyhow::bail!("no fact #{id}");
            }
        }
        tx.execute("INSERT INTO facts (text, created_at) VALUES (?1, ?2)", params![text, now()])?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn delete_fact(&self, id: i64) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM facts WHERE id = ?1", [id])? > 0)
    }

    pub fn facts(&self) -> Result<Vec<Fact>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, text FROM facts ORDER BY id")?;
        let facts = stmt
            .query_map([], |r| Ok(Fact { id: r.get(0)?, text: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(facts)
    }

    // ---- scheduled tasks -----------------------------------------------

    pub fn add_task(&self, channel: &str, chat: &str, schedule: &str, prompt: &str, next_run: i64, opts: &TaskOptions) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO tasks (channel, chat, schedule, prompt, next_run, created_at, skills, script, isolated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![channel, chat, schedule, prompt, next_run, now(), opts.skills.join(","), opts.script, opts.isolated],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Tasks of one chat, or of every chat when `chat` is `None`.
    pub fn tasks(&self, chat: Option<(&str, &str)>) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, channel, chat, schedule, prompt, next_run, last_run, skills, script, isolated FROM tasks
             WHERE (?1 IS NULL OR (channel = ?1 AND chat = ?2)) ORDER BY id",
        )?;
        let (c, h) = chat.unzip();
        let tasks = stmt.query_map(params![c, h], row_task)?.collect::<rusqlite::Result<_>>()?;
        Ok(tasks)
    }

    pub fn due_tasks(&self, at: i64) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, channel, chat, schedule, prompt, next_run, last_run, skills, script, isolated FROM tasks
             WHERE next_run IS NOT NULL AND next_run <= ?1 ORDER BY next_run",
        )?;
        let tasks = stmt.query_map([at], row_task)?.collect::<rusqlite::Result<_>>()?;
        Ok(tasks)
    }

    /// Records a run; `next_run: None` finishes the task (one-shot).
    pub fn finish_run(&self, id: i64, next_run: Option<i64>) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET last_run = ?2, next_run = ?3 WHERE id = ?1",
            params![id, now(), next_run],
        )?;
        Ok(())
    }

    pub fn delete_task(&self, id: i64, chat: Option<(&str, &str)>) -> Result<bool> {
        let (c, h) = chat.unzip();
        let n = self.conn().execute(
            "DELETE FROM tasks WHERE id = ?1 AND (?2 IS NULL OR (channel = ?2 AND chat = ?3))",
            params![id, c, h],
        )?;
        Ok(n > 0)
    }
}

fn row_task(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        channel: r.get(1)?,
        chat: r.get(2)?,
        schedule: r.get(3)?,
        prompt: r.get(4)?,
        next_run: r.get(5)?,
        last_run: r.get(6)?,
        skills: r.get::<_, String>(7)?.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
        script: r.get(8)?,
        isolated: r.get(9)?,
    })
}

fn insert_messages(tx: &rusqlite::Transaction, session: &str, msgs: &[Message], index: bool) -> Result<()> {
    let at = now();
    for m in msgs {
        tx.execute(
            "INSERT INTO messages (session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![session, m.role.as_str(), m.content_json(), at],
        )?;
        let text = searchable_text(m);
        if index && !text.is_empty() {
            tx.execute(
                "INSERT INTO msg_fts (text, session_id, role, at) VALUES (?1, ?2, ?3, ?4)",
                params![text, session, m.role.as_str(), at.to_string()],
            )?;
        }
    }
    Ok(())
}

/// Plain text a human said or read; tool traffic is not indexed.
fn searchable_text(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Turns free text into a safe FTS5 query: every word quoted, any of them may match.
fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(12)
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

impl crate::agent::SessionStore for Db {
    fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)> {
        Db::resume_session(self, chat_key)
    }
    fn new_session(&self, chat_key: &str) -> Result<String> {
        Db::new_session(self, chat_key)
    }
    fn append(&self, session: &str, msgs: &[Message], index: bool) -> Result<()> {
        Db::append(self, session, msgs, index)
    }
    fn replace_live(&self, session: &str, msgs: &[Message]) -> Result<()> {
        Db::replace_live(self, session, msgs)
    }
    fn facts(&self) -> Result<Vec<Fact>> {
        Db::facts(self)
    }
    fn record_usage(&self, session: &str, usage: &Usage) -> Result<()> {
        Db::record_usage(self, session, usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_resume_and_compact() {
        let db = Db::in_memory();
        let (s, msgs) = db.resume_session("cli").unwrap();
        assert!(msgs.is_empty());
        db.append(&s, &[Message::user_text("my cat is called Murzik")], true).unwrap();
        let (s2, msgs) = db.resume_session("cli").unwrap();
        assert_eq!((s2.as_str(), msgs.len()), (s.as_str(), 1));

        db.replace_live(&s, &[Message::user_text("summary")]).unwrap();
        let (_, msgs) = db.resume_session("cli").unwrap();
        assert_eq!(msgs[0].text(), "summary");
        // archived text is still searchable, and the re-inserted summary is not duplicated
        let hits = db.search("Murzik?", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(db.search("summary", 5).unwrap().is_empty());

        let fresh = db.new_session("cli").unwrap();
        assert_ne!(fresh, s);
        assert!(db.resume_session("cli").unwrap().1.is_empty());
        assert!(db.resume_session("other").unwrap().1.is_empty());
    }

    #[test]
    fn search_handles_odd_input_and_cyrillic() {
        let db = Db::in_memory();
        let (s, _) = db.resume_session("cli").unwrap();
        db.append(&s, &[Message::user_text("Купи молоко завтра")], true).unwrap();
        assert_eq!(db.search("молоко", 5).unwrap().len(), 1);
        assert!(db.search("\"*(", 5).unwrap().is_empty());
    }

    #[test]
    fn facts_and_tasks() {
        let db = Db::in_memory();
        let id = db.replace_facts(&[], "user lives in Berlin").unwrap();
        assert_eq!(db.facts().unwrap().len(), 1);
        assert!(db.delete_fact(id).unwrap());
        assert!(!db.delete_fact(id).unwrap());

        let t = db.add_task("telegram", "42", "every 1h", "ping", 100, &TaskOptions::default()).unwrap();
        assert!(db.due_tasks(99).unwrap().is_empty());
        assert_eq!(db.due_tasks(100).unwrap().len(), 1);
        assert!(!db.delete_task(t, Some(("telegram", "7"))).unwrap());
        db.finish_run(t, Some(200)).unwrap();
        assert!(db.due_tasks(150).unwrap().is_empty());
        db.finish_run(t, None).unwrap();
        assert!(db.due_tasks(1000).unwrap().is_empty());
        assert_eq!(db.tasks(Some(("telegram", "42"))).unwrap().len(), 1);
        assert!(db.delete_task(t, Some(("telegram", "42"))).unwrap());
    }
}
