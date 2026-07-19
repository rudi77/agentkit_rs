//! Major-GC-Beispiel (Spec §3.3): eigenes CompactionModel + PromotionSink implementieren,
//! Watermark-Empfehlung befolgen, Promotion VOR Compaction beobachten.
//!
//! Das CompactionModel ist hier bewusst offline (regelbasiert), damit das Beispiel ohne
//! API-Key läuft — in echt implementiert der Host das Trait über sein LLM-Backend
//! (z. B. agentkits `Llm`-Trait oder das Feature `http` mit `AnthropicCompactionModel`).
//!
//! Ausführen: `cargo run --example major_gc`

use ctxman::compaction::{CompactionModel, CompactionRequest, CompactionResult};
use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::promotion::{PromotedFact, PromotionSink};
use ctxman::{AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions};

/// Offline-„LLM": fasst das Fenster regelbasiert zusammen. `prompt_template_id`
/// unterscheidet Fact-Extraction (Promotion) von der eigentlichen Compaction.
struct RuleBasedModel;

impl CompactionModel for RuleBasedModel {
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        let summary = if request.prompt_template_id == "fact-extraction-v1" {
            // Promotion: nur „Entscheidungs"-Zeilen extrahieren; leer = keine Fakten.
            request
                .window
                .iter()
                .filter(|item| item.content.contains("Entscheidung:"))
                .map(|item| format!("- {}", item.content))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            // Compaction: ein kompaktes Summary über das ganze Fenster.
            format!(
                "[Zusammenfassung von {} Segmenten: {}]",
                request.window.len(),
                request
                    .window
                    .iter()
                    .map(|item| item.kind.as_deref().unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Ok(CompactionResult { summary })
    }
}

/// Eigene Promotion-Senke: druckt die Fakten (in echt: Webhook, Memory-Store, JSONL …).
struct StdoutSink;

impl PromotionSink for StdoutSink {
    fn write(&self, fact: &PromotedFact, _sink_url: &str) -> Result<(), CtxmanError> {
        println!("PROMOTION → dauerhafter Fakt gesichert:\n{}\n", fact.fact);
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Kleines Budget, damit die hard-Watermark (0,80·B) schnell erreicht ist —
    // die Beispiel-Konversation hat ~51 Tokens, 51/60 = 0,85 ⇒ hard.
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = 60;

    let services = CtxmanServices {
        compaction_model: Some(Box::new(RuleBasedModel)),
        promotion_sink: Some(Box::new(StdoutSink)),
        ..Default::default()
    };
    let mut session = ContextSession::new(policy, services);

    // Konversation, die das Budget füllt — inkl. einer Entscheidung (Promotion-Kandidat).
    session.append_segments(vec![
        AppendRequest::inline("user_msg", Some(Role::User), "Wie deployen wir den Service?"),
        AppendRequest::inline(
            "assistant_msg",
            Some(Role::Assistant),
            "Entscheidung: Wir deployen über Blue-Green auf AKS, Rollback via Slot-Swap.",
        ),
        AppendRequest::inline("user_msg", Some(Role::User), "Und die Datenbankmigration?"),
        AppendRequest::inline(
            "assistant_msg",
            Some(Role::Assistant),
            "Die Migration läuft vorab als Job, idempotent, mit Advisory-Lock.",
        ),
    ])?;

    let out = session.render(RenderOptions {
        provider: "anthropic".to_string(),
        scope: RenderScope::Path,
        turn_advance: true,
    })?;
    println!(
        "Render: tokens_total={} watermark={} empfohlene GC: {:?}\n",
        out.tokens_total, out.watermark, out.recommended_gc
    );

    // Die Empfehlung befolgen — bei `hard` ist das die Major Collection.
    if out.recommended_gc == Some(ctxman::gc::GcLevel::Major) {
        let report = session.run_major_gc()?;
        println!(
            "MAJOR GC → {} Segmente zu 1 Summary kompaktiert (Tokens {} → {}), Fakt promotet: {}\n",
            report.compacted_source_ids.len(),
            report.tokens_before,
            report.tokens_after,
            report.fact_promoted
        );
    }

    let after = session.render(RenderOptions {
        provider: "anthropic".to_string(),
        scope: RenderScope::Path,
        turn_advance: false,
    })?;
    println!("Nach der Compaction: tokens_total={} watermark={}", after.tokens_total, after.watermark);

    println!("\n— Events (fact_promoted kommt VOR compaction_started, Spec §3.3) —");
    for event in session.drain_events() {
        println!("seq={} {}", event.seq, event.event_type);
    }
    Ok(())
}
