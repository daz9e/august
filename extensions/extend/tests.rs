//! `extend` against a fake core: it brings the TypeScript host and the guide, and an
//! extension saved as TypeScript gets an `extension.json` that runs it through that host.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};

async fn extend() -> (FakeAugust, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (fake, august) = FakeAugust::new(&[("AUGUST_EXTENSIONS", dir.path().to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    (fake, dir)
}

#[tokio::test]
async fn a_typescript_extension_runs_through_the_host_it_brings() {
    let (fake, dir) = extend().await;
    let host = dir.path().join(".runtime/ts/host.ts");
    assert!(std::fs::read_to_string(&host).unwrap().contains("August extension host"));

    let code = "export default (august) => august.describe('Weather', 'Weather tool');";
    fake.tool("save_extension", json!({"name": "weather", "code": code}), ctx("1")).await.unwrap();
    let folder = dir.path().join("weather");
    assert_eq!(std::fs::read_to_string(folder.join("index.ts")).unwrap(), code);
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(folder.join("extension.json")).unwrap()).unwrap();
    assert_eq!(manifest["command"], json!(["bun", "run", host.display().to_string(), "index.ts", "weather"]));
    assert_eq!(fake.calls("extension_enable"), [json!({"name": "weather"})]);
}

#[tokio::test]
async fn the_guide_is_a_skill_that_points_at_the_host() {
    let (_fake, dir) = extend().await;
    let skill = std::fs::read_to_string(dir.path().join(".runtime/skills/writing-extensions/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: writing-extensions\n"));
    assert!(skill.contains(&dir.path().join(".runtime/ts/host.ts").display().to_string()) && !skill.contains("{host}"));
    assert!(skill.contains("declare module \"august\""));
}

#[tokio::test]
async fn other_languages_bring_their_own_extension_json() {
    let (fake, _dir) = extend().await;
    let err = fake.tool("save_extension", json!({"name": "py", "files": {"main.py": "print()"}}), ctx("1")).await.unwrap_err();
    assert!(err.to_string().contains("extension.json"), "{err}");
}
