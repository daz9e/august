//! `clarify`: the agent asks the user a question with a few answers to tap (or answer in
//! their own words) and waits, here up to ten minutes.

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(600);

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.needs(&["messaging"]);
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
            // A sub-agent or a scheduled task has nobody to ask.
            if ctx.turn.as_ref().is_some_and(|t| !t.show) {
                bail!("nobody can answer here (a background task); decide yourself and say what you assumed");
            }
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
            Ok(match ctx.ask(question, &options, WAIT).await? {
                Some(answer) if options.contains(&answer) => format!("The user chose: {answer}"),
                Some(words) => format!("The user answered in their own words: {words}"),
                None => "The user didn't answer within 10 minutes.".into(),
            })
        },
    );
    august.run().await;
}

#[cfg(test)]
mod tests;
