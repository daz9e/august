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
         concrete steps, commands and gotchas. Requires the user's approval."
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

    async fn call(&self, input: &Value, ctx: &ToolCtx) -> Result<String> {
        let (name, description, body) =
            (str_arg(input, "name")?, str_arg(input, "description")?, str_arg(input, "body")?);
        let exists = crate::skills::list().iter().any(|s| s.name == name);
        let verb = if exists { "update" } else { "create" };
        if !ctx.approver.approve(&format!("{verb} skill `{name}`: {description}")).await {
            anyhow::bail!("the user denied saving this skill");
        }
        let path = crate::skills::save(name, description, body)?;
        Ok(format!("saved {}", path.display()))
    }
}
