//! The control socket, how the `august` program reaches the core: the commands extensions
//! add to it, the programs those run calling the core as their extension, and shutting down.

use crate::support::*;
use august::gateway::control::socket_in;
use august_ext::client::Client;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// One request on the control socket; its reply.
async fn ask(core: &Core, req: Value) -> Value {
    let stream = tokio::net::UnixStream::connect(socket_in(&core.home)).await.unwrap();
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let line = tokio::io::BufReader::new(read).lines().next_line().await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn extensions_add_commands_to_the_august_program() {
    let core = core()
        .ext("settings-app", |a| {
            a.register_cli("settings", "Edit the settings", &["my-settings".into(), "--full".into()]);
        })
        .start()
        .await;
    let listed = ask(&core, json!({"op": "cli"})).await;
    assert!(listed["result"].as_array().unwrap().contains(&json!({"name": "settings", "description": "Edit the settings", "owner": "settings-app"})), "{listed}");

    let opened = ask(&core, json!({"op": "cli_open", "name": "settings"})).await["result"].clone();
    assert_eq!((opened["exec"].clone(), opened["owner"].as_str()), (json!(["my-settings", "--full"]), Some("settings-app")));
    // The program it runs calls the core as that extension, with its permissions only.
    let mut client = Client::connect(&socket_in(&core.home), opened["token"].as_str().unwrap()).await.unwrap();
    assert_eq!(client.call("status", json!({})).await.unwrap()["model"], MODEL);
    let refused = client.call("extensions", json!({})).await.unwrap_err().to_string();
    assert!(refused.contains("admin"), "{refused}");
    let mut stranger = Client::connect(&socket_in(&core.home), "guess").await.unwrap();
    assert!(stranger.call("status", json!({})).await.is_err());

    assert!(ask(&core, json!({"op": "cli_open", "name": "nothing"})).await["error"].as_str().unwrap().contains("nothing"));
}

#[tokio::test]
async fn a_shutdown_tells_extensions_why_and_ends_the_core() {
    let reasons: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = reasons.clone();
    let core = core()
        .ext("watch", move |a| {
            let seen = seen.clone();
            a.on("shutdown", move |data, _| {
                seen.lock().unwrap().push(data["reason"].as_str().unwrap_or_default().into());
                async { Ok(None) }
            });
        })
        .start()
        .await;
    assert_eq!(ask(&core, json!({"op": "shutdown", "reason": "restart"})).await, json!({"result": null}));
    assert_eq!(*reasons.lock().unwrap(), ["restart"]);
    core.wait_until("the control socket to close", |c| !socket_in(&c.home).exists()).await;
}
