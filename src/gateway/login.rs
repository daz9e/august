//! Signing in to accounts. Extensions declare accounts; August runs every sign-in: it asks
//! for API keys itself, and for any other kind (OAuth, device codes, ...) shows the steps an
//! extension's script takes (`login_*` ops) in the thread the sign-in started from, receives
//! OAuth redirects on localhost and keeps the secrets (`config::secret`).

use super::{Gateway, waits};
use crate::extensions::{AccountInfo, Origin};
use crate::messengers::{Button, OutMessage, Thread};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long one step waits for the user.
const STEP_WAIT: Duration = Duration::from_secs(300);

/// A sign-in in progress: whose script runs it, where it shows, its redirect if awaited.
pub(super) struct Session {
    ext: String,
    thread: Thread,
    callback: Option<tokio::sync::oneshot::Receiver<Value>>,
}

/// Where an account's status is kept (core's own key-value scope).
const SCOPE: &str = "august:accounts";

impl Gateway {
    /// Every account with the extension offering it and how it stands:
    /// `{id, label, extension, providers, kind: key|login, status: none|connected|expired, who}`.
    pub(super) fn accounts(&self) -> Result<Value> {
        let mut out = Vec::new();
        for (ext, a) in self.ext.accounts() {
            let (status, who) = self.account_status(&ext, &a)?;
            let kind = if a.key.is_some() { "key" } else { "login" };
            out.push(json!({"id": a.id, "label": a.label, "extension": ext, "providers": a.providers, "kind": kind, "status": status, "who": who}));
        }
        Ok(Value::Array(out))
    }

    fn account_status(&self, ext: &str, a: &AccountInfo) -> Result<(String, Option<String>)> {
        if let Some(key) = &a.key {
            if let Some(env) = key.env.as_deref().filter(|e| std::env::var(e).is_ok_and(|v| !v.is_empty())) {
                return Ok(("connected".into(), Some(format!("key from ${env}"))));
            }
            let saved = crate::config::secret(ext, &a.id)?.is_some();
            return Ok((if saved { "connected" } else { "none" }.into(), None));
        }
        let v: Value = self.db.kv_get(SCOPE, &format!("{ext}/{}", a.id))?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Ok((v["status"].as_str().unwrap_or("none").into(), v["who"].as_str().map(String::from)))
    }

    /// Records how an account of `ext` stands; an expired one is told to the home thread.
    pub(super) async fn account_update(&self, ext: &str, id: &str, status: &str, who: Option<&str>) -> Result<()> {
        let account = self.find_account(id).filter(|(e, _)| e == ext).ok_or_else(|| anyhow!("{ext} has no account `{id}`"))?;
        anyhow::ensure!(["none", "connected", "expired"].contains(&status), "status is none, connected or expired");
        let key = format!("{ext}/{id}");
        let value = (status != "none").then(|| json!({"status": status, "who": who}).to_string());
        self.db.kv_set(SCOPE, &key, value.as_deref())?;
        if status == "expired"
            && let Ok(home) = self.home()
            && let Ok(m) = self.messenger(&home)
        {
            let text = format!("🔑 The sign-in to {} has expired. Sign in again with `/login {id}`.", account.1.label);
            m.send(&home.id, &OutMessage::text(text)).await.ok();
        }
        Ok(())
    }

    fn find_account(&self, id: &str) -> Option<(String, AccountInfo)> {
        self.ext.accounts().into_iter().find(|(_, a)| a.id == id)
    }

    /// Signs in to account `id` from `thread`; returns who signed in.
    pub(super) async fn login(self: &Arc<Self>, id: &str, thread: Thread) -> Result<Value> {
        let (ext, account) = self.find_account(id).ok_or_else(|| anyhow!("no account `{id}`"))?;
        if let Some(key) = &account.key {
            let value = self.ask(&thread, &format!("Send your {} for {}.", key.label, account.label), true, &[]).await?;
            let check = json!({"account": id, "key": value});
            self.ext.call_account(&ext, "account_check", check, &Origin::thread(thread.clone())).await.map_err(|e| anyhow!("the key was not accepted: {e}"))?;
            crate::config::set_secret(&ext, id, Some(&value))?;
            return Ok(json!({"who": null}));
        }
        let session = self.next_login.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.logins.lock().unwrap().insert(session, Session { ext: ext.clone(), thread: thread.clone(), callback: None });
        let result = self.ext.call_account(&ext, "login", json!({"account": id, "session": session}), &Origin::thread(thread)).await;
        self.logins.lock().unwrap().remove(&session);
        let signed = result.map_err(|e| anyhow!(e))?;
        self.account_update(&ext, id, "connected", signed["who"].as_str()).await?;
        Ok(signed)
    }

