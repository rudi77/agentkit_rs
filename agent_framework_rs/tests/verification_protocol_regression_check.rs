use std::sync::Arc;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy, ToolRegistry};
use serde_json::json;

fn registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.add("write_file", "writes", json!({"type":"object"}), |_| {
        Ok("written".into())
    });
    tools.add(
        "run_shell",
        "runs",
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        |_| Ok("exit=0\n--- STDOUT ---\nok\n--- STDERR ---\n".into()),
    );
    tools
}

#[test]
fn unannotated_green_shell_check_cannot_verify_a_change() {
    let turns = vec![
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::tool(0, "green", "run_shell", "{\"command\":\"pytest\"}")],
        vec![Chunk::text("incorrectly verified")],
        vec![Chunk::tool(0, "exception", "run_shell", "{\"command\":\"pytest\",\"verification_kind\":\"exception\",\"exception_reason\":\"external_system_unavailable\",\"exception_detail\":\"staging unavailable\"}")],
        vec![Chunk::text("verified only with documented exception")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(8)
        .build();

    assert_eq!(
        agent.run("fix it"),
        "verified only with documented exception"
    );
}
