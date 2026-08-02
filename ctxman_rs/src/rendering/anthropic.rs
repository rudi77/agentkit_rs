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
        let messages = paare_tool_ergebnisse(messages);

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

        RenderResult {
            request_fragment,
            cache_breakpoints,
            builtin_tools,
        }
    }
}

/// Stellt die Ordnungszusicherung der Anthropic-API her: die `tool_result`-Blöcke
/// zu einer Assistant-Nachricht mit `tool_use` stehen in der UNMITTELBAR
/// FOLGENDEN Nachricht.
///
/// Dieselbe Naht wie in [`super::openai::paare_tool_antworten`] — und derselbe
/// Auslöser: `ManagedContext::messages` heilt eine verwaiste Unit (I5), indem es
/// ein Platzhalter-`tool_result` anhängt, und das bekommt die höchste `seq`. Bei
/// OpenAI kostete das 10 von 64 Polyglot-Tasks; hier war es bislang latent, weil
/// agentkit nur den OpenAI-Pfad fährt. Latent heißt nicht harmlos: ctxman ist
/// eine eigenständige Bibliothek, deren Anthropic-Adapter dieselbe Zusicherung
/// schuldet.
///
/// Anthropic ist sogar strenger als OpenAI — nicht „irgendwann danach", sondern
/// „in der nächsten Nachricht", und die `tool_result`-Blöcke müssen dort vorn
/// stehen. Beides erfüllt eine eigene User-Nachricht direkt hinter dem Aufruf.
///
/// Im Normalfall (Antwort folgt ohnehin direkt) ist die Ausgabe unverändert:
/// der Planer coalesced nur gleiche Rollen, `tool_result` (Rolle `Tool`) und
/// eine User-Nachricht landen also nie in derselben neutralen Nachricht.
///
/// Verlustfrei: ein `tool_result` ohne zugehörigen `tool_use` bleibt an seinem
/// Platz, statt verworfen zu werden.
fn paare_tool_ergebnisse(messages: Vec<Value>) -> Vec<Value> {
    use std::collections::{HashMap, HashSet};

    let benutzt: HashSet<String> = messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| b["id"].as_str().map(str::to_string))
        .collect();

    let gehoert_zu_aufruf = |block: &Value| {
        block["type"] == "tool_result"
            && block["tool_use_id"]
                .as_str()
                .is_some_and(|id| benutzt.contains(id))
    };

    // Zugeordnete Ergebnisse einsammeln; alles andere bleibt, wo es steht.
    let mut ergebnisse: HashMap<String, Vec<Value>> = HashMap::new();
    for message in &messages {
        for block in message["content"].as_array().into_iter().flatten() {
            if gehoert_zu_aufruf(block) {
                let id = block["tool_use_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                ergebnisse.entry(id).or_default().push(block.clone());
            }
        }
    }

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages {
        let blocks = message["content"].as_array().cloned().unwrap_or_default();
        let rest: Vec<Value> = blocks
            .iter()
            .filter(|b| !gehoert_zu_aufruf(b))
            .cloned()
            .collect();
        // Eine Nachricht, die NUR aus zugeordneten Ergebnissen bestand, fällt
        // weg — ihre Blöcke kommen gleich hinter ihrem Aufruf wieder.
        if !rest.is_empty() {
            out.push(json!({ "role": message["role"], "content": rest }));
        }

        let ids: Vec<String> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str().map(str::to_string))
            .collect();
        let antwort_blocks: Vec<Value> = ids
            .into_iter()
            .filter_map(|id| ergebnisse.remove(&id))
            .flatten()
            .collect();
        if !antwort_blocks.is_empty() {
            out.push(json!({ "role": "user", "content": antwort_blocks }));
        }
    }
    out
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
