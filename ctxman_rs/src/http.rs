//! HTTP-Adapter hinter dem Feature `http` (ureq, synchron): Anthropic-Compaction-Backend und
//! Webhook-Promotion-Senke. Credentials kommen ausschließlich vom Host (Non-Goal N5);
//! ctxman ruft NIEMALS das LLM des Agents auf (Non-Goal N1) — dies ist das ctxman-eigene,
//! günstige Compaction-Backend.

use serde_json::json;

use crate::compaction::{CompactionModel, CompactionRequest, CompactionResult, WindowItem};
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
    fn resolve_system_prompt(template_id: &str) -> &'static str {
        match template_id {
            "fact-extraction-v1" => {
                "Extract the key facts and decisions from the conversation below as a concise bulleted list."
            }
            _ => "Summarize the following conversation segments concisely, preserving all essential context.",
        }
    }

    fn build_user_content(window: &[WindowItem]) -> String {
        window
            .iter()
            .map(|item| match &item.kind {
                Some(kind) => format!("[{kind}]\n{}", item.content),
                None => item.content.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }
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
        let response = ureq::post(&format!("{}/v1/messages", self.base_url.trim_end_matches('/')))
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", &self.api_version)
            .send_json(body)
            .map_err(|e| CtxmanError::Compaction(e.to_string()))?;

        let parsed: serde_json::Value = response
            .into_json()
            .map_err(|e| CtxmanError::Compaction(e.to_string()))?;

        let summary = parsed["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or_default()
            .to_string();

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
