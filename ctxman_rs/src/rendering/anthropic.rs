use serde_json::{json, Value};

use super::adapter::ProviderAdapter;
use super::model::{
    CacheBreakpoint, CacheBreakpointKind, RenderBlockKind, RenderContentBlock, RenderMessage,
    RenderModel, RenderResult,
};
use super::static_prefix::parse_json_or_string;
use crate::domain::Role;

/// Provider-Adapter für die Anthropic Messages API (Spec §4.6). Erzeugt aus dem neutralen,
/// bereits sortierten und coalesced [`RenderModel`] das Anthropic-Wire-Format
/// `{ system, tools[], messages[] }`: der System-Prompt ist Top-Level-Feld (keine Message),
/// `tool_def`-Items wandern in den separaten `tools`-Parameter (nie in die Message-Liste),
/// `tool_result` wird als User-Block, `tool_call` als Assistant-`tool_use`-Block gemappt.
/// Zustandslos (Spec §11).
pub struct AnthropicMessagesAdapter;

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn provider(&self) -> &'static str {
        "anthropic"
    }

    fn render(&self, model: &RenderModel) -> RenderResult {
        // Spec §4.6: System-Prompt ist Top-Level-Feld, NICHT Teil der Message-Liste.
        // Mehrere system-Items werden zu einem System-Text zusammengefügt.
        let system = model
            .static_items
            .iter()
            .filter(|i| i.kind != "tool_def")
            .map(|i| i.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        // Spec §4.6: tool_def-Items als Anthropic-Tool-Schemas im separaten tools[]-Parameter —
        // nie Teil der Message-Liste.
        let tools: Vec<Value> = model
            .static_items
            .iter()
            .filter(|i| i.kind == "tool_def")
            .map(|i| parse_json_or_string(&i.content))
            .collect();

        let messages: Vec<Value> = model.messages.iter().map(build_message).collect();

        let tool_count = tools.len();
        let request_fragment = json!({
            "system": system,
            "tools": tools,
            "messages": messages,
        });

        // Spec §4.6: cache_breakpoints markieren mindestens das Ende der Static-Region.
        // Anthropic unterstützt Prompt-Caching ⇒ Breakpoint an der Static-Region-Grenze
        // (Index = Tool-Count).
        let cache_breakpoints = vec![CacheBreakpoint {
            kind: CacheBreakpointKind::StaticRegionEnd,
            index: tool_count,
        }];

        // Spec §3.4: expand_context_ref im Anthropic-Tool-Schema.
        let builtin_tools = vec![build_expand_context_ref_tool()];

        RenderResult { request_fragment, cache_breakpoints, builtin_tools }
    }
}

fn build_message(message: &RenderMessage) -> Value {
    // Spec §4.6: Anthropic kennt nur user/assistant. tool_result-Blöcke werden als
    // User-Content-Blöcke geführt; tool wird daher zu user gemappt.
    let role = match message.role {
        Role::Assistant => "assistant",
        _ => "user",
    };

    let content: Vec<Value> = message.blocks.iter().map(build_content_block).collect();

    json!({ "role": role, "content": content })
}

fn build_content_block(block: &RenderContentBlock) -> Value {
    match block.kind {
        // Spec §4.6: tool_call ⇒ assistant tool_use-Block.
        RenderBlockKind::ToolCall => json!({
            "type": "tool_use",
            "id": block.tool_call_id,
            "name": block.tool_name,
            "input": parse_json_or_string(block.text.as_deref().unwrap_or("")),
        }),
        // Spec §4.6: tool_result ⇒ user tool_result-Block (Anthropic-Konvention).
        RenderBlockKind::ToolResult => json!({
            "type": "tool_result",
            "tool_use_id": block.tool_call_id,
            "content": block.text.as_deref().unwrap_or(""),
        }),
        RenderBlockKind::Text => json!({
            "type": "text",
            "text": block.text.as_deref().unwrap_or(""),
        }),
    }
}

fn build_expand_context_ref_tool() -> Value {
    json!({
        "name": "expand_context_ref",
        "description": "Expand an externalized context segment by its segment_id to retrieve its full content.",
        "input_schema": {
            "type": "object",
            "properties": { "segment_id": { "type": "string" } },
            "required": ["segment_id"],
        },
    })
}
