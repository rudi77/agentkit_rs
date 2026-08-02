use serde_json::{json, Value};

use super::adapter::ProviderAdapter;
use super::model::{RenderBlockKind, RenderContentBlock, RenderMessage, RenderModel, RenderResult};
use super::static_prefix::parse_json_or_string;
use crate::domain::Role;

/// Provider-Adapter für die OpenAI Chat Completions API (Spec §4.6). Erzeugt aus dem neutralen,
/// bereits sortierten und coalesced [`RenderModel`] das OpenAI-Wire-Format
/// `{ tools[], messages[] }`: der System-Prompt ist die ERSTE Message mit `role: system`
/// (kein Top-Level-Feld), `tool_def`-Items wandern in den separaten `tools`-Parameter
/// (nie in die Message-Liste), `tool_call` wird zur Assistant-Message mit `tool_calls`,
/// `tool_result` zur Message mit `role: tool`. Zustandslos (Spec §11).
pub struct OpenAiChatAdapter;

impl ProviderAdapter for OpenAiChatAdapter {
    fn provider(&self) -> &'static str {
        "openai"
    }

    fn render(&self, model: &RenderModel) -> RenderResult {
        let mut messages: Vec<Value> = Vec::new();

        // Spec §4.6: System-Prompt ist bei OpenAI die ERSTE Message (role: system),
        // kein Top-Level-Feld.
        let system = model
            .static_items
            .iter()
            .filter(|i| i.kind != "tool_def")
            .map(|i| i.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }

        // Spec §4.6: tool_def-Items als OpenAI-Function-Tool-Schemas im separaten
        // tools[]-Parameter — nie Teil der Message-Liste.
        let tools: Vec<Value> = model
            .static_items
            .iter()
            .filter(|i| i.kind == "tool_def")
            .map(|i| json!({ "type": "function", "function": parse_json_or_string(&i.content) }))
            .collect();

        for message in &model.messages {
            build_messages(message, &mut messages);
        }
        let messages = paare_tool_antworten(messages);

        let request_fragment = json!({ "tools": tools, "messages": messages });

        // Spec §4.6: OpenAI Chat Completions unterstützt keine empfohlenen Cache-Breakpoints ⇒ leer.
        let cache_breakpoints = Vec::new();

        // Spec §3.4: expand_context_ref im OpenAI-Function-Schema.
        let builtin_tools = vec![build_expand_context_ref_tool()];

        RenderResult {
            request_fragment,
            cache_breakpoints,
            builtin_tools,
        }
    }
}

/// Stellt die Ordnungszusicherung der OpenAI-API her: auf eine Assistant-Nachricht
/// mit `tool_calls` folgen UNMITTELBAR die Antworten zu jeder `tool_call_id`.
///
/// Der Render sortiert Working-Segmente strikt nach `seq` (I4) — das genügt,
/// solange eine Antwort direkt nach ihrem Aufruf entsteht. Es genügt NICHT,
/// wenn sie nachgereicht wird: `ManagedContext::messages` heilt eine verwaiste
/// Unit (I5), indem es ein Platzhalter-`tool_result` anhängt, und das bekommt
/// die höchste `seq`. Die Antwort stand danach dutzende Nachrichten hinter
/// ihrem Aufruf, und der Provider lehnte den ganzen Request ab:
///
/// > An assistant message with 'tool_calls' must be followed by tool messages
/// > responding to each 'tool_call_id'
///
/// Beobachtet in 10 von 64 Polyglot-Tasks (Lauf `verifyfix`) — jedes Mal ein
/// Totalausfall des Tasks, nicht bloß ein schlechteres Ergebnis.
///
/// Die Zusicherung gehört in den Adapter und nicht in den Planer: sie ist eine
/// Eigenheit des OpenAI-Wire-Formats. Anthropic führt `tool_result` als
/// User-Block und kennt sie nicht.
///
/// Verlustfrei: eine Antwort ohne zugehörigen Aufruf bleibt an ihrem Platz,
/// statt verworfen zu werden.
fn paare_tool_antworten(messages: Vec<Value>) -> Vec<Value> {
    use std::collections::{HashMap, HashSet};

    // Welche tool_call_ids ruft überhaupt jemand auf?
    let aufgerufen: HashSet<String> = messages
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();

    // Antworten einsammeln — nur die, die zu einem Aufruf gehören.
    let mut antworten: HashMap<String, Vec<Value>> = HashMap::new();
    for m in &messages {
        if m["role"] != "tool" {
            continue;
        }
        if let Some(id) = m.get("tool_call_id").and_then(Value::as_str) {
            if aufgerufen.contains(id) {
                antworten.entry(id.to_string()).or_default().push(m.clone());
            }
        }
    }

    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if m["role"] == "tool" {
            let zugeordnet = m
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| aufgerufen.contains(id));
            // Zugeordnete Antworten kommen weiter unten hinter ihren Aufruf;
            // verwaiste bleiben, wo sie sind.
            if !zugeordnet {
                out.push(m);
            }
            continue;
        }
        let ids: Vec<String> = m
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(m);
        for id in ids {
            if let Some(liste) = antworten.remove(&id) {
                out.extend(liste);
            }
        }
    }
    out
}

