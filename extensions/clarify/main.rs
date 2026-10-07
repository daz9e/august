//! `clarify`: the agent asks the user a question with a few answers to tap, and waits.

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let august = August::new();
    august.register_tool(
        "clarify",
        "Ask the user a question with 2-6 short answers to choose from (buttons in the chat) and \
         wait for the choice. Use it when a decision is genuinely theirs and the options are \
         clear; for open questions just ask in your reply.",
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 6},
            },
            "required": ["question", "options"],
            "additionalProperties": false,
        }),
        |input, ctx| async move {
            let question = str_arg(&input, "question").trim();
            let options: Vec<String> = input["options"]
                .as_array()
                .into_iter()
                .flatten()
                .take(6)
                .map(|o| o.as_str().map(String::from).unwrap_or_else(|| o.to_string()))
                .collect();
            if question.is_empty() || options.len() < 2 {
                bail!("need a question and at least 2 options");
            }
            Ok(match ctx.ask(question, &options).await? {
                Some(answer) => format!("The user chose: {answer}"),
                None => "The user didn't answer within 5 minutes.".into(),
            })
        },
    );
    august.run().await;
}
