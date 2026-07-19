//! End-to-End-Beispiel: Session anlegen, Static-Region setzen, Nachrichten appenden,
//! rendern, großes Tool-Ergebnis externalisieren (Minor GC) und per Page Fault zurückholen.
//!
//! Ausführen: `cargo run --example basic`

use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::storage::FileSystemBlobStore;
use ctxman::{
    AppendRequest, ContextSession, CtxmanServices, RenderOptions, StaticSegmentSpec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Policy mit kleiner Externalisierungs-Schwelle, damit das Beispiel sofort greift.
    let mut policy = PolicyConfig::default_policy();
    policy.externalize_threshold_tokens = 50;

    let blob_dir = std::env::temp_dir().join("ctxman-example-blobs");
    let services = CtxmanServices {
        blob_store: Box::new(FileSystemBlobStore::new(&blob_dir)),
        ..Default::default()
    };
    let mut session = ContextSession::new(policy, services);

    // 1) Static-Region über den Epoch-Bump setzen (Spec §4.2 — nie per Append, I1).
    session.bump_static_epoch(vec![
        StaticSegmentSpec {
            kind: "system_prompt".to_string(),
            role: Some(Role::System),
            content: "Du bist ein hilfreicher Agent.".to_string(),
            source: Some("core".to_string()),
        },
        StaticSegmentSpec {
            kind: "tool_def".to_string(),
            role: None,
            content: r#"{"name":"suche","description":"Websuche"}"#.to_string(),
            source: Some("core".to_string()),
        },
    ])?;

    // 2) Working-Segmente appenden: User-Frage + Tool-Roundtrip mit großem Ergebnis.
    session.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "Fasse mir die Doku zusammen.",
    ))?;
    session.append_segments(vec![
        AppendRequest {
            tool_call_id: Some("call_1".to_string()),
            source: Some("suche".to_string()),
            ..AppendRequest::inline("tool_call", Some(Role::Assistant), r#"{"q":"doku"}"#)
        },
        AppendRequest {
            tool_call_id: Some("call_1".to_string()),
            ..AppendRequest::inline(
                "tool_result",
                Some(Role::Tool),
                &"Sehr langes Suchergebnis. ".repeat(20),
            )
        },
    ])?;

    // 3) Rendern (Anthropic-Format): byte-stabiles Request-Fragment + Metadaten.
    let out = session.render(RenderOptions {
        provider: "anthropic".to_string(),
        scope: RenderScope::Path,
        turn_advance: true,
    })?;
    println!("— Render 1 —");
    println!("tokens_total={} watermark={}", out.tokens_total, out.watermark);
    println!("cache_prefix_hash={}", out.cache_prefix_hash);

    // 4) Minor GC: das große tool_result wandert in den Blob Store (Spec §3.2.2).
    let report = session.run_minor_gc()?;
    println!("— Minor GC — externalisiert: {:?}", report.externalized);

    // 5) Render zeigt jetzt den Ref-Hint statt des Roh-Contents.
    let out = session.render(RenderOptions {
        provider: "anthropic".to_string(),
        scope: RenderScope::Path,
        turn_advance: true,
    })?;
    println!("— Render 2 (nach Externalisierung) —");
    println!("{}", out.canonical_json);

    // 6) Page Fault: Inhalt über die Segment-ID zurückholen (Spec §3.4).
    if let Some(&segment_id) = report.externalized.first() {
        let expanded = session.expand_ref(segment_id)?;
        println!(
            "— Page Fault — {} Zeichen zurückgeholt, neues Segment {}",
            expanded.content.len(),
            expanded.expansion_segment_id
        );
    }

    // 7) Events (Spec §6) abholen — der Audit-Trail aller Operationen.
    println!("— Events —");
    for event in session.drain_events() {
        println!("seq={} {}", event.seq, event.event_type);
    }

    Ok(())
}
