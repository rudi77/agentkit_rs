//! Persistenz-Beispiel: FileSystem-Blob-Store + JSON-Snapshot. Simuliert einen
//! Prozess-Neustart — die wiederhergestellte Session rendert byte-identisch
//! (gleicher `cache_prefix_hash`, I4) und der Page Fault findet die Blobs wieder.
//!
//! Ausführen: `cargo run --example persistence`

use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::storage::FileSystemBlobStore;
use ctxman::{
    AppendRequest, ContextSession, CtxmanServices, RenderOptions, StaticSegmentSpec,
};

fn services(blob_dir: &std::path::Path) -> CtxmanServices {
    CtxmanServices {
        blob_store: Box::new(FileSystemBlobStore::new(blob_dir)),
        ..Default::default()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::temp_dir().join("ctxman-example-persistence");
    let blob_dir = base.join("blobs");
    let snapshot_path = base.join("session.json");
    std::fs::create_dir_all(&base)?;

    // ── Prozess 1: Session aufbauen, externalisieren, Snapshot schreiben. ──
    let hash_before;
    {
        let mut policy = PolicyConfig::default_policy();
        policy.externalize_threshold_tokens = 20;
        let mut session = ContextSession::new(policy, services(&blob_dir));

        session.bump_static_epoch(vec![StaticSegmentSpec {
            kind: "system_prompt".to_string(),
            role: Some(Role::System),
            content: "Du bist ein Assistent mit Gedächtnis.".to_string(),
            source: Some("core".to_string()),
        }])?;
        session.append_segments(vec![
            AppendRequest {
                tool_call_id: Some("call_1".to_string()),
                source: Some("lies_datei".to_string()),
                ..AppendRequest::inline("tool_call", Some(Role::Assistant), r#"{"pfad":"notizen.md"}"#)
            },
            AppendRequest {
                tool_call_id: Some("call_1".to_string()),
                ..AppendRequest::inline(
                    "tool_result",
                    Some(Role::Tool),
                    &"Wichtige Notizen. ".repeat(10),
                )
            },
        ])?;

        // Externalisieren (Blob landet unter blob_dir) und Snapshot sichern.
        let report = session.run_minor_gc()?;
        println!("Prozess 1: {} Segment(e) in den Blob Store externalisiert.", report.externalized.len());

        let out = session.render(RenderOptions {
            provider: "anthropic".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        })?;
        hash_before = out.cache_prefix_hash.clone();

        session.save_to_file(&snapshot_path)?;
        println!("Prozess 1: Snapshot → {}\n", snapshot_path.display());
    } // Session wird gedroppt — „Prozess-Ende".

    // ── Prozess 2: Snapshot laden, identisch weiterarbeiten. ──
    let mut restored = ContextSession::load_from_file(&snapshot_path, services(&blob_dir))?;

    let out = restored.render(RenderOptions {
        provider: "anthropic".to_string(),
        scope: RenderScope::Path,
        turn_advance: false,
    })?;
    println!("Prozess 2: cache_prefix_hash identisch? {}", out.cache_prefix_hash == hash_before);

    // Der Page Fault findet den Blob über den content-addressed Key wieder.
    let externalized_id = restored
        .segments()
        .iter()
        .find(|s| s.state() == ctxman::domain::SegmentState::Externalized)
        .map(|s| s.id())
        .expect("externalisiertes Segment aus Prozess 1");
    let expanded = restored.expand_ref(externalized_id)?;
    println!(
        "Prozess 2: Page Fault liefert {} Zeichen aus dem Blob Store zurück.",
        expanded.content.len()
    );

    // Die Session arbeitet nahtlos weiter (seq/Versionen laufen monoton fort).
    restored.append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Weiter geht's."))?;
    println!(
        "Prozess 2: context_version={} nach dem Weiterarbeiten.",
        restored.session().context_version()
    );

    let _ = std::fs::remove_dir_all(&base);
    Ok(())
}
