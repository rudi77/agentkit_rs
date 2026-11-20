use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use std::{fs, sync::Arc};
use tandem_app::{
    exploration::{explore, ExploreRequest},
    run, Plan, RunRequest, Snapshot, MAX_PROMPT_BYTES,
};

fn request(prompt: String) -> RunRequest {
    RunRequest {
        plan: Plan {
            revision: 1,
            goal: "Fehler verstehen".into(),
            not_now: "Keine Änderungen".into(),
            acceptance: "Befund".into(),
        },
        snapshot: Snapshot {
            path: "Example.cs".into(),
            prefix: String::new(),
            editable: "int Read() { return 1; }".into(),
            suffix: String::new(),
        },
        mode: "explore".into(),
        prompt,
        history: vec![],
    }
}

#[test]
fn long_chat_text_passes_both_adapter_paths() {
    let text = "Fehler äöü 🚀\n".repeat(6000) + "ENDE_DES_LOGS";
    assert!(text.len() > 48000);
    let result = run(
        request(text.clone()),
        Arc::new(FakeLlm::new(vec![vec![Chunk::text("Langer Text gelesen")]])),
        &new_cancel(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.answer, "Langer Text gelesen");
    assert!(result.patch.is_none());
    let root = std::env::temp_dir().join(format!(
        "tandem-long-input-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("Example.cs"), "int Read() { return 1; }").unwrap();
    let result = explore(
        &root,
        ExploreRequest {
            prompt: text,
            plan: request(String::new()).plan,
            history: vec![],
            overlays: vec![],
        },
        Arc::new(FakeLlm::new(vec![vec![Chunk::text(
            "Repository lesend geprüft",
        )]])),
        &new_cancel(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.answer, "Repository lesend geprüft");
    fs::remove_file(root.join("Example.cs")).unwrap();
    fs::remove_dir(root).unwrap();
}

#[test]
fn oversized_composed_context_fails_before_model_execution() {
    let result = run(
        request("x".repeat(MAX_PROMPT_BYTES + 1)),
        Arc::new(FakeLlm::new(vec![])),
        &new_cancel(),
        |_| {},
    );
    assert!(result.unwrap_err().contains("640 KB"));
}
