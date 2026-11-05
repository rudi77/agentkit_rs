use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use serde_json::json;
use std::sync::Arc;
use tandem_app::{demo_llm, run, validate_patch, Patch, Plan, RunRequest, Snapshot};

fn request(mode: &str) -> RunRequest {
    RunRequest {
        plan: Plan {
            revision: 7,
            goal: "Leerzeichen entfernen".into(),
            not_now: "Keine neue Klasse".into(),
            acceptance: "Verhalten prüfen".into(),
        },
        snapshot: Snapshot {
            path: "Demo.cs".into(),
            prefix: "// Mensch: ä 🦀\n".into(),
            editable: "string Clean(string input) { return input; }".into(),
            suffix: "\n// Unverändert".into(),
        },
        mode: mode.into(),
        prompt: "Schlage die kleinste Änderung vor.".into(),
        history: vec![],
    }
}

fn patch() -> Patch {
    Patch {
        path: "Demo.cs".into(),
        old_text: "return input;".into(),
        new_text: "return input.Trim();".into(),
        reason: "Rand-Leerzeichen entfernen".into(),
    }
}

#[test]
fn patch_preserves_every_byte_outside_the_contract() {
    let r = request("pair");
    let content = validate_patch(&r.snapshot, &patch()).unwrap();
    assert_eq!(
        content,
        "// Mensch: ä 🦀\nstring Clean(string input) { return input.Trim(); }\n// Unverändert"
    );
}

#[test]
fn file_and_region_escapes_are_rejected() {
    let r = request("pair");
    let mut p = patch();
    p.path = "Other.cs".into();
    assert!(validate_patch(&r.snapshot, &p).is_err());
    p.path = "Demo.cs".into();
    p.old_text = "// Unverändert".into();
    assert!(validate_patch(&r.snapshot, &p).is_err());
    p.old_text.clear();
    assert!(validate_patch(&r.snapshot, &p).is_err());
}

#[test]
fn ambiguous_replacement_is_rejected() {
    let mut r = request("pair");
    r.snapshot.editable = "return input; return input;".into();
    assert!(validate_patch(&r.snapshot, &patch()).is_err());
}

#[test]
fn offline_demo_uses_the_real_agentkit_tool_loop() {
    let r = request("pair");
    let llm = demo_llm(&r);
    let mut tool_seen = false;
    let result = run(r, llm, &new_cancel(), |event| {
        if event.etype == agentkit::TOOL_CALL {
            tool_seen = true;
        }
    })
    .unwrap();
    assert!(tool_seen);
    assert_eq!(result.plan_revision, 7);
    assert_eq!(result.patch.unwrap().new_text, "return input.Trim();");
    assert!(result.answer.contains("noch nicht"));
}

#[test]
fn explore_has_no_patch_shell_or_file_write_tools() {
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Welche Leerzeichen sollen entfernt werden?",
    )]]));
    let result = run(request("explore"), llm.clone(), &new_cancel(), |_| {}).unwrap();
    assert!(result.patch.is_none());
    // Tool-Liste aus einem echten Modellaufruf wird weiter unten über einen
    // absichtlichen Aufruf geprüft: unbekannte Werkzeuge dürfen nichts ausführen.
    let attempts = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "write",
            "propose_patch",
            &serde_json::to_string(&patch()).unwrap(),
        )],
        vec![Chunk::tool(
            0,
            "shell",
            "run_shell",
            "{\"command\":\"echo nope\"}",
        )],
        vec![Chunk::text("Keine Schreibberechtigung.")],
    ]));
    let result = run(request("explore"), attempts, &new_cancel(), |_| {}).unwrap();
    assert!(result.patch.is_none());
}

#[test]
fn invalid_agent_patch_never_becomes_a_proposal() {
    let mut p = patch();
    p.path = "../escape.cs".into();
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "bad",
            "propose_patch",
            &serde_json::to_string(&p).unwrap(),
        )],
        vec![Chunk::text("Der Scope reicht nicht.")],
    ]));
    assert!(run(request("pair"), llm, &new_cancel(), |_| {})
        .unwrap()
        .patch
        .is_none());
}

#[test]
fn agent_can_suggest_but_not_replace_the_shared_plan() {
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "plan", "propose_plan", &json!({"goal":"Erst Null-Verhalten klären","not_now":"Keine Klasse","acceptance":"Null-Fall festlegen","reason":"Die Semantik ist offen"}).to_string())],
        vec![Chunk::text("Wie soll null behandelt werden?")],
    ]));
    let result = run(request("explore"), llm, &new_cancel(), |_| {}).unwrap();
    assert_eq!(result.plan_revision, 7);
    assert!(result.plan_suggestion.is_some());
    assert!(result.patch.is_none());
}

#[test]
fn canceled_run_returns_no_result() {
    let r = request("pair");
    let cancel = new_cancel();
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(run(r.clone(), demo_llm(&r), &cancel, |_| {}).is_err());
}
