//! Skills: reusable how-tos the agent loads on demand. Each lives in
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
//! Only names and descriptions go into the system prompt; the body is read with the
//! `load_skill` tool when it is relevant.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

pub fn dir() -> PathBuf {
    crate::config::home().join("skills")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Splits `---\nkey: value\n---\nbody` into the description and the body.
fn parse(text: &str) -> (Option<String>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text);
    };
    let description = rest[..end]
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().trim_matches('"').to_string());
    let body = rest[end + 4..].trim_start_matches(['\n', '\r']);
    (description, body)
}

/// Every valid skill, sorted by name. Unreadable folders are skipped.
pub fn list() -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut skills: Vec<Skill> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !valid_name(&name) {
                return None;
            }
            let text = std::fs::read_to_string(e.path().join("SKILL.md")).ok()?;
            let (description, _) = parse(&text);
            Some(Skill { name, description: description.unwrap_or_default() })
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// System-prompt section listing the available skills (empty when there are none).
pub fn prompt_section() -> String {
    let skills = list();
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\n## Skills\nThese skills hold instructions for specific tasks. When one matches the \
         request, call `load_skill` with its name and follow it.\n",
    );
    for k in skills {
        s += &format!("- {}: {}\n", k.name, k.description);
    }
    s
}

/// The skill body, plus the paths of any other files in its folder.
pub fn load(name: &str) -> Result<String> {
    if !valid_name(name) {
        bail!("invalid skill name `{name}`");
    }
    let folder = dir().join(name);
    let text = std::fs::read_to_string(folder.join("SKILL.md"))
        .with_context(|| format!("no skill named `{name}`"))?;
    let (_, body) = parse(&text);
    let mut out = body.to_string();
    let extra: Vec<String> = std::fs::read_dir(&folder)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != "SKILL.md")
        .map(|e| e.path().display().to_string())
        .collect();
    if !extra.is_empty() {
        out += &format!("\n\n[Other files of this skill]\n{}", extra.join("\n"));
    }
    Ok(out)
}

/// Creates or overwrites a skill.
pub fn save(name: &str, description: &str, body: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("skill names use lowercase letters, digits, `-` and `_` (max 64)");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter() {
        let (d, b) = parse("---\nname: x\ndescription: Does a thing\n---\nStep 1\n");
        assert_eq!(d.as_deref(), Some("Does a thing"));
        assert_eq!(b, "Step 1\n");
        let (d, b) = parse("just text");
        assert_eq!((d, b), (None, "just text"));
    }

    #[test]
    fn names() {
        assert!(valid_name("deploy-blog_2"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name("Has Space"));
        assert!(!valid_name(""));
    }
}
