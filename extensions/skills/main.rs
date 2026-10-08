//! `skills`: reusable how-tos the agent loads on demand. Each lives in
//! `~/.august/skills/<name>/SKILL.md` with a small frontmatter:
//!
//! ```text
//! ---
//! name: deploy-blog
//! description: Build and publish the blog (use when asked to deploy or publish it)
//! ---
//! Step-by-step instructions...
//! ```
//!
//! Only names and descriptions go into the system prompt; `load_skill` reads the body.
//! The agent writes and fixes skills with `save_skill` / `edit_skill`. Skills August ships (in the extensions' `.runtime/skills`) can't be changed.

use anyhow::{Context, Result, bail};
use august_ext::{August, str_arg};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Skill {
    name: String,
    description: String,
}

fn env_dir(key: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_default())
}

/// The user's skills.
fn dir() -> PathBuf {
    env_dir("AUGUST_HOME").join("skills")
}

/// The skills that ship with August.
fn shipped() -> PathBuf {
    env_dir("AUGUST_EXTENSIONS").join(".runtime/skills")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn is_shipped(name: &str) -> bool {
    shipped().join(name).join("SKILL.md").is_file()
}

/// Splits `---\nkey: value\n---\nbody` into the description and the body.
fn parse(text: &str) -> (Option<String>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text);
    };
    let description = rest[..end].lines().find_map(|l| l.strip_prefix("description:")).map(|d| d.trim().trim_matches('"').to_string());
    (description, rest[end + 4..].trim_start_matches(['\n', '\r']))
}

/// The skills in `root`.
fn skills_in(root: &Path) -> Vec<Skill> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let text = std::fs::read_to_string(e.path().join("SKILL.md")).ok().filter(|_| valid_name(&name))?;
            Some(Skill { name, description: parse(&text).0.unwrap_or_default() })
        })
        .collect()
}

