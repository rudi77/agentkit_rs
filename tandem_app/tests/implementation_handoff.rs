use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use serde_json::{json, Value};
use std::{fs, sync::Arc};
use tandem_app::{
    exploration::{explore, ExploreRequest},
    repository::Repository,
    Plan,
};

fn run_handoff(replace_plan: bool) {
    let root = std::env::temp_dir().join(format!(
        "tandem-handoff-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Example.cs"),
        "class Example { int Value() { return 1; } }\n",
    )
    .unwrap();
    let repo = Repository::open(&root, vec![]).unwrap();
    let source = repo.read_file("Example.cs", 1, 10).unwrap()["source"]["id"].clone();
    let plan = json!({"goal":"Wert ändern","not_now":"Keine anderen Dateien","open_questions":"","steps":[{"id":"change","title":"Vorhandene Methode ändern","why":"Anforderung bestätigt","files":["Example.cs"],"acceptance":"Rückgabe ist 2","source_ids":[source]}]});
    let mut turns = vec![
        vec![Chunk::tool(
            0,
            "invalid",
            "begin_implementation",
            "{\"step_id\":\"invented\"}",
        )],
        vec![Chunk::tool(
            0,
            "read",
            "read_file",
            "{\"path\":\"Example.cs\"}",
        )],
        vec![Chunk::tool(
            0,
            "plan",
            "propose_change_plan",
            &plan.to_string(),
        )],
        vec![Chunk::tool(
            0,
            "begin",
            "begin_implementation",
            "{\"step_id\":\"change\"}",
        )],
    ];
    if replace_plan {
        turns.push(vec![Chunk::tool(
            0,
            "replace",
            "propose_change_plan",
            &plan.to_string(),
        )]);
    }
    turns.push(vec![Chunk::text(
        "Die Vorbereitung eines prüfbaren Vorschlags ist angefordert, keine Datei geändert.",
    )]);
    let mut events = vec![];
    let result = explore(
        &root,
        ExploreRequest {
            prompt: "Setze den Plan jetzt um.".into(),
            plan: Plan {
                revision: 1,
                goal: "".into(),
                not_now: "".into(),
                acceptance: "".into(),
            },
            history: vec![],
            overlays: vec![],
        },
        Arc::new(FakeLlm::new(turns)),
        &new_cancel(),
        |event| events.push(serde_json::to_value(event).unwrap()),
    )
    .unwrap();
    assert_eq!(
        result.implementation_step_id,
        if replace_plan {
            None
        } else {
            Some("change".into())
        }
    );
    assert!(result.suggested_plan.is_some());
    assert!(!result.stale);
    assert!(events
        .iter()
        .any(|event: &Value| event.to_string().contains("Zuerst einen belegten Plan")));
    assert_eq!(
        fs::read_to_string(root.join("Example.cs")).unwrap(),
        "class Example { int Value() { return 1; } }\n"
    );
}

#[test]
fn implementation_handoff_requires_an_existing_evidence_bound_step_and_does_not_write() {
    run_handoff(false);
}

#[test]
fn replacing_the_plan_cancels_a_previous_implementation_handoff() {
    run_handoff(true);
}
