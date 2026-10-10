//! `config` against a fake core: the agent lists, reads and changes settings through the
//! core's `config_*` operations.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};

async fn config() -> FakeAugust {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake
}

async fn run(fake: &FakeAugust, input: Value) -> Result<String, String> {
    fake.tool("config", input, ctx("1")).await.map_err(|e| e.to_string())
}

#[tokio::test]
async fn list_shows_each_setting_with_its_default_and_description() {
    let fake = config().await;
    fake.on("config_list", |p| {
        Ok(match p["prefix"].as_str() {
            Some("") => json!([
                {"path": "august.model", "value": "m1", "description": "The active model"},
                {"path": "extensions.approvals.settings.judge", "value": null, "default": true, "description": "Let a separate model call"}
            ]),
            _ => json!([]),
        })
    });
    let list = run(&fake, json!({"action": "list"})).await.unwrap();
    assert_eq!(list, "august.model = \"m1\" — The active model\nextensions.approvals.settings.judge = null (default true) — Let a separate model call");
    assert_eq!(run(&fake, json!({"action": "list", "path": "nowhere"})).await.unwrap(), "no settings there");
}

#[tokio::test]
async fn get_and_set_go_by_path_and_set_shows_the_value_after() {
    let fake = config().await;
    fake.on("config_get", |p| Ok(if p["path"] == "august.effort" { json!("high") } else { json!({"judge": false}) }));

    let shown = run(&fake, json!({"action": "get", "path": "extensions.approvals.settings"})).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&shown).unwrap(), json!({"judge": false}));

    let set = run(&fake, json!({"action": "set", "path": "august.effort", "value": "high"})).await.unwrap();
    assert_eq!(set, "august.effort = \"high\"");
    assert_eq!(fake.calls("config_set"), vec![json!({"path": "august.effort", "value": "high"})]);

    // A path is needed for both; an unknown action is refused; neither changes anything.
    for action in ["get", "set"] {
        assert!(run(&fake, json!({"action": action})).await.unwrap_err().contains("needs a `path`"));
    }
    assert!(run(&fake, json!({"action": "drop", "path": "x"})).await.unwrap_err().contains("unknown action `drop`"));
    assert_eq!(fake.calls("config_set").len(), 1);
}

#[tokio::test]
async fn the_cores_refusal_reaches_the_agent() {
    let fake = config().await;
    fake.on("config_set", |_| Err(anyhow::anyhow!("blocked: the user denied it")));
    let err = run(&fake, json!({"action": "set", "path": "august.home", "value": null})).await.unwrap_err();
    assert!(err.contains("the user denied it"), "{err}");
    assert!(fake.calls("config_get").is_empty());
}
