//! The step budget of a turn: when it runs out, the model gets one call without tools
//! to report what it did, instead of the turn just stopping.

use crate::support::*;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test]
async fn out_of_steps_the_agent_reports_progress() {
    let llm: Llm = Box::new(|req| {
        let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
        if has_tools {
            reply_tool("shell", json!({"command": "echo step"}))
        } else {
            reply_text("Did 3 steps; the rest is left for next time.")
        }
    });
    let fake = Fake::start(vec![message(1, json!({"text": "loop forever"}))], HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_MAX_STEPS", "3")]);
    fake.wait_for(Duration::from_secs(30), |f| f.sent_texts().iter().any(|t| t.contains("Did 3 steps"))).await;

    let reqs = fake.llm_requests();
    assert_eq!(reqs.len(), 4);
    let last = reqs.last().unwrap().to_string();
    assert!(last.contains("Step limit reached"), "{last}");
}
