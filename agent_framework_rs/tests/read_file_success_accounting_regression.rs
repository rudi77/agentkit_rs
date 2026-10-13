use std::sync::Arc;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy, ToolRegistry, DELEGATE_NUDGE};
use serde_json::json;

fn turns() -> Vec<Vec<Chunk>> {
    vec![
        (0..4)
            .map(|i| Chunk::tool(i, &format!("r{i}"), "read_file", "{}"))
            .collect(),
        vec![Chunk::text("fertig")],
    ]
}

#[test]
fn failed_read_file_calls_do_not_count_towards_the_delegation_threshold() {
    let empty = json!({"type": "object", "properties": {}});
    let mut tools = ToolRegistry::new();
    tools.add("read_file", "fails to read", empty.clone(), |_| {
        Err("read failed".to_owned())
    });
    tools.add("task", "delegates", empty, |_| Ok("report".to_owned()));

    let llm = Arc::new(FakeLlm::new(turns()));
    let mut agent = Agent::builder(llm)
        .tools(tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();
    agent.run("Explain this crate.");

    assert!(
        !agent.memory.messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"].as_str() == Some(DELEGATE_NUDGE)
        }),
        "failed read_file calls must not trigger the delegation nudge"
    );
}
