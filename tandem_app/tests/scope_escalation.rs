use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use serde_json::json;
use std::sync::Arc;
use tandem_app::{run, Plan, RunRequest, Snapshot};

#[test]
fn scope_conflict_removes_earlier_patch_and_blocks_later_patch_without_expanding_rights() {
    let patch = json!({"path":"Flow.cs","old_text":"return input;","new_text":"return input + 1;","reason":"Kleinster Eingriff"});
    let scope = json!({"reason":"Weiterer Port erforderlich","files":["Port.cs"],"symbols":["IPort.Read"],"alternative":"Bestehenden Port unverändert verwenden","risk":"Öffentlicher Vertrag würde erweitert"});
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "before",
            "propose_patch",
            &patch.to_string(),
        )],
        vec![Chunk::tool(
            0,
            "scope",
            "propose_scope_change",
            &scope.to_string(),
        )],
        vec![Chunk::tool(0, "after", "propose_patch", &patch.to_string())],
        vec![Chunk::text(
            "Scope-Konflikt: Bitte gemeinsam entscheiden, keine Codeänderung ausgeführt.",
        )],
    ]));
    let result = run(
        RunRequest {
            plan: Plan {
                revision: 3,
                goal: "Lesen ermöglichen".into(),
                not_now: "Keine zusätzlichen Dateien".into(),
                acceptance: "Gezielter Test".into(),
            },
            snapshot: Snapshot {
                path: "Flow.cs".into(),
                prefix: String::new(),
                editable: "return input;".into(),
                suffix: String::new(),
            },
            mode: "pair".into(),
            prompt: "Klein ändern".into(),
            history: vec![],
        },
        llm,
        &new_cancel(),
        |_| {},
    )
    .unwrap();
    assert!(result.patch.is_none());
    assert_eq!(result.scope_suggestion.unwrap().files, vec!["Port.cs"]);
    assert_eq!(result.plan_revision, 3);
}
