use agentkit::{run_strategy_from_str, PlanningOutput, RunStrategy};

#[test]
fn planning_strategy_is_non_executing_and_preserves_a_dependency_dag() {
    assert!(matches!(run_strategy_from_str("planning"), RunStrategy::Planning(_)));

    let output = PlanningOutput::from_json(
        r#"{
            "approach":"Only change the parser and its tests.",
            "constraints":["Do not change public output."],
            "rules":["Use focused tests."],
            "steps":[
                {"id":"inspect","step":"Inspect the parser","depends_on":[]},
                {"id":"change","step":"Implement the narrow change","depends_on":["inspect"]},
                {"id":"test","step":"Run focused tests","depends_on":["change"]}
            ]
        }"#,
        12,
    )
    .expect("a valid planning result");

    assert_eq!(output.steps[2].depends_on, vec!["change"]);
    assert_eq!(output.rules, vec!["Use focused tests."]);
    assert!(output.render().contains("test <- change"));

    let completed = std::collections::HashSet::from(["inspect"]);
    assert_eq!(output.ready_step_ids(&completed), vec!["change"]);
}
