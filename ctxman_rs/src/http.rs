//! HTTP-Adapter hinter dem Feature `http` (ureq, synchron): Anthropic-Compaction-Backend und
//! Webhook-Promotion-Senke. Credentials kommen ausschließlich vom Host (Non-Goal N5);
//! ctxman ruft NIEMALS das LLM des Agents auf (Non-Goal N1) — dies ist das ctxman-eigene,
//! günstige Compaction-Backend.

use serde_json::json;

use crate::compaction::{
    CompactionModel, CompactionRequest, CompactionResult, WindowItem, FACT_EXTRACTION_TEMPLATE_ID,
};
use crate::error::CtxmanError;
use crate::promotion::{PromotedFact, PromotionSink};

/// Compaction-LLM-Adapter für die Anthropic Messages API (Spec §8; Port von
/// `AnthropicCompactionModel.cs`). Zustandslos.
pub struct AnthropicCompactionModel {
    pub base_url: String,
    pub api_key: String,
    pub api_version: String,
    pub max_tokens: u32,
}

impl AnthropicCompactionModel {
    pub fn new(api_key: &str) -> Self {
        AnthropicCompactionModel {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: api_key.to_string(),
            api_version: "2023-06-01".to_string(),
            max_tokens: 1024,
        }
    }

    /// Minimale Built-in-Templates — Spec §8 verlangt keine spezifische Template-Registry.
    /// Wortgleich mit `CompactionPrompts.cs` des C#-Originals; die Begründungen stehen an
    /// [`SUMMARIZE_PROMPT`] und [`FACT_EXTRACTION_PROMPT`].
    fn resolve_system_prompt(template_id: &str) -> &'static str {
        match template_id {
            FACT_EXTRACTION_TEMPLATE_ID => FACT_EXTRACTION_PROMPT,
            _ => SUMMARIZE_PROMPT, // "default-v1" und alle anderen
        }
    }

    fn build_user_content(window: &[WindowItem]) -> String {
        window
            .iter()
            .map(|item| match item.label() {
                Some(label) => format!("{label}\n{}", item.content),
                None => item.content.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }
}

/// Feste Abschnitte statt „fasse knapp zusammen": nach der Compaction sieht das Modell des
/// Agents NUR noch diese Zusammenfassung — was sie nicht enthält, tut der Agent noch einmal.
/// Der Satz zur früheren Zusammenfassung ist nicht optional: ein älteres `compaction_summary`
/// ist live und landet beim nächsten Major GC im Fenster; ohne die Anweisung wird die
/// Zusammenfassung einer Zusammenfassung von Lauf zu Lauf dünner.
const SUMMARIZE_PROMPT: &str = "Summarize the following segments of an agent's context so that \
the agent can continue its work WITHOUT the original segments. If they contain an earlier \
summary (kind compaction_summary), carry its content over completely — it is replaced by \
yours. Use these sections (terse bullet points, but complete facts; omit empty sections):\n\
GOAL: the task in one sentence.\n\
DONE: what has been done.\n\
CHANGED: artifacts that were changed (e.g. files) — what changed.\n\
FINDINGS: facts that are still needed (causes, locations, commands that work).\n\
FAILED ATTEMPTS: what did not work and why — so it is not repeated.\n\
OPEN: what to do next.";

/// Der letzte Satz macht Spec §3.3 „leeres Summary = keine dauerhaften Fakten" überhaupt erst
/// erreichbar: ohne ihn antwortet ein Modell immer mit irgendetwas.
const FACT_EXTRACTION_PROMPT: &str = "Extract the durable facts from the segments below — \
decisions, constraints and learned invariants that remain true beyond this conversation — as a \
concise bulleted list. If there are none, reply with an empty response: no text at all, not \
even \"none\".";

/// Liest das Summary aus einer Messages-API-Antwort. Eine bei `max_tokens` abgeschnittene
/// Antwort ist KEIN Summary: die Compaction ersetzte die Quellen durch einen Text ohne Ende,
/// eine Fact-Extraction verlöre die hinteren Fakten. Spec §3.1: lieber ein expliziter,
/// retrybarer Fehler als ein lossy Notabwurf. Eigene Funktion, damit sie ohne Netz testbar ist.
fn summary_from_response(
    parsed: &serde_json::Value,
    max_tokens: u32,
    template_id: &str,
) -> Result<String, CtxmanError> {
    if parsed["stop_reason"] == "max_tokens" {
        return Err(CtxmanError::Compaction(format!(
            "Antwort bei max_tokens={max_tokens} abgeschnitten (Template {template_id}) — \
             max_tokens erhöhen"
        )));
    }
    Ok(parsed["content"]
        .as_array()
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b["type"] == "text")
                .and_then(|b| b["text"].as_str())
        })
        .unwrap_or_default()
        .to_string())
}

