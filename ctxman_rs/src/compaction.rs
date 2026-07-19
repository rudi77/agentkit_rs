//! Abstraktion für das ctxman-eigene Compaction-LLM-Backend (Spec §8). Ruft NIEMALS das LLM
//! des Agents auf (Non-Goal N1) — es ist ein separates, günstiges Modell, das ausschließlich
//! für Summarization und Fact-Extraction eingesetzt wird.

use crate::error::CtxmanError;

/// Prompt-Template der Compaction-Summarization (Spec §3.3 Schritt 2; Policy-Default).
pub const DEFAULT_TEMPLATE_ID: &str = "default-v1";
/// Prompt-Template der Promotion-Fact-Extraction (Spec §3.3 Schritt 1).
pub const FACT_EXTRACTION_TEMPLATE_ID: &str = "fact-extraction-v1";

/// Ein einzelnes Element der Fenster-Sammlung, die kompaktiert werden soll.
/// `content` ist Segment-Inhalt oder Summary (nie leer — Aufrufer filtert evicted);
/// `kind` dient dem Prompt als Kontext-Hinweis (Spec §2.2, offenes Vokabular).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowItem {
    pub content: String,
    pub kind: Option<String>,
}

/// Eingabe für [`CompactionModel::summarize`]. Trägt das Segment-Fenster zusammen mit der
/// Prompt-Template-ID und dem Modell-Bezeichner aus der Policy. Provider-agnostisch und
/// I/O-frei. Wird sowohl für Compaction-Summarization als auch für Promotion-Fact-Extraction
/// verwendet (unterschiedliche `prompt_template_id`, Spec §8).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionRequest {
    pub window: Vec<WindowItem>,
    pub prompt_template_id: String,
    pub model: String,
}

/// Ergebnis eines [`CompactionModel::summarize`]-Aufrufs. `run_major_gc` nutzt `summary` als
/// Content des neu angelegten `compaction_summary`-Segments (Spec §3.3); ein leeres Summary
/// bei der Fact-Extraction bedeutet „keine dauerhaften Fakten im Fenster".
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
}

/// Synchroner Port von `ICompactionModel`. Der Host implementiert das Trait — z. B. später in
/// agentkit über dessen `Llm`-Trait; die Signatur trägt bewusst nur eigene Structs/Strings.
pub trait CompactionModel: Send + Sync {
    /// Fasst das übergebene Segment-Fenster gemäß dem angegebenen Prompt-Template zusammen
    /// oder extrahiert Fakten daraus (je nach `prompt_template_id`).
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError>;
}
