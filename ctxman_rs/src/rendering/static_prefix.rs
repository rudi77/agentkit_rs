use serde_json::{json, Value};

use super::model::RenderStaticItem;

/// Baut das provider-agnostische Static-Prefix-Objekt für `cache_prefix_hash` (Spec §6, I4).
/// Spiegelt die Static-Region (System-Prompt + Tool-Defs) unabhängig vom Adapter.
/// (Port von `RenderStaticPrefix.cs`.)
pub fn build_hash_input(items: &[RenderStaticItem]) -> Value {
    let system = items
        .iter()
        .filter(|i| i.kind != "tool_def")
        .map(|i| i.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    let tools: Vec<Value> = items
        .iter()
        .filter(|i| i.kind == "tool_def")
        .map(|i| parse_json_or_string(&i.content))
        .collect();

    json!({ "system": system, "tools": tools })
}

/// Tool-Def-Content als JSON parsen, damit er als Objekt nistet und kanonisch re-sortiert wird;
/// nicht parsbarer (oder JSON-`null`-)Content wird als String durchgereicht — Verhalten wie
/// `JsonNode.Parse(content) ?? content` im C#-Original.
pub(crate) fn parse_json_or_string(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(Value::Null) | Err(_) => Value::String(content.to_string()),
        Ok(value) => value,
    }
}
