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

fn agent(turns: Vec<Vec<Chunk>>) -> Agent {
    Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(8)
        .build()
}

#[test]
fn arbitrary_green_test_does_not_satisfy_structured_verification() {
    let turns = vec![
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::tool(0, "test", "run_shell", "{\"command\":\"pytest\"}")],
        vec![Chunk::text("done")],
        vec![Chunk::tool(0, "exception", "run_shell", "{\"command\":\"pytest\",\"verification_kind\":\"exception\",\"exception_reason\":\"external_system_unavailable\",\"exception_detail\":\"staging unavailable\"}")],
        vec![Chunk::text("done with documented exception")],
    ];
    assert_eq!(agent(turns).run("fix it"), "done with documented exception");
}

#[test]
fn red_then_same_green_reproduction_plus_module_check_allows_final() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"before\"}")],
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::tool(0, "after", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"after\"}")],
        vec![Chunk::tool(0, "module", "run_shell", "{\"command\":\"pytest -q\",\"verification_kind\":\"module_or_project\"}")],
        vec![Chunk::text("verified")],
    ];
    assert_eq!(agent(turns).run("fix it"), "verified");
}

#[test]
fn declared_project_script_can_satisfy_structured_verification() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", "{\"command\":\"python scripts/verify_case.py\",\"verification_kind\":\"reproduction\",\"check_id\":\"verify-case\",\"phase\":\"before\"}")],
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::tool(0, "after", "run_shell", "{\"command\":\"python scripts/verify_case.py\",\"verification_kind\":\"reproduction\",\"check_id\":\"verify-case\",\"phase\":\"after\"}")],
        vec![Chunk::tool(0, "module", "run_shell", "{\"command\":\"python scripts/check_module.py\",\"verification_kind\":\"module_or_project\"}")],
        vec![Chunk::text("verified by project scripts")],
    ];
    assert_eq!(agent(turns).run("fix it"), "verified by project scripts");
}

#[test]
fn reproduction_without_module_check_is_not_enough() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"before\"}")],
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::tool(0, "after", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"after\"}")],
        vec![Chunk::text("done")],
        vec![Chunk::tool(0, "module", "run_shell", "{\"command\":\"pytest -q\",\"verification_kind\":\"module_or_project\"}")],
        vec![Chunk::text("verified after module check")],
    ];
    assert_eq!(agent(turns).run("fix it"), "verified after module check");
}

#[test]
fn later_change_invalidates_a_completed_protocol() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"before\"}")],
        vec![Chunk::tool(0, "write1", "write_file", "{}")],
        vec![Chunk::tool(0, "after", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"after\"}")],
        vec![Chunk::tool(0, "module", "run_shell", "{\"command\":\"pytest -q\",\"verification_kind\":\"module_or_project\"}")],
        vec![Chunk::tool(0, "write2", "write_file", "{}")],
        vec![Chunk::text("done")],
        vec![Chunk::tool(0, "exception", "run_shell", "{\"command\":\"pytest\",\"verification_kind\":\"exception\",\"exception_reason\":\"test_infrastructure_broken\",\"exception_detail\":\"runner unavailable after the final edit\"}")],
        vec![Chunk::text("done after invalidation")],
    ];
    assert_eq!(agent(turns).run("fix it"), "done after invalidation");
}

#[test]
fn same_batch_change_and_checks_do_not_prove_ordering() {
    let turns = vec![
        vec![Chunk::tool(0, "before", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"before\"}")],
        vec![
            Chunk::tool(0, "write", "write_file", "{}"),
            Chunk::tool(1, "after", "run_shell", "{\"command\":\"pytest -q regression\",\"verification_kind\":\"reproduction\",\"check_id\":\"regression\",\"phase\":\"after\"}"),
            Chunk::tool(2, "module", "run_shell", "{\"command\":\"pytest -q\",\"verification_kind\":\"module_or_project\"}"),
        ],
        vec![Chunk::text("done")],
        vec![Chunk::tool(0, "exception", "run_shell", "{\"command\":\"pytest\",\"verification_kind\":\"exception\",\"exception_reason\":\"test_infrastructure_broken\",\"exception_detail\":\"batch checks have no ordering proof\"}")],
        vec![Chunk::text("done only after exception")],
    ];
    assert_eq!(agent(turns).run("fix it"), "done only after exception");
}

#[test]
fn incomplete_protocol_at_step_limit_is_not_a_successful_final() {
    let turns = vec![
        vec![Chunk::tool(0, "write", "write_file", "{}")],
        vec![Chunk::text("claimed success")],
    ];
    let mut limited = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .max_steps(2)
        .build();
    assert_eq!(
        limited.run("fix it"),
        "(max_steps erreicht; Verifikation unvollständig)"
    );
}
