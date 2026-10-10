//! The Telegram messenger against a fake core and a fake Bot API: what users write comes in
//! as `inbound` (allowed users only, groups marking what is addressed, topics as threads),
//! what August sends goes out as HTML with inline buttons, plus reactions, the command
//! menu, topics, the `pin` action, downloads and pairing the owner at login.

#[path = "../fake_http.rs"]
mod fake_http;

use super::serve;
use august_ext::FakeAugust;
use fake_http::{FakeHttp, Req, Resp};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "TEST";
const OWNER: i64 = 7;
const CHAT: i64 = 5;

/// The fake Bot API: updates queued with `push` come back on the next `getUpdates`.
struct BotApi {
    http: FakeHttp,
    updates: Arc<Mutex<Vec<Value>>>,
}

impl BotApi {
    async fn start() -> BotApi {
        let updates: Arc<Mutex<Vec<Value>>> = Arc::default();
        let queue = updates.clone();
        let http = FakeHttp::start(move |req| {
            let ok = |v: Value| Resp::json(json!({"ok": true, "result": v}));
            if let Some(file) = req.path.strip_prefix(&format!("/file/bot{TOKEN}/")) {
                return if file == "doc" { Resp::bytes("application/pdf", b"%PDF".to_vec()) } else { Resp::status(404, "gone") };
            }
            let method = req.path.strip_prefix(&format!("/bot{TOKEN}/")).unwrap_or("");
            match method {
                "getMe" => ok(json!({"id": 99, "is_bot": true, "username": "AugustBot"})),
                "getUpdates" => {
                    let batch: Vec<Value> = queue.lock().unwrap().drain(..).collect();
                    let empty = batch.is_empty();
                    let resp = ok(json!(batch));
                    if empty { resp.after(Duration::from_millis(20)) } else { resp }
                }
                // Telegram can't parse this HTML; the plain retry has no parse_mode.
                "sendMessage" if req.json()["parse_mode"] == "HTML" && req.json()["text"].as_str().is_some_and(|t| t.contains("<b>bad</b>")) => {
                    Resp::json(json!({"ok": false, "description": "Bad Request: can't parse entities: nope"}))
                }
                "sendMessage" => ok(json!({"message_id": 42})),
                "createForumTopic" => ok(json!({"message_thread_id": 77})),
                "getFile" => ok(json!({"file_path": req.json()["file_id"]})),
                "" => Resp::status(404, "unknown token"),
                _ => ok(json!(true)),
            }
        })
        .await;
        BotApi { http, updates }
    }

    fn push(&self, update: Value) {
        self.updates.lock().unwrap().push(update);
    }

    /// The JSON bodies of calls of `method`.
    fn calls(&self, method: &str) -> Vec<Value> {
        self.http.to(&format!("/{method}")).iter().map(Req::json).collect()
    }