impl CompactionModel for AnthropicCompactionModel {
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        let body = json!({
            "model": request.model,
            "max_tokens": self.max_tokens,
            "system": Self::resolve_system_prompt(&request.prompt_template_id),
            "messages": [{ "role": "user", "content": Self::build_user_content(&request.window) }],
        });

        // Spec §8: Auth via x-api-key + anthropic-version (Non-Goal N5 — aus Konfiguration).
        let response = ureq::post(&format!(
            "{}/v1/messages",
            self.base_url.trim_end_matches('/')
        ))
        .set("x-api-key", &self.api_key)
        .set("anthropic-version", &self.api_version)
        .send_json(body)
        .map_err(|e| CtxmanError::Compaction(e.to_string()))?;

        let parsed: serde_json::Value = response
            .into_json()
            .map_err(|e| CtxmanError::Compaction(e.to_string()))?;

        let summary = summary_from_response(&parsed, self.max_tokens, &request.prompt_template_id)?;

        Ok(CompactionResult { summary })
    }
}

/// Webhook-Implementierung von [`PromotionSink`] (Spec §3.3 / §5; Port von
/// `WebhookPromotionSink.cs`): POST `{ fact, source_session, source_turn, kind }` (snake_case)
/// an die per-Session konfigurierte `promotion.sink.url`. Write-only (Non-Goal N2);
/// HTTP-Fehler propagieren als [`CtxmanError::Promotion`] — Retry obliegt dem Aufrufer.
pub struct WebhookPromotionSink;

impl PromotionSink for WebhookPromotionSink {
    fn write(&self, fact: &PromotedFact, sink_url: &str) -> Result<(), CtxmanError> {
        let body = serde_json::to_value(fact).expect("PromotedFact ist serialisierbar");
        ureq::post(sink_url)
            .send_json(body)
            .map_err(|e| CtxmanError::Promotion(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn abgeschnittene_antwort_ist_ein_fehler_kein_summary() {
        let parsed = json!({
            "content": [{ "type": "text", "text": "GOAL: halb" }],
            "stop_reason": "max_tokens",
        });
        let err = summary_from_response(&parsed, 1024, "default-v1").unwrap_err();
        assert!(err.to_string().contains("max_tokens"), "{err}");
    }

    #[test]
    fn vollstaendige_antwort_liefert_das_summary() {
        let parsed = json!({
            "content": [{ "type": "text", "text": "GOAL: fertig" }],
            "stop_reason": "end_turn",
        });
        assert_eq!(
            summary_from_response(&parsed, 1024, "default-v1").unwrap(),
            "GOAL: fertig"
        );
    }

    #[test]
    fn der_tool_name_erreicht_das_compaction_modell() {
        let content = AnthropicCompactionModel::build_user_content(&[WindowItem {
            content: r#"{"cmd":"cargo test"}"#.into(),
            kind: Some("tool_call".into()),
            source: Some("run_shell".into()),
        }]);
        assert!(content.starts_with("[tool_call: run_shell]\n"), "{content}");
    }

    #[test]
    fn fact_extraction_erlaubt_eine_leere_antwort() {
        let prompt = AnthropicCompactionModel::resolve_system_prompt(FACT_EXTRACTION_TEMPLATE_ID);
        assert!(prompt.contains("reply with an empty response"));
    }
}