fn build_messages(message: &RenderMessage, out: &mut Vec<Value>) {
    // Spec §4.6: tool_result ⇒ je eigene Message mit role: tool (kein Content-Block-Array).
    if message
        .blocks
        .iter()
        .any(|b| b.kind == RenderBlockKind::ToolResult)
    {
        for block in message
            .blocks
            .iter()
            .filter(|b| b.kind == RenderBlockKind::ToolResult)
        {
            out.push(json!({
                "role": "tool",
                "tool_call_id": block.tool_call_id,
                "content": block.text.as_deref().unwrap_or(""),
            }));
        }
        // Übrige Blöcke derselben Nachricht NICHT verschlucken. Vorher stand
        // hier ein `return`: hätte ein Coalescing je einen tool_result- und
        // einen Text-Block in dieselbe Rolle gelegt, wäre der Text spurlos
        // verschwunden — Kontextverlust, den niemand bemerkt hätte. Heute
        // erzeugt kein Produzent diese Kombination (tool_result trägt immer
        // Rolle `tool`), aber ein stiller Datenverlust ist der falsche Ort für
        // Verlass auf den Aufrufer.
        let rest: Vec<&RenderContentBlock> = message
            .blocks
            .iter()
            .filter(|b| b.kind != RenderBlockKind::ToolResult)
            .collect();
        if rest.is_empty() {
            return;
        }
        let text: String = rest
            .iter()
            .filter(|b| b.kind == RenderBlockKind::Text)
            .map(|b| b.text.as_deref().unwrap_or(""))
            .collect();
        let tool_calls: Vec<Value> = rest
            .iter()
            .filter(|b| b.kind == RenderBlockKind::ToolCall)
            .map(|b| build_tool_call(b))
            .collect();
        let mut msg = json!({ "role": role_wire(message.role), "content": text });
        if !tool_calls.is_empty() {
            msg["tool_calls"] = Value::Array(tool_calls);
        }
        out.push(msg);
        return;
    }

    // Spec §4.6: tool_call ⇒ Assistant-Message mit tool_calls[].
    let tool_calls: Vec<Value> = message
        .blocks
        .iter()
        .filter(|b| b.kind == RenderBlockKind::ToolCall)
        .map(build_tool_call)
        .collect();

    let content: String = message
        .blocks
        .iter()
        .filter(|b| b.kind == RenderBlockKind::Text)
        .map(|b| b.text.as_deref().unwrap_or(""))
        .collect();

    let mut msg = json!({ "role": role_wire(message.role), "content": content });

    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }

    out.push(msg);
}

fn build_tool_call(block: &RenderContentBlock) -> Value {
    json!({
        "id": block.tool_call_id,
        "type": "function",
        "function": {
            "name": block.tool_name,
            "arguments": block.text.as_deref().unwrap_or(""),
        },
    })
}

fn role_wire(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::User => "user",
    }
}

fn build_expand_context_ref_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "expand_context_ref",
            "description": "Expand an externalized context segment by its segment_id to retrieve its full content.",
            "parameters": {
                "type": "object",
                "properties": { "segment_id": { "type": "string" } },
                "required": ["segment_id"],
            },
        },
    })
}
