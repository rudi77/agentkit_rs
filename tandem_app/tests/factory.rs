use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use serde_json::{json, Value};
use std::sync::Arc;
use tandem_app::{factory, repository::fingerprint};

fn job() -> Value {
    let code = "string Clean(string input) { return input; }";
    let hash = fingerprint(code.as_bytes());
    json!({"schema":"tandem.factory-job/v1","job_id":"job","session_id":"session","created_at":"2026-11-04","worker_policy":"proposal_only",
      "binding":{"plan_revision":1,"contract_id":"contract","contract_version":1,"baseline_sha256":hash,"disk_sha256":hash,"relative_path":"Demo.cs"},
      "contract":{"id":"contract","version":1,"plan_revision":1,"baseline_hash":hash,"path":"Demo.cs","start_line":1,"end_line":1,"mode":"pair","operations":["modify"],"preserve_public_signatures":true,"allow_new_symbols":false,"needs_now":"Trim","not_now":"Keine neuen Klassen","invariants":"","acceptance":"Leerzeichenfall prüfen","permissions":{"analysis":"read_only","code_write":"propose_only","shell":"none"}},
      "request":{"plan":{"revision":1,"goal":"Trim","not_now":"Keine neuen Klassen","acceptance":"Leerzeichenfall prüfen"},"snapshot":{"path":"Demo.cs","prefix":"","editable":code,"suffix":""},"mode":"pair","prompt":"Kleinste Änderung","history":[]}})
}

#[test]
fn worker_returns_bound_proposal_without_authority_or_test_evidence() {
    let bytes = serde_json::to_vec_pretty(&job()).unwrap();
    let prepared = factory::prepare(&bytes, None).unwrap();
    let patch = json!({"path":"Demo.cs","old_text":"return input;","new_text":"return input.Trim();","reason":"Randzeichen entfernen"});
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "patch", "propose_patch", &patch.to_string())],
        vec![Chunk::text("Vorschlag bereit")],
    ]));
    let result = factory::execute(prepared, llm, &new_cancel(), |_| {}).unwrap();
    assert_eq!(result.job_sha256, fingerprint(&bytes));
    assert_eq!(result.status, "proposal");
    assert_eq!(result.execution_authorization, "none");
    assert_eq!(result.verification, "not_run");
    assert_eq!(result.workspace_check, "not_requested");
    assert_eq!(
        result.result.patch.unwrap().new_text,
        "return input.Trim();"
    );
}

#[test]
fn inconsistent_or_privilege_expanding_jobs_fail_before_model_execution() {
    for (pointer, value) in [
        ("/worker_policy", json!("apply")),
        ("/binding/relative_path", json!("../secret")),
        ("/binding/relative_path", json!(".env")),
        ("/binding/contract_version", json!(2)),
        ("/contract/permissions/shell", json!("allowed")),
        ("/contract/operations", json!(["delete"])),
        ("/contract/end_line", json!(2)),
        ("/request/mode", json!("explore")),
        ("/request/snapshot/editable", json!("changed")),
        ("/contract/acceptance", json!("other")),
    ] {
        let mut changed = job();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            factory::prepare(&serde_json::to_vec(&changed).unwrap(), None).is_err(),
            "{pointer}"
        );
    }
    assert!(factory::prepare(&vec![b' '; factory::MAX_JOB_BYTES + 1], None).is_err());
}

#[test]
fn scope_conflict_is_a_decision_without_patch_or_expanded_authority() {
    let prepared = factory::prepare(&serde_json::to_vec(&job()).unwrap(), None).unwrap();
    let scope = json!({"reason":"Weiterer Port nötig","files":["Port.cs"],"symbols":["IPort.Read"],"alternative":"Vorhandenen Port nutzen","risk":"Vertrag erweitert"});
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "scope",
            "propose_scope_change",
            &scope.to_string(),
        )],
        vec![Chunk::text("Bitte klären")],
    ]));
    let result = factory::execute(prepared, llm, &new_cancel(), |_| {}).unwrap();
    assert_eq!(result.status, "decision_required");
    assert!(result.result.patch.is_none());
    assert!(result.result.scope_suggestion.is_some());
    assert_eq!(result.execution_authorization, "none");
}
