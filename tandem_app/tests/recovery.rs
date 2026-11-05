use agentkit::{agent::new_cancel, testing::FakeLlm, Chunk};
use std::{fs, sync::Arc};
use tandem_app::{
    exploration::{explore, ExploreRequest},
    Plan,
};

#[test]
fn a_recoverable_search_error_does_not_discard_later_evidence() {
    let root = std::env::temp_dir().join(format!(
        "tandem-recovery-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Code.cs"), "class Code {}\n").unwrap();
    let model = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "missing",
            "read_file",
            "{\"path\":\"DoesNotExist.cs\"}",
        )],
        vec![Chunk::tool(
            0,
            "found",
            "read_file",
            "{\"path\":\"Code.cs\"}",
        )],
        vec![Chunk::text(
            "Die zuerst vermutete Datei existiert nicht. Code.cs wurde danach belegt gelesen.",
        )],
    ]));
    let request = ExploreRequest {
        prompt: "Finde den Code".into(),
        plan: Plan {
            revision: 1,
            goal: "Lesen".into(),
            not_now: String::new(),
            acceptance: String::new(),
        },
        history: vec![],
        overlays: vec![],
    };
    let result = explore(&root, request, model, &new_cancel(), |_| {}).unwrap();
    assert!(result.sources.iter().any(|source| source.path == "Code.cs"));
    assert!(!result.stale);
    fs::remove_dir_all(root).unwrap();
}
