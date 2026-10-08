//! SQLite storage in `~/.august/august.db`: conversations (with FTS5 search),
//! token usage, long-term facts and extensions' key-value storage. Calls are short, so one mutex-guarded
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
CREATE TABLE IF NOT EXISTS bindings (
    chat_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id)
);
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
CREATE TABLE IF NOT EXISTS kv (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (scope, key)
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
        // Columns added after the first release.
        for (column, ddl) in [("name", "ALTER TABLE sessions ADD COLUMN name TEXT"), ("settings", "ALTER TABLE sessions ADD COLUMN settings TEXT NOT NULL DEFAULT '{}'")] {
            let has: bool = conn.query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = ?1", [column], |r| r.get::<_, i64>(0))? > 0;
            if !has {
                conn.execute_batch(ddl)?;
            }
        }
        Ok(Arc::new(Self { conn: Mutex::new(conn) }))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- conversations -------------------------------------------------

    /// The session `chat_key` is bound to (created if there is none) and its live messages.
    pub fn resume_session(&self, chat_key: &str) -> Result<(String, Vec<Message>)> {
        let id = match self.latest_session(chat_key)? {
            Some(id) => id,
            None => return Ok((self.new_session(chat_key)?, Vec::new())),
        };
        Ok((id.clone(), self.live(&id)?))
    }

    /// A session's live messages (what the model sees).
    pub fn live(&self, id: &str) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT role, content FROM messages WHERE session_id = ?1 AND archived = 0 ORDER BY id")?;
        let msgs = stmt
            .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter_map(|(role, content)| Message::from_parts(&role, &content))
            .collect();
        Ok(msgs)
    }

    /// The chat's current session: the one bound to it, else its newest.
    fn latest_session(&self, chat_key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let bound = conn.query_row("SELECT session_id FROM bindings WHERE chat_key = ?1", [chat_key], |r| r.get(0)).optional()?;
        if bound.is_some() {
            return Ok(bound);
        }
        Ok(conn
            .query_row(
                "SELECT id FROM sessions WHERE chat_key = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [chat_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// A new session started in `chat_key`, which is bound to it from now on.
    pub fn new_session(&self, chat_key: &str) -> Result<String> {
        let id = crate::util::new_uuid();
        self.conn().execute(
            "INSERT INTO sessions (id, chat_key, created_at) VALUES (?1, ?2, ?3)",
            params![id, chat_key, now()],
        )?;
        self.bind(chat_key, &id)?;
        Ok(id)
    }

    /// Makes `session` the conversation of `chat_key`.
    pub fn bind(&self, chat_key: &str, session: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO bindings (chat_key, session_id) VALUES (?1, ?2) ON CONFLICT(chat_key) DO UPDATE SET session_id = ?2",
            params![chat_key, session],
        )?;
        Ok(())
    }

    /// Sessions, newest first, of the chat that started them (all when `None`):
    /// `{id, chat, name, settings, created_at, messages, bound: [chats]}`.
    pub fn sessions(&self, chat_key: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.chat_key, s.name, s.settings, s.created_at,
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id),
                    (SELECT group_concat(b.chat_key, char(10)) FROM bindings b WHERE b.session_id = s.id)
             FROM sessions s WHERE ?1 IS NULL OR s.chat_key = ?1 ORDER BY s.created_at DESC, s.rowid DESC",
        )?;
        let rows = stmt.query_map([chat_key], |r| {
            let settings: String = r.get(3)?;
            let bound: Option<String> = r.get(6)?;
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "chat": r.get::<_, String>(1)?,
                "name": r.get::<_, Option<String>>(2)?,
                "settings": serde_json::from_str::<serde_json::Value>(&settings).unwrap_or_default(),
                "created_at": r.get::<_, i64>(4)?,
                "messages": r.get::<_, i64>(5)?,
                "bound": bound.map(|b| b.split('\n').map(String::from).collect::<Vec<_>>()).unwrap_or_default(),
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One session (see `sessions`), if it exists.
    pub fn session(&self, id: &str) -> Result<Option<serde_json::Value>> {
        Ok(self.sessions(None)?.into_iter().find(|s| s["id"] == id))
    }

    /// Renames a session (`None` keeps the name) and merges `settings` into its settings
    /// (a null field deletes it).
    pub fn update_session(&self, id: &str, name: Option<&str>, settings: &serde_json::Value) -> Result<()> {
        let conn = self.conn();
        let current: String = conn
            .query_row("SELECT settings FROM sessions WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no session `{id}`"))?;
        let mut merged: serde_json::Value = serde_json::from_str(&current).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(o) = merged.as_object_mut() {
            for (k, v) in settings.as_object().into_iter().flatten() {
                if v.is_null() {
                    o.remove(k);
                } else {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        conn.execute("UPDATE sessions SET settings = ?2 WHERE id = ?1", params![id, merged.to_string()])?;
        if let Some(name) = name {
            conn.execute("UPDATE sessions SET name = ?2 WHERE id = ?1", params![id, name])?;
        }
        Ok(())
    }

    /// A session's settings (`{model, system, tools}`, each optional).
    pub fn session_settings(&self, id: &str) -> Result<serde_json::Value> {
        let s: Option<String> = self.conn().query_row("SELECT settings FROM sessions WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::json!({})))
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

    /// Totals of the chat's current session and of today across all chats:
    /// `{session, today}`, each `{calls, input, output, cache_read, cache_write}`.
    pub fn usage(&self, chat_key: &str) -> Result<serde_json::Value> {
        let session = self.latest_session(chat_key)?.unwrap_or_default();
        let midnight = chrono::Local::now()
            .date_naive()
            .and_time(chrono::NaiveTime::MIN)
            .and_local_timezone(chrono::Local)
            .earliest()
            .map_or(0, |t| t.timestamp());
        let json = |(calls, u): (u64, Usage)| {
            serde_json::json!({
                "calls": calls, "input": u.input_tokens, "output": u.output_tokens,
                "cache_read": u.cache_read_tokens, "cache_write": u.cache_write_tokens,
            })
        };
        Ok(serde_json::json!({"session": json(self.usage_total(Some(&session), 0)?), "today": json(self.usage_total(None, midnight)?)}))
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

    // ---- key-value storage (extensions keep their state here) -----------

    pub fn kv_get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT value FROM kv WHERE scope = ?1 AND key = ?2")?;
        Ok(stmt.query_map(params![scope, key], |r| r.get(0))?.next().transpose()?)
    }

    /// Stores `value` under `key`; `None` deletes it.
    pub fn kv_set(&self, scope: &str, key: &str, value: Option<&str>) -> Result<()> {
        let conn = self.conn();
        match value {
            Some(v) => conn.execute("INSERT OR REPLACE INTO kv (scope, key, value) VALUES (?1, ?2, ?3)", params![scope, key, v])?,
            None => conn.execute("DELETE FROM kv WHERE scope = ?1 AND key = ?2", params![scope, key])?,
        };
        Ok(())
    }

    /// `(key, value)` of every key starting with `prefix`, in key order.
    pub fn kv_list(&self, scope: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT key, value FROM kv WHERE scope = ?1 AND substr(key, 1, length(?2)) = ?2 ORDER BY key")?;
        let rows = stmt.query_map(params![scope, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
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
    fn live(&self, session: &str) -> Result<Vec<Message>> {
        Db::live(self, session)
    }
    fn bind(&self, chat_key: &str, session: &str) -> Result<()> {
        Db::bind(self, chat_key, session)
    }
    fn session_settings(&self, session: &str) -> Result<serde_json::Value> {
        Db::session_settings(self, session)
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
    fn facts() {
        let db = Db::in_memory();
        let id = db.replace_facts(&[], "user lives in Berlin").unwrap();
        assert_eq!(db.facts().unwrap().len(), 1);
        assert!(db.delete_fact(id).unwrap());
        assert!(!db.delete_fact(id).unwrap());
    }
}