    /// Signs out of account `id`: its key, or whatever its extension keeps.
    pub(super) async fn logout(&self, id: &str) -> Result<()> {
        let (ext, account) = self.find_account(id).ok_or_else(|| anyhow!("no account `{id}`"))?;
        if account.key.is_some() {
            return crate::config::set_secret(&ext, id, None);
        }
        self.ext.call_account(&ext, "logout", json!({"account": id}), &Origin::default()).await.map_err(|e| anyhow!(e))?;
        self.account_update(&ext, id, "none", None).await
    }

    /// The thread of session `p.session`, which must be `ext`'s.
    fn session_thread(&self, ext: &str, p: &Value) -> Result<(u64, Thread)> {
        let id = p["session"].as_u64().ok_or_else(|| anyhow!("missing `session`"))?;
        let logins = self.logins.lock().unwrap();
        let s = logins.get(&id).filter(|s| s.ext == ext).ok_or_else(|| anyhow!("no sign-in #{id} of yours is running"))?;
        Ok((id, s.thread.clone()))
    }

    /// One step of a sign-in script (`login_ask`, `login_choose`, `login_open`,
    /// `login_progress`, `login_callback`, `login_wait`).
    pub(super) async fn login_step(self: &Arc<Self>, ext: &str, op: &str, p: &Value) -> Result<Value> {
        let (session, thread) = self.session_thread(ext, p)?;
        let text = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        Ok(match op {
            "login_ask" => json!(self.ask(&thread, &text("label"), p["secret"] == true, &[]).await?),
            "login_choose" => {
                let options: Vec<String> = serde_json::from_value(p["options"].clone()).unwrap_or_default();
                json!(self.ask(&thread, &text("question"), false, &options).await?)
            }
            "login_open" => {
                // At this machine's terminal the browser opens by itself.
                if thread.messenger == "cli" {
                    open_browser(&text("url"));
                }
                let note = Some(text("note")).filter(|n| !n.is_empty()).unwrap_or_else(|| "Open this link to sign in:".into());
                self.messenger(&thread)?.send(&thread.id, &OutMessage::text(format!("{note}\n{}", text("url")))).await?;
                Value::Null
            }
            "login_progress" => {
                self.messenger(&thread)?.send(&thread.id, &OutMessage::text(text("text"))).await?;
                Value::Null
            }
            "login_callback" => {
                let port = p["port"].as_u64().unwrap_or(0) as u16;
                let path = Some(text("path")).filter(|s| s.starts_with('/')).unwrap_or_else(|| "/callback".into());
                let (url, rx) = receive_redirect(port, &path).await?;
                if let Some(s) = self.logins.lock().unwrap().get_mut(&session) {
                    s.callback = Some(rx);
                }
                json!(url)
            }
            "login_wait" => {
                let rx = self.logins.lock().unwrap().get_mut(&session).and_then(|s| s.callback.take());
                let rx = rx.ok_or_else(|| anyhow!("call login_callback first"))?;
                let timeout = Duration::from_millis(p["timeout_ms"].as_u64().unwrap_or(300_000));
                self.wait_redirect(&thread, rx, timeout).await?
            }
            other => bail!("unknown step {other}"),
        })
    }

    /// Asks in `thread` and waits for the answer: a text (with `secret`, deleted from the
    /// chat) or one of `options` as buttons.
    async fn ask(self: &Arc<Self>, thread: &Thread, question: &str, secret: bool, options: &[String]) -> Result<String> {
        let tag = self.next_login.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let buttons: Vec<Button> = options.iter().enumerate().map(|(i, o)| Button { id: format!("login{tag}.{i}"), label: o.clone() }).collect();
        let accept = waits::Accept { buttons: buttons.iter().map(|b| b.id.clone()).collect(), text: true, secret };
        let (wait, rx) = self.waits.add(thread.clone(), accept);
        let mark = if secret { "🔑" } else { "❓" };
        let message = OutMessage { text: format!("{mark} {question}"), buttons: if buttons.is_empty() { vec![] } else { vec![buttons.clone()] }, ..Default::default() };
        let sent = self.messenger(thread)?.send(&thread.id, &message).await;
        let reply = tokio::time::timeout(STEP_WAIT, rx).await.ok().and_then(Result::ok);
        self.waits.remove(wait);
        let answer = match reply {
            Some(waits::Reply::Press(b)) => buttons.iter().position(|x| x.id == b).map(|i| options[i].clone()),
            Some(waits::Reply::Text(t)) => Some(t),
            Some(waits::Reply::Cancelled(_)) => bail!("sign-in cancelled"),
            None => None,
        };
        if let (Ok(id), Some(a)) = (sent, &answer) {
            let shown = if secret { "received" } else { a.as_str() };
            self.messenger(thread)?.edit(&thread.id, &id, &OutMessage::text(format!("{mark} {question}\n→ {shown}"))).await.ok();
        }
        answer.ok_or_else(|| anyhow!("no answer within {} minutes", STEP_WAIT.as_secs() / 60))
    }

