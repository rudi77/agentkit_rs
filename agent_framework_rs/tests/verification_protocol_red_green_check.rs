use std::sync::Arc;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy, ToolEffect, ToolRegistry};
use serde_json::json;

fn registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.add("write_file", "writes", json!({"type":"object"}), |_| {
        Ok("written".into())
    });
    tools.add("sync_data", "mutates remote data", json!({"type":"object"}), |_| {
        Ok("synchronized".into())
    });
    tools.declare("sync_data", ToolEffect::Destructive);
    tools.add(
        "sync_required",
        "mutates remote data with required input",
        json!({"type":"object","required":["value"]}),
        |_| Ok("synchronized".into()),
    );
    tools.declare("sync_required", ToolEffect::Destructive);
    tools.add("sync_soft_failure", "reports a write failure", json!({"type":"object"}), |_| {
        Ok("ERROR: remote write rejected".into())
    });
    tools.declare("sync_soft_failure", ToolEffect::Destructive);
    tools.add("write_file", "fails to write", json!({"type":"object"}), |_| {
        Err("disk is read-only".into())
    });
    tools.add(
        "run_shell",
        "runs",
        json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        |args| {
            Ok(if args["phase"] == "before" {
                "exit=1\n--- STDOUT ---\nfailed\n--- STDERR ---\n".into()
            } else {
                "exit=0\n--- STDOUT ---\nok\n--- STDERR ---\n".into()
            })
        },
    );
    tools
}

#[test]
fn changed_work_requires_structured_red_green_evidence_and_module_check() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"regression","phase":"before"}"#)],
        vec![Chunk::tool(0, "write", "sync_data", "{}")],
        vec![Chunk::tool(0, "early-module", "run_shell", r#"{"command":"pytest -q","verification_kind":"module_or_project"}"#)],
        vec![Chunk::tool(0, "unannotated", "run_shell", r#"{"command":"pytest"}"#)],
        vec![Chunk::text("unverified final")],
        vec![Chunk::tool(0, "after", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"regression","phase":"after"}"#)],
        vec![Chunk::text("still missing module check")],
        vec![
            Chunk::tool(0, "parallel-after", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"regression","phase":"after"}"#),
            Chunk::tool(1, "parallel-module", "run_shell", r#"{"command":"pytest -q","verification_kind":"module_or_project"}"#),
        ],
        vec![Chunk::text("still missing sequential module check")],
        vec![Chunk::tool(0, "module", "run_shell", r#"{"command":"pytest -q","verification_kind":"module_or_project"}"#)],
        vec![Chunk::text("verified final")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(12)
        .build();

    assert_eq!(agent.run("fix it"), "verified final");
}

#[test]
fn declared_destructive_tool_requires_structured_verification() {
    let turns = vec![
        vec![Chunk::text("pre-sync status")],
        vec![Chunk::tool(0, "sync-1", "sync_data", "{}")],
        vec![Chunk::text("unverified after sync")],
        vec![Chunk::tool(0, "before", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"sync","phase":"before"}"#)],
        vec![Chunk::tool(0, "sync-2", "sync_data", "{}")],
        vec![
            Chunk::tool(0, "parallel-sync", "sync_data", "{}"),
            Chunk::tool(1, "parallel-after", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"sync","phase":"after"}"#),
            Chunk::tool(2, "parallel-module", "run_shell", r#"{"command":"pytest -q","verification_kind":"module_or_project"}"#),
        ],
        vec![Chunk::text("unverified parallel sync")],
        vec![Chunk::tool(0, "after", "run_shell", r#"{"command":"pytest -q regression","verification_kind":"reproduction","check_id":"sync","phase":"after"}"#)],
        vec![Chunk::tool(0, "module", "run_shell", r#"{"command":"pytest -q","verification_kind":"module_or_project"}"#)],
        vec![Chunk::text("verified after sync")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(12)
        .build();

    assert_eq!(agent.run("synchronize it"), "verified after sync");
}

#[test]
fn failed_write_does_not_require_verification() {
    let turns = vec![
        vec![Chunk::text("preflight")],
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::text("write failed")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(4)
        .build();

    assert_eq!(agent.run("write it"), "write failed");
}

#[test]
fn soft_failed_destructive_tool_does_not_require_verification() {
    let turns = vec![
        vec![Chunk::text("preflight")],
        vec![Chunk::tool(0, "soft-failure", "sync_soft_failure", "{}")],
        vec![Chunk::text("sync failed")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(4)
        .build();

    assert_eq!(agent.run("synchronize it"), "sync failed");
}

#[test]
fn invalid_destructive_tool_call_does_not_require_verification() {
    let turns = vec![
        vec![Chunk::text("preflight")],
        vec![Chunk::tool(0, "invalid-sync", "sync_required", "{")],
        vec![Chunk::text("sync was not executed")],
    ];

    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(4)
        .build();

    assert_eq!(agent.run("synchronize it"), "sync was not executed");
}