    async fn wait_call(&self, method: &str) -> Value {
        for _ in 0..1000 {
            if let Some(c) = self.calls(method).pop() {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no {method} call")
    }
}

/// A message from `user` in chat `chat` (`private`, `group`, `supergroup`).
fn message(id: i64, chat_type: &str, user: i64, text: &str) -> Value {
    let chat = if chat_type == "private" { user } else { CHAT };
    json!({"update_id": id, "message": {
        "message_id": id, "date": 0, "text": text,
        "chat": {"id": chat, "type": chat_type},
        "from": {"id": user, "is_bot": false, "first_name": "Ann", "username": "ann"},
    }})
}

/// Telegram connected with the token and the owner allowed by the environment.
async fn telegram() -> (FakeAugust, BotApi) {
    let api = BotApi::start().await;
    let owner = OWNER.to_string();
    let (fake, august) =
        FakeAugust::new(&[("TELEGRAM_BOT_TOKEN", TOKEN), ("TELEGRAM_ALLOWED_USERS", &owner), ("TELEGRAM_API_BASE", &api.http.url)]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.wait_until("the messenger", |f| f.manifest().is_some_and(|m| m["messengers"][0]["id"] == "telegram")).await;
    (fake, api)
}

async fn messenger(fake: &FakeAugust, method: &str, mut params: Value) -> anyhow::Result<Value> {
    params["messenger"] = json!("telegram");
    fake.request(method, params).await
}

#[tokio::test]
async fn private_messages_and_commands_come_in() {
    let (fake, api) = telegram().await;
    api.push(message(1, "private", OWNER, "hi"));
    api.push(message(2, "private", OWNER, "/model x"));
    let hi = fake.wait_call("inbound", 1).await;
    assert_eq!((hi["kind"].as_str(), hi["text"].as_str(), hi["addressed"].as_bool()), (Some("message"), Some("hi"), Some(true)));
    assert_eq!(hi["thread"], json!({"messenger": "telegram", "id": "7"}));
    assert_eq!((hi["user"]["id"].as_str(), hi["user"]["name"].as_str(), hi["place"]["kind"].as_str()), (Some("7"), Some("Ann (@ann)"), Some("dm")));
    let cmd = fake.wait_call("inbound", 2).await;
    assert_eq!((cmd["kind"].as_str(), cmd["name"].as_str(), cmd["args"].as_str()), (Some("command"), Some("model"), Some("x")));
}

#[tokio::test]
async fn strangers_are_turned_away_and_ignored_in_groups() {
    let (fake, api) = telegram().await;
    api.push(message(1, "group", 8, "hi all"));
    api.push(message(2, "private", 8, "hi"));
    let denied = api.wait_call("sendMessage").await;
    assert_eq!(denied["chat_id"], 8);
    assert!(denied["text"].as_str().unwrap().contains("not authorized"), "{denied}");
    // A stranger's button press is turned away too; nothing reaches August.
    api.push(json!({"update_id": 3, "callback_query": {"id": "cb3", "from": {"id": 8, "first_name": "S"},
        "message": {"message_id": 1, "chat": {"id": CHAT, "type": "private"}}, "data": "k"}}));
    api.push(message(4, "private", OWNER, "hi"));
    fake.wait_call("inbound", 1).await;
    assert_eq!(api.calls("sendMessage").len(), 2, "the group chatter got no answer");
    assert_eq!(fake.calls("inbound").len(), 1);
}

#[tokio::test]
async fn groups_mark_what_is_addressed() {
    let (fake, api) = telegram().await;
    api.push(message(1, "supergroup", OWNER, "dinner at 8?"));
    api.push(message(2, "supergroup", OWNER, "@augustbot what's the weather"));
    // A command for another bot is chatter too.
    api.push(message(3, "group", OWNER, "/new@OtherBot"));
    fake.wait_call("inbound", 3).await;
    let got: Vec<(String, bool)> = fake.calls("inbound").iter().map(|e| (e["text"].as_str().unwrap().to_string(), e["addressed"] == true)).collect();
    assert_eq!(got, [("dinner at 8?".into(), false), ("what's the weather".into(), true), ("/new@OtherBot".into(), false)]);
    assert_eq!(fake.calls("inbound")[0]["place"]["kind"], "group");
}

#[tokio::test]
async fn topics_are_threads_and_replies_quote() {
    let (fake, api) = telegram().await;
    let mut u = message(1, "supergroup", OWNER, "and flights?");
    u["message"]["is_topic_message"] = json!(true);
    u["message"]["message_thread_id"] = json!(3);
    u["message"]["reply_to_message"] = json!({"message_id": 3, "forum_topic_created": {"name": "Trip"}, "from": {"id": 99}});
    api.push(u.clone());
    let e = fake.wait_call("inbound", 1).await;
    assert_eq!(e["thread"]["id"], "5/3");
    assert_eq!(e["place"], json!({"kind": "thread", "parent": "5", "title": "Trip"}));
    // The topic's start is no quote, but the bot opened it, so it is addressed.
    assert_eq!((e["reply_to"].is_null(), e["addressed"].as_bool()), (true, Some(true)));

    u["update_id"] = json!(2);
    u["message"]["reply_to_message"] = json!({"message_id": 9, "text": "Book it?", "from": {"id": 99}});
    api.push(u);
    let e = fake.wait_call("inbound", 2).await;
    assert_eq!(e["reply_to"], json!({"id": "9", "text": "Book it?", "mine": true}));
}

#[tokio::test]
async fn edits_presses_and_reactions_come_in() {
    let (fake, api) = telegram().await;
    let mut edit = message(1, "private", OWNER, "hi there");
    edit["edited_message"] = edit["message"].take();
    edit["edited_message"]["message_id"] = json!(4);
    api.push(edit);
    api.push(json!({"update_id": 2, "callback_query": {"id": "cb1", "from": {"id": OWNER, "first_name": "A"},
        "message": {"message_id": 1, "chat": {"id": CHAT, "type": "private"}}, "data": "k1.0"}}));
    api.push(json!({"update_id": 3, "message_reaction": {
        "chat": {"id": CHAT, "type": "private"}, "message_id": 40, "date": 0,
        "user": {"id": OWNER, "is_bot": false, "first_name": "Owner"},
        "old_reaction": [], "new_reaction": [{"type": "emoji", "emoji": "👍"}]}}));
    fake.wait_call("inbound", 3).await;
    let got = fake.calls("inbound");
    assert_eq!((got[0]["kind"].as_str(), got[0]["id"].as_str(), got[0]["text"].as_str()), (Some("edited"), Some("4"), Some("hi there")));
    assert_eq!((got[1]["kind"].as_str(), got[1]["button"].as_str()), (Some("press"), Some("k1.0")));
    assert_eq!((got[2]["kind"].as_str(), got[2]["message"].as_str(), got[2]["emoji"].as_str()), (Some("reaction"), Some("40"), Some("👍")));
    // The press is confirmed, which stops the button's spinner.
    assert_eq!(api.calls("answerCallbackQuery")[0]["callback_query_id"], "cb1");
}

#[tokio::test]
async fn sent_messages_are_html_with_inline_buttons() {
    let (fake, api) = telegram().await;
    let msg = json!({"text": "**Done**", "buttons": [[{"id": "a", "label": "✅ Allow"}, {"id": "d", "label": "❌ Deny"}]]});
    let id = messenger(&fake, "messenger_send", json!({"thread": "5", "message": msg})).await.unwrap();
    assert_eq!(id, "42");
    let sent = &api.calls("sendMessage")[0];
    assert_eq!((sent["text"].as_str(), sent["parse_mode"].as_str()), (Some("<b>Done</b>"), Some("HTML")));
    assert_eq!(sent["reply_markup"]["inline_keyboard"][0][1], json!({"text": "❌ Deny", "callback_data": "d"}));

    // In a topic, it goes to the topic.
    messenger(&fake, "messenger_send", json!({"thread": "5/77", "message": {"text": "plans go here"}})).await.unwrap();
    let topic = &api.calls("sendMessage")[1];
    assert_eq!((topic["chat_id"].as_i64(), topic["message_thread_id"].as_i64()), (Some(5), Some(77)));
}

#[tokio::test]
async fn html_telegram_cannot_parse_is_sent_as_plain_text() {
    let (fake, api) = telegram().await;
    let id = messenger(&fake, "messenger_send", json!({"thread": "5", "message": {"text": "**bad**"}})).await.unwrap();
    assert_eq!(id, "42");
    let sent = api.calls("sendMessage");
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("parse_mode").is_none());
    assert_eq!(sent[1]["text"], "**bad**");
}

#[tokio::test]
async fn reactions_commands_topics_and_pins_go_out() {
    let (fake, api) = telegram().await;
    messenger(&fake, "messenger_react", json!({"thread": "5", "id": "1", "emoji": "👀"})).await.unwrap();
    let set = &api.calls("setMessageReaction")[0];
    assert_eq!((set["message_id"].as_i64(), set["reaction"][0]["emoji"].as_str()), (Some(1), Some("👀")));

    let commands = json!([{"name": "new", "description": "New session"}, {"name": "goal", "description": "Set a goal"}]);
    messenger(&fake, "messenger_commands", json!({"commands": commands})).await.unwrap();
    assert_eq!(api.calls("setMyCommands")[0]["commands"][1], json!({"command": "goal", "description": "Set a goal"}));

    let thread = messenger(&fake, "messenger_open_thread", json!({"parent": "5", "title": "Trip"})).await.unwrap();
    assert_eq!(thread, "5/77");
    assert_eq!(api.calls("createForumTopic")[0]["name"], "Trip");

    // A pin without a message id never reaches Telegram.
    assert!(messenger(&fake, "messenger_action", json!({"thread": "5", "action": "pin", "args": {}})).await.is_err());
    messenger(&fake, "messenger_action", json!({"thread": "5", "action": "pin", "args": {"message": "1"}})).await.unwrap();
    let pins = api.calls("pinChatMessage");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0]["message_id"], 1);
}

