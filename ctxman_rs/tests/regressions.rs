//! Regressionstests zu Befunden aus dem Code-Review: jeder Test nagelt ein Verhalten fest,
//! das vorher falsch war und in keiner Einzelsuite auffiel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ctxman::compaction::{
    CompactionModel, CompactionRequest, CompactionResult, FACT_EXTRACTION_TEMPLATE_ID,
};
use ctxman::domain::{PolicyConfig, RenderScope, Role, SegmentState, WatermarkLevel};
use ctxman::promotion::{PromotedFact, PromotionSink, VecPromotionSink};
use ctxman::{AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions};

fn small_policy(budget_tokens: u32) -> PolicyConfig {
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = budget_tokens;
    policy
}

fn openai() -> RenderOptions {
    RenderOptions {
        provider: "openai".to_string(),
        scope: RenderScope::Path,
        turn_advance: true,
    }
}

/// Liefert für die Fact-Extraction immer "fakt"; die Zusammenfassung scheitert bei den
/// ersten `failures` Aufrufen und liefert danach `summary`.
struct ScriptedModel {
    summary: &'static str,
    failures: usize,
    compaction_calls: AtomicUsize,
}

impl CompactionModel for ScriptedModel {
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        if request.prompt_template_id == FACT_EXTRACTION_TEMPLATE_ID {
            return Ok(CompactionResult {
                summary: "fakt".to_string(),
            });
        }
        if self.compaction_calls.fetch_add(1, Ordering::SeqCst) < self.failures {
            return Err(CtxmanError::Compaction("modell nicht erreichbar".into()));
        }
        Ok(CompactionResult {
            summary: self.summary.to_string(),
        })
    }
}

struct SharedSink(Arc<VecPromotionSink>);

impl PromotionSink for SharedSink {
    fn write(&self, fact: &PromotedFact, sink_url: &str) -> Result<(), CtxmanError> {
        self.0.write(fact, sink_url)
    }
}

fn session_with(model: ScriptedModel) -> (ContextSession, Arc<VecPromotionSink>) {
    let sink = Arc::new(VecPromotionSink::new());
    let services = CtxmanServices {
        compaction_model: Some(Box::new(model)),
        promotion_sink: Some(Box::new(SharedSink(sink.clone()))),
        clock: Box::new(|| 0),
        ..Default::default()
    };
    let mut session = ContextSession::new(small_policy(1000), services);
    for (kind, role, text) in [
        ("user_msg", Role::User, "eins"),
        ("assistant_msg", Role::Assistant, "zwei"),
    ] {
        session
            .append_segment(AppendRequest::inline(kind, Some(role), text))
            .unwrap();
    }
    session
        .append_segment(AppendRequest::inline(
            "user_msg",
            Some(Role::User),
            &"Frage".repeat(500),
        ))
        .unwrap();
    (session, sink)
}

#[test]
fn emergency_render_liefert_das_fragment_nach_der_eviction() {
    let mut session = ContextSession::new(small_policy(1000), CtxmanServices::default());
    // mcp_resource: TTL 3 Züge — 525 Tokens, die im Notfall fällig sind.
    session
        .append_segment(AppendRequest::inline(
            "mcp_resource",
            Some(Role::User),
            &"ALT".repeat(700),
        ))
        .unwrap();
    for _ in 0..5 {
        session.render(openai()).unwrap();
    }
    session
        .append_segment(AppendRequest::inline(
            "user_msg",
            Some(Role::User),
            &"n".repeat(1800),
        ))
        .unwrap();

    let out = session.render(openai()).unwrap();

    assert_eq!(out.watermark, WatermarkLevel::Emergency); // Stufe VOR der Eviction.
    assert_eq!(out.tokens_total, 450);
    assert!(
        !out.canonical_json.contains("ALTALT"),
        "das Fragment enthält noch das evictete Segment"
    );
    assert!(out.canonical_json.contains("nnnn"));
}