/// Every skill, sorted by name; a shipped one wins over the user's of the same name.
fn list() -> Vec<Skill> {
    let mut skills = skills_in(&shipped());
    for s in skills_in(&dir()) {
        if !skills.iter().any(|k| k.name == s.name) {
            skills.push(s);
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn section() -> String {
    let mut s = String::from(
        "## Skills\nAfter solving a non-trivial, repeatable task, offer to save the procedure \
         with `save_skill`; when a skill turns out wrong or incomplete, fix it with `edit_skill`.",
    );
    let skills = list();
    if !skills.is_empty() {
        s += "\nThese skills hold instructions for specific tasks. When one matches the request, call \
              `load_skill` with its name and follow it.\n";
        for k in skills {
            s += &format!("- {}: {}\n", k.name, k.description);
        }
    }
    s
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()) {
        let path = e.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The skill body, plus the paths of any other files in its folder.
fn load(name: &str) -> Result<String> {
    if !valid_name(name) {
        bail!("invalid skill name `{name}`");
    }
    let folder = if is_shipped(name) { shipped().join(name) } else { dir().join(name) };
    let text = std::fs::read_to_string(folder.join("SKILL.md")).with_context(|| format!("no skill named `{name}`"))?;
    let mut out = parse(&text).1.to_string();
    let mut extra = Vec::new();
    files_under(&folder, &mut extra);
    extra.retain(|p| p != &folder.join("SKILL.md"));
    extra.sort();
    if !extra.is_empty() {
        let extra: Vec<String> = extra.iter().map(|p| p.display().to_string()).collect();
        out += &format!("\n\n[Supporting files of this skill; read them with `read` when needed]\n{}", extra.join("\n"));
    }
    Ok(out)
}

/// Creates or overwrites a skill.
fn save(name: &str, description: &str, body: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("skill names use lowercase letters, digits, `-` and `_` (max 64)");
    }
    if is_shipped(name) {
        bail!("`{name}` is a built-in skill; pick another name");
    }
    if description.trim().is_empty() || description.contains('\n') {
        bail!("description must be one non-empty line");
    }
    let folder = dir().join(name);
    std::fs::create_dir_all(&folder)?;
    let path = folder.join("SKILL.md");
    std::fs::write(&path, format!("---\nname: {name}\ndescription: {}\n---\n{}\n", description.trim(), body.trim()))?;
    Ok(path)
}

/// Folder of a skill the agent may change (not a shipped one).
fn own_folder(name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("invalid skill name `{name}`");
    }
    if is_shipped(name) {
        bail!("`{name}` is a built-in skill and can't be changed");
    }
    let folder = dir().join(name);
    if !folder.join("SKILL.md").is_file() {
        bail!("no skill named `{name}`");
    }
    Ok(folder)
}

/// Replaces the one occurrence of `old` in a skill's SKILL.md with `new`.
fn patch(name: &str, old: &str, new: &str) -> Result<()> {
    let path = own_folder(name)?.join("SKILL.md");
    let text = std::fs::read_to_string(&path)?;
    match text.matches(old).count() {
        0 => bail!("`old` was not found in {name}/SKILL.md; load the skill and copy the text exactly"),
        1 => {}
        n => bail!("`old` occurs {n} times in {name}/SKILL.md; include more context to make it unique"),
    }
    let patched = text.replacen(old, new, 1);
    if parse(&patched).0.is_none_or(|d| d.is_empty()) {
        bail!("the patch would break the skill's frontmatter (name/description)");
    }
    std::fs::write(&path, patched)?;
    Ok(())
}

/// Moves a skill to `skills/.archive/` (recoverable). Returns where it went.
fn archive(name: &str) -> Result<PathBuf> {
    let folder = own_folder(name)?;
    let archive = dir().join(".archive");
    std::fs::create_dir_all(&archive)?;
    let to = archive.join(format!("{name}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()));
    std::fs::rename(&folder, &to)?;
    Ok(to)
}

/// `references/x.md`-style path inside a skill folder; anything else is refused.
fn support_path(name: &str, file: &str) -> Result<PathBuf> {
    let ok_dir = ["references/", "templates/", "scripts/"].iter().any(|d| file.starts_with(d));
    let parts_ok = file.split('/').all(|p| !p.is_empty() && p != "." && p != "..");
    if !ok_dir || !parts_ok || file.contains('\\') {
        bail!("supporting files go under references/, templates/ or scripts/ (e.g. references/api.md)");
    }
    Ok(own_folder(name)?.join(file))
}

fn save_skill(input: &Value) -> Result<String> {
    let (name, description, body) = (str_arg(input, "name"), str_arg(input, "description"), str_arg(input, "body"));
    Ok(format!("saved {}", save(name, description, body)?.display()))
}

fn edit_skill(input: &Value) -> Result<String> {
    let (action, name) = (str_arg(input, "action"), str_arg(input, "name"));
    let (old, new, file, content) = (str_arg(input, "old"), str_arg(input, "new"), str_arg(input, "file"), str_arg(input, "content"));
    Ok(match action {
        "patch" => {
            patch(name, old, new)?;
            format!("patched skill `{name}`")
        }
        "write_file" => {
            let path = support_path(name, file)?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, content)?;
            format!("wrote {}", path.display())
        }
        "remove_file" => {
            std::fs::remove_file(support_path(name, file)?).with_context(|| format!("no file {file} in skill `{name}`"))?;
            format!("removed {file} from `{name}`")
        }
        "archive" => format!("archived skill `{name}` to {}", archive(name)?.display()),
        other => bail!("unknown action `{other}`"),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.register_prompt_section("skills", &section());
    // Skills added or changed by hand show up in the next conversation.
    let a = august.clone();
    august.on("session_start", move |_, _| {
        let a = a.clone();
        async move {
            a.register_prompt_section("skills", &section());
            Ok(None)
        }
    });
    august.register_tool(
        "load_skill",
        "Read the full instructions of a skill listed in the Skills section of the system prompt.",
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Skill name"}},
            "required": ["name"],
            "additionalProperties": false
        }),
        |input, _| async move { load(str_arg(&input, "name")) },
    );
    let a = august.clone();
    august.register_tool(
        "save_skill",
        "Create or update a skill: a reusable how-to for a kind of task. Save one after \
         working out a non-trivial procedure the user is likely to ask for again, or when the \
         user asks you to. The description says when to use it (one line); the body holds \
         concrete steps, commands and gotchas.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "description": {"type": "string", "description": "One line: what it does and when to use it"},
                "body": {"type": "string", "description": "Markdown instructions"}
            },
            "required": ["name", "description", "body"],
            "additionalProperties": false
        }),
        move |input, _| {
            let a = a.clone();
            async move {
                let out = save_skill(&input)?;
                a.register_prompt_section("skills", &section());
                Ok(out)
            }
        },
    );
    let a = august.clone();
    august.register_tool(
        "edit_skill",
        "Change an existing skill without rewriting it: `patch` replaces one exact, unique \
         piece of SKILL.md (load the skill first and copy the text), `write_file` / \
         `remove_file` manage supporting files under references/, templates/ or scripts/ \
         (mention new ones in SKILL.md), `archive` retires an outdated skill (recoverable). \
         Fix wrong instructions in place instead of appending corrections.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["patch", "write_file", "remove_file", "archive"]},
                "name": {"type": "string", "description": "Skill name"},
                "old": {"type": "string", "description": "patch: exact text to replace"},
                "new": {"type": "string", "description": "patch: replacement text"},
                "file": {"type": "string", "description": "write_file/remove_file: e.g. references/api.md"},
                "content": {"type": "string", "description": "write_file: file contents"}
            },
            "required": ["action", "name"],
            "additionalProperties": false
        }),
        move |input, _| {
            let a = a.clone();
            async move {
                let out = edit_skill(&input)?;
                a.register_prompt_section("skills", &section());
                Ok(out)
            }
        },
    );
    august.run().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter() {
        let (d, b) = parse("---\nname: x\ndescription: Does a thing\n---\nStep 1\n");
        assert_eq!(d.as_deref(), Some("Does a thing"));
        assert_eq!(b, "Step 1\n");
        assert_eq!(parse("just text"), (None, "just text"));
    }
}
