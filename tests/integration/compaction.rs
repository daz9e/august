//! Context compaction: a long conversation is summarised in fixed sections, and a
//! later compaction updates the earlier summary instead of summarising it again.

use crate::support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn is_summary(req: &Value) -> bool {
    req["messages"][0]["content"].as_str().is_some_and(|s| s.contains("You compress conversations"))
}

fn user_text(req: &Value) -> String {
    let m = req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap();
    m["content"].as_str().map(String::from).unwrap_or_else(|| m["content"].to_string())
}

#[tokio::test]
async fn long_conversation_is_summarised_and_the_summary_updated() {
    let summaries = Arc::new(AtomicUsize::new(0));
    let n = summaries.clone();
    let llm: Llm = Box::new(move |req| {
        if is_summary(req) {
            let i = n.fetch_add(1, Ordering::SeqCst) + 1;
            reply_text(&format!("## Goal\nSUMMARY-{i}"))
        } else {
            reply_text("ok")
        }
    });
    let fake = Fake::start(Vec::new(), HashMap::new(), Some(llm)).await;
    let _gw = spawn_gateway_env(&fake, LlmSetup::Fake, &[], &[], &[("AUGUST_CONTEXT_TOKENS", "4000")]);

    let chat_calls = |f: &Fake| f.llm_requests().iter().filter(|r| !is_summary(r)).count();
    for i in 1..=14 {
        fake.push_updates(vec![message(i, json!({"text": format!("note {i}: {}", "lorem ipsum ".repeat(120))}))]);
        fake.wait_for(TIMEOUT, |f| chat_calls(f) >= i as usize).await;
    }

    let reqs = fake.llm_requests();
    let sums: Vec<&Value> = reqs.iter().filter(|r| is_summary(r)).collect();
    assert!(sums.len() >= 2, "expected two compactions, got {}", sums.len());
    let template = sums[0]["messages"][0]["content"].as_str().unwrap();
    for section in ["## Goal", "## Progress", "## Key Decisions", "## Relevant Files", "## Next Steps"] {
        assert!(template.contains(section), "{section} missing");
    }
    assert!(user_text(sums[0]).starts_with("Transcript to summarise"));
    // The second compaction gets the first summary to update, not as transcript text.
    let second = user_text(sums[1]);
    assert!(second.starts_with("Current summary:\n\n## Goal\nSUMMARY-1"), "{second}");
    assert_eq!(second.matches("SUMMARY-1").count(), 1);

    // The conversation continues on top of the newest summary.
    let last = reqs.iter().rev().find(|r| !is_summary(r)).unwrap().to_string();
    let newest = format!("SUMMARY-{}", sums.len());
    assert!(last.contains(&newest) && !last.contains("SUMMARY-1\\n"), "{last}");
}
