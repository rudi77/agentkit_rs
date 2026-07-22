use serde_json::Value;

use crate::domain::{Role, WatermarkLevel};

/// Art eines Content-Blocks innerhalb einer [`RenderMessage`] (Spec §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBlockKind {
    Text,
    ToolCall,
    ToolResult,
}

/// Ein Content-Block einer Working-Message. Provider-agnostisch — der Adapter mappt ihn in das
/// Wire-Format des Providers (z. B. `tool_result` als User-Block bei Anthropic vs. `role: tool`
/// bei OpenAI; Spec §4.6).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderContentBlock {
    pub kind: RenderBlockKind,
    pub text: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
}

/// Ein Element der Static-Region (System-Prompt oder `tool_def`; Spec §2.3, §4.6).
/// Static-Items sind bereits vom Planner kanonisch sortiert. `content_hash` dient dem
/// deterministischen Render-Prefix (Spec §4.6, I4).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderStaticItem {
    pub source: Option<String>,
    pub kind: String,
    pub content: String,
    pub content_hash: String,
    pub tokens: u32,
}

/// Eine Working-Region-Message: eine Rolle und ihre — durch Coalescing zusammengefassten —
/// Content-Blocks (Spec §4.6). Die Liste ist bereits seq-sortiert und coalesced vom Planner.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderMessage {
    pub role: Role,
    pub blocks: Vec<RenderContentBlock>,
}

/// Neutrales, bereits sortiertes und coalesced Render-Input für einen
/// [`super::adapter::ProviderAdapter`] (Spec §4.6). Trägt keine Provider-Semantik — der
/// Adapter erzeugt daraus das Wire-Format.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderModel {
    pub static_items: Vec<RenderStaticItem>,
    pub messages: Vec<RenderMessage>,
    pub emit_expand_context_ref: bool,
    pub tokens_total: i64,
}

/// Ergebnis des deterministischen Render-Plans (Spec §4.6). Der Planner wirft bei
/// unvollständigen Units NICHT — er meldet sie in `open_tool_call_ids`; der Aufrufer
/// entscheidet über den Fehler (Spec §2 I5).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderPlanResult {
    /// Neutrales, sortiertes und coalesced Render-Input für den Provider-Adapter.
    pub model: RenderModel,
    /// tool_call_ids gerenderter `tool_call`-Segmente ohne korrespondierendes render-eligibles
    /// `tool_result`. Leer ⇔ alle Units vollständig (Spec §2 I5).
    pub open_tool_call_ids: Vec<String>,
    /// Summe der pro-Segment gezählten Tokens (Zählung beim Append; keine Neuberechnung).
    pub tokens_total: i64,
    /// Abgeleiteter Watermark-Status ok|soft|hard|emergency (Spec §3.1).
    pub watermark: WatermarkLevel,
    /// Kanonisches Static-Prefix (Tool-Defs + System-Prompt), aus dem der Aufrufer den
    /// `cache_prefix_hash` berechnet (Spec §6, I4).
    pub static_prefix: Vec<RenderStaticItem>,
}

/// Art eines [`CacheBreakpoint`]. Mindestens das Ende der Static-Region wird markiert
/// (Spec §4.6) — sofern der Provider Prompt-Caching unterstützt, sonst leere Breakpoint-Liste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheBreakpointKind {
    StaticRegionEnd,
}

/// Markierung, an welcher Position des gerenderten Outputs ein Provider-Cache-Breakpoint
/// platziert werden soll (Spec §4.6). `index` ist die provider-spezifische Position
/// (z. B. Message-Index), die der Adapter auflöst.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheBreakpoint {
    pub kind: CacheBreakpointKind,
    pub index: usize,
}

/// Ergebnis eines [`super::adapter::ProviderAdapter::render`]-Aufrufs (Spec §4.3, §4.6).
/// `request_fragment` ist provider-spezifisch (Anthropic: `{ system, tools[], messages[] }`,
/// OpenAI: `{ tools[], messages[] }`). `builtin_tools` enthält das `expand_context_ref`-Tool
/// im Provider-Schema.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderResult {
    pub request_fragment: Value,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    pub builtin_tools: Vec<Value>,
}