#[test]
fn major_gc_lehnt_leere_zusammenfassung_ab_und_laesst_den_zustand_stehen() {
    let (mut session, sink) = session_with(ScriptedModel {
        summary: "  \n",
        failures: 0,
        compaction_calls: AtomicUsize::new(0),
    });
    session.drain_events();

    let result = session.run_major_gc();

    assert!(matches!(result, Err(CtxmanError::Compaction(_))));
    assert_eq!(session.segments().len(), 3);
    assert!(session
        .segments()
        .iter()
        .all(|s| s.state() == SegmentState::Live));
    assert!(session.drain_events().is_empty());
    assert!(sink.facts().is_empty());
}

#[test]
fn major_gc_retry_promotet_den_fakt_nur_einmal() {
    let (mut session, sink) = session_with(ScriptedModel {
        summary: "Zusammenfassung",
        failures: 1,
        compaction_calls: AtomicUsize::new(0),
    });

    assert!(session.run_major_gc().is_err());
    assert!(sink.facts().is_empty(), "Sink-Write vor dem Modell-Call");

    let report = session.run_major_gc().unwrap();
    assert!(report.fact_promoted);
    assert_eq!(sink.facts().len(), 1);
}

#[test]
fn snapshot_ersetzt_die_datei_ohne_temp_reste() {
    let dir = std::env::temp_dir().join(format!("ctxman_snapshot_atomar_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("snapshot.json");

    let mut session = ContextSession::new(small_policy(1000), CtxmanServices::default());
    session.save_to_file(&path).unwrap();
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "neu"))
        .unwrap();
    session.save_to_file(&path).unwrap();

    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["snapshot.json".to_string()]);
    let loaded = ContextSession::load_from_file(&path, CtxmanServices::default()).unwrap();
    assert_eq!(loaded.segments().len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn openai_trennt_coalescte_textbloecke_mit_leerzeile() {
    let mut session = ContextSession::new(small_policy(1000), CtxmanServices::default());
    for text in ["Zusammenfassung.", "Naechste Frage"] {
        session
            .append_segment(AppendRequest::inline("user_msg", Some(Role::User), text))
            .unwrap();
    }

    let out = session.render(openai()).unwrap();

    assert_eq!(
        out.request_fragment["messages"][0]["content"],
        "Zusammenfassung.\n\nNaechste Frage"
    );
}

#[test]
fn expand_ref_ueber_1_mib_expandiert_gekuerzt_mit_hinweis() {
    let mut session = ContextSession::new(small_policy(1000), CtxmanServices::default());
    let total = 1_400_000; // 700 000 × "ä": Mehrbyte-Zeichen an der Schnittkante.
    let id = session
        .append_segment(AppendRequest {
            content: ctxman::AppendContent::Blob {
                content: "ä".repeat(700_000).into_bytes(),
                content_type: "text/plain".to_string(),
                summary: Some("gross".to_string()),
            },
            ..AppendRequest::inline("tool_result", Some(Role::User), "")
        })
        .unwrap();

    let out = session.expand_ref(id).unwrap();

    assert!(out.content.len() <= 1_048_576);
    assert!(out.content.starts_with("ää"));
    assert!(
        out.content
            .contains(&format!("von {total} Bytes expandiert")),
        "Hinweis fehlt: {}",
        &out.content[out.content.len() - 120..]
    );
    let expansion = session
        .segments()
        .iter()
        .find(|s| s.id() == out.expansion_segment_id)
        .unwrap();
    assert_eq!(expansion.content(), Some(out.content.as_str()));
}

#[test]
fn floats_in_tool_defs_rendern_ohne_panic_und_stabil() {
    let render = || {
        let mut session = ContextSession::new(small_policy(1000), CtxmanServices::default());
        session
            .bump_static_epoch(vec![ctxman::StaticSegmentSpec {
                kind: "tool_def".to_string(),
                role: None,
                content: r#"{"name":"t","parameters":{"properties":{"x":{"default":0.5}}}}"#
                    .to_string(),
                source: Some("core".to_string()),
            }])
            .unwrap();
        session.render(openai()).unwrap()
    };

    let (first, second) = (render(), render());

    assert!(first.canonical_json.contains(r#""default":0.5"#));
    assert_eq!(first.canonical_json, second.canonical_json);
    assert_eq!(first.cache_prefix_hash, second.cache_prefix_hash);
}