    /// The redirect's query: from the browser on this machine, or the address the user pastes
    /// (a browser elsewhere ends on a page that doesn't load; its address holds the answer).
    async fn wait_redirect(self: &Arc<Self>, thread: &Thread, rx: tokio::sync::oneshot::Receiver<Value>, timeout: Duration) -> Result<Value> {
        let (wait, pasted) = self.waits.add(thread.clone(), waits::Accept { text: true, secret: true, ..Default::default() });
        let hint = "If the browser is on another device, it ends on a page that doesn't load: paste that page's address here.";
        self.messenger(thread)?.send(&thread.id, &OutMessage::text(hint)).await.ok();
        let got = tokio::select! {
            q = rx => q.ok(),
            r = pasted => match r {
                Ok(waits::Reply::Text(t)) => Some(query_of(&t)?),
                Ok(waits::Reply::Cancelled(_)) => bail!("sign-in cancelled"),
                _ => None,
            },
            _ = tokio::time::sleep(timeout) => None,
        };
        self.waits.remove(wait);
        got.ok_or_else(|| anyhow!("no sign-in within {} minutes", timeout.as_secs() / 60))
    }
}

/// The query parameters of a pasted address.
fn query_of(url: &str) -> Result<Value> {
    let url = reqwest::Url::parse(url.trim()).map_err(|_| anyhow!("that is not an address"))?;
    Ok(Value::Object(url.query_pairs().map(|(k, v)| (k.into_owned(), Value::String(v.into_owned()))).collect()))
}

/// Opens `url` in this machine's browser (best effort; `AUGUST_OPEN_BROWSER=0`: never).
fn open_browser(url: &str) {
    if std::env::var("AUGUST_OPEN_BROWSER").is_ok_and(|v| v == "0") {
        return;
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// Listens on `127.0.0.1:<port>` (0: any free one) for one request to `path`; returns the
/// address to redirect to and its query once it comes. Browsers open spare connections, so
/// each is served on its own; others get a 404. It gives up after 15 minutes.
async fn receive_redirect(port: u16, path: &str) -> Result<(String, tokio::sync::oneshot::Receiver<Value>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("port {port} is busy ({e}): another sign-in may still be waiting"))?;
    let url = format!("http://localhost:{}{path}", listener.local_addr()?.port());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let path = path.to_string();
    tokio::spawn(async move {
        let (found, mut got) = tokio::sync::mpsc::channel::<Value>(1);
        let mut conns = tokio::task::JoinSet::new();
        let deadline = tokio::time::sleep(Duration::from_secs(900));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                Ok((sock, _)) = listener.accept() => { conns.spawn(serve(sock, path.clone(), found.clone())); }
                Some(q) = got.recv() => { tx.send(q).ok(); return; }
                _ = &mut deadline => return,
            }
        }
    });
    Ok((url, rx))
}

async fn serve(mut sock: tokio::net::TcpStream, path: String, found: tokio::sync::mpsc::Sender<Value>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 64 * 1024 {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let target = req.split_whitespace().nth(1).unwrap_or("/");
    let query = query_of(&format!("http://localhost{target}")).ok();
    let ours = target.split('?').next() == Some(path.as_str());
    let (status, message) = if ours { ("200 OK", "Signed in. You can close this window and return to August.") } else { ("404 Not Found", "Not found.") };
    let body = format!("<!doctype html><meta charset=utf-8><title>August</title><p style=\"font:16px system-ui;margin:3em\">{message}</p>");
    let resp = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    sock.write_all(resp.as_bytes()).await.ok();
    sock.shutdown().await.ok();
    if ours && let Some(q) = query {
        found.send(q).await.ok();
    }
}
