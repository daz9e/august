use super::*;
use serde_json::json;

pub struct LoadSkill;

#[async_trait]
impl Tool for LoadSkill {
    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn description(&self) -> &'static str {
        "Read the full instructions of a skill listed in the Skills section of the system prompt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Skill name"}},
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, _ctx: &ToolCtx) -> Result<String> {
        crate::skills::load(str_arg(input, "name")?)
    }
}

pub struct SaveSkill;

#[async_trait]
impl Tool for SaveSkill {
    fn name(&self) -> &'static str {
        "save_skill"
    }

    fn description(&self) -> &'static str {
        "Create or update a skill: a reusable how-to for a kind of task. Save one after \
         working out a non-trivial procedure the user is likely to ask for again, or when the \
         user asks you to. The description says when to use it (one line); the body holds \
         concrete steps, commands and gotchas."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "description": {"type": "string", "description": "One line: what it does and when to use it"},
                "body": {"type": "string", "description": "Markdown instructions"}
            },
            "required": ["name", "description", "body"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: &Value, _ctx: &ToolCtx) -> Result<String> {
        let (name, description, body) =
            (str_arg(input, "name")?, str_arg(input, "description")?, str_arg(input, "body")?);
        let path = crate::skills::save(name, description, body)?;
        Ok(format!("saved {}", path.display()))
    }
}

pub struct EditSkill;

#[async_trait]
impl Tool for EditSkill {
    fn name(&self) -> &'static str {
        "edit_skill"
    }

    fn description(&self) -> &'static str {
        "Change an existing skill without rewriting it: `patch` replaces one exact, unique \
         piece of SKILL.md (load the skill first and copy the text), `write_file` / \
         `remove_file` manage supporting files under references/, templates/ or scripts/ \
         (mention new ones in SKILL.md), `archive` retires an outdated skill (recoverable). \
         Fix wrong instructions in place instead of appending corrections."
    }

    fn input_schema(&self) -> Value {
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
        })
    }

    async fn call(&self, input: &Value, _ctx: &ToolCtx) -> Result<String> {
        let (action, name) = (str_arg(input, "action")?, str_arg(input, "name")?);
        Ok(match action {
            "patch" => {
                crate::skills::patch(name, str_arg(input, "old")?, str_arg(input, "new")?)?;
                format!("patched skill `{name}`")
            }
            "write_file" => {
                let path = crate::skills::write_file(name, str_arg(input, "file")?, str_arg(input, "content")?)?;
                format!("wrote {}", path.display())
            }
            "remove_file" => {
                crate::skills::remove_file(name, str_arg(input, "file")?)?;
                format!("removed {} from `{name}`", str_arg(input, "file")?)
            }
            "archive" => format!("archived skill `{name}` to {}", crate::skills::archive(name)?.display()),
            other => anyhow::bail!("unknown action `{other}`"),
        })
    }
}
