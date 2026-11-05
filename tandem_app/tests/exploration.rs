use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use serde_json::json;
use std::{fs, sync::Arc};
use tandem_app::{
    exploration::{explore, validate_plan, ChangePlan, ChangeStep, ExploreRequest},
    repository::Repository,
    Plan,
};

#[test]
fn agent_finds_code_and_tests_and_proposes_an_evidence_bound_plan_without_writes() {
    let root = std::env::temp_dir().join(format!(
        "tandem-explore-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("AGENTS.md"),
        "Keine Änderungen, bestehende Tests unverändert lassen.",
    )
    .unwrap();
    fs::write(
        root.join("Gateway.cs"),
        "void DeleteRecord() { DeactivateAccount(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("tests/DeleteTests.cs"),
        "void DeleteRecord_old_soft_delete() {}\n",
    )
    .unwrap();
    let repo = Repository::open(&root, vec![]).unwrap();
    let code = repo.read_file("Gateway.cs", 1, 10).unwrap()["source"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let test = repo.read_file("tests/DeleteTests.cs", 1, 10).unwrap()["source"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let plan = ChangePlan {
        goal: "Hard-Delete eingrenzen".into(),
        not_now: "Keine PROD-Löschung".into(),
        open_questions: "Deployter SBF-Vertrag?".into(),
        steps: vec![ChangeStep {
            id: "scope".into(),
            title: "Vertrag prüfen".into(),
            why: "Der Gateway deaktiviert nur".into(),
            files: vec!["Gateway.cs".into(), "tests/DeleteTests.cs".into()],
            acceptance: "Neue Hard-Delete-Fälle getrennt planen".into(),
            source_ids: vec![code, test],
            details: None,
        }],
    };
    let llm=Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0,"find","search_code","{\"query\":\"DeleteRecord\"}")],
        vec![Chunk::tool(0,"read","read_file","{\"path\":\"Gateway.cs\"}")],
        vec![Chunk::tool(0,"tests","read_file","{\"path\":\"tests/DeleteTests.cs\"}")],
        vec![Chunk::tool(0,"plan","propose_change_plan",&serde_json::to_string(&plan).unwrap())],
        vec![Chunk::text("Implementierung und vorhandenen Test gefunden; fachliche Abdeckung ist noch zu klären. Keine Tests ausgeführt.")],
    ]));
    let request = ExploreRequest {
        prompt: "Untersuche Hard-Delete, nur lesend".into(),
        plan: Plan {
            revision: 2,
            goal: "Klären".into(),
            not_now: String::new(),
            acceptance: String::new(),
        },
        history: vec![],
        overlays: vec![],
    };
    let result = explore(&root, request, llm, &new_cancel(), |_| {}).unwrap();
    assert!(!result.stale);
    assert_eq!(result.suggested_plan.unwrap().steps.len(), 1);
    assert!(result.sources.iter().any(|s| s.kind == "test"));
    assert!(result.instruction_paths.contains(&"AGENTS.md".into()));
    assert_eq!(
        fs::read_to_string(root.join("Gateway.cs")).unwrap(),
        "void DeleteRecord() { DeactivateAccount(); }\n"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 3);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invented_sources_and_protected_planned_paths_are_rejected() {
    let mut plan:ChangePlan=serde_json::from_value(json!({"goal":"G","not_now":"N","open_questions":"?","steps":[{"id":"one","title":"T","why":"W","files":["Gateway.cs"],"acceptance":"A","source_ids":["invented"]}]})).unwrap();
    assert!(validate_plan(&plan, &[]).is_err());
    let source = tandem_app::repository::Source {
        id: "known".into(),
        path: "Gateway.cs".into(),
        sha256: "hash".into(),
        start_line: 1,
        end_line: 2,
        kind: "code".into(),
        editor: false,
    };
    plan.steps[0].source_ids = vec![source.id.clone()];
    plan.steps[0].files = vec!["../escape.cs".into()];
    assert!(validate_plan(&plan, std::slice::from_ref(&source)).is_err());
    plan.steps[0].files = vec!["appsettings.json".into()];
    assert!(validate_plan(&plan, &[source]).is_err());
}