#[tokio::test]
async fn files_download_and_failures_keep_the_token_out() {
    let (fake, _api) = telegram().await;
    let file = |id: &str| json!({"thread": "7", "file": {"id": id, "kind": "document"}});
    let got = messenger(&fake, "messenger_download", file("doc")).await.unwrap();
    assert_eq!(got, "JVBERg=="); // "%PDF", base64
    let err = messenger(&fake, "messenger_download", file("gone")).await.unwrap_err().to_string();
    assert!(err.contains("download failed") && !err.contains(&format!("bot{TOKEN}")), "{err}");
}

#[tokio::test]
async fn login_connects_the_bot_and_pairs_the_owner() {
    let api = BotApi::start().await;
    let (fake, august) = FakeAugust::new(&[("TELEGRAM_API_BASE", &api.http.url)]);
    fake.on("login_ask", |_| Ok(json!(TOKEN)));
    fake.on("login_choose", |_| Ok(json!("Allow")));
    tokio::spawn(serve(august));
    fake.started().await;

    let login = tokio::spawn({
        let fake = fake.clone();
        async move { fake.request("login", json!({"account": "telegram", "session": 1})).await }
    });
    // The token, then the owner messages the bot and is confirmed.
    fake.wait_until("pairing", |f| f.calls("login_progress").iter().any(|p| p["text"].as_str().unwrap().contains("https://t.me/AugustBot"))).await;
    api.push(message(1, "private", OWNER, "hi"));
    assert_eq!(login.await.unwrap().unwrap()["who"], "@AugustBot");
    assert!(fake.calls("login_choose")[0]["question"].as_str().unwrap().contains("Ann (@ann) (id 7)"));
    assert_eq!(fake.calls("secret_set")[0], json!({"key": "token", "value": TOKEN}));
    assert_eq!(fake.calls("config_set")[0], json!({"path": "extensions.telegram.settings.allowed", "value": [7]}));
    assert_eq!(api.wait_call("sendMessage").await["chat_id"], 7);

    // Now the owner talks to August in Telegram.
    api.push(message(2, "private", OWNER, "hello"));
    assert_eq!(fake.wait_call("inbound", 1).await["text"], "hello");
}
