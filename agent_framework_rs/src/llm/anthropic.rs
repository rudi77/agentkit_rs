//! Anthropic Messages API (`POST /v1/messages`) — nativ, ohne OpenAI-Shim.
//!
//! Der Agent-Loop spricht intern das OpenAI-Nachrichtenformat (`role`/`content`/
//! `tool_calls`/`tool`-Nachrichten) — das ist die Form, in der `memory`, ctxman
//! und die Sitzungsdateien den Verlauf halten. Dieser Adapter übersetzt an der
//! Leitung in beide Richtungen: Verlauf → Content-Blöcke (`text`, `tool_use`,
//! `tool_result`), SSE-Ereignisse → [`Chunk`]s. Der Loop bleibt unberührt.
//!
//! Drei Dinge, die ein OpenAI-kompatibler Umweg nicht leistet und die der Grund
//! für diesen Adapter sind:
//!
//! - **Prompt-Caching.** Drei `cache_control`-Marken — letztes Tool, System-Prompt,
//!   letzter Block der letzten Nachricht. Ein Agent-Loop schickt bei jedem Schritt
//!   den ganzen bisherigen Verlauf; mit den Marken wird der schon gesehene Teil
//!   zum Cache-Preis gelesen statt neu bezahlt. Sichtbar in `token_usage`
//!   (`cached_input_tokens`).
//! - **Denk-Blöcke bleiben erhalten.** Aktuelle Modelle denken adaptiv, und die
//!   `thinking`-Blöcke einer Antwort müssen im nächsten Request UNVERÄNDERT
//!   zurück — sonst verliert das Modell seine Überlegung mitten in einer
//!   Tool-Runde. Der Verlauf im OpenAI-Format hat dafür keinen Platz; deshalb
//!   merkt sich der Adapter die Original-Blöcke jeder Antwort (siehe
//!   [`AnthropicLlm::replay`]) und setzt sie beim nächsten Request wieder ein.
//! - **Gemessener Verbrauch** inklusive Cache-Anteil (`message_start` +
//!   `message_delta`).
//!
//! Wird der Verlauf verändert (Kompaktierung, `/rewind`, ctxman-GC), passen
//! gemerkte Denk-Blöcke nicht mehr zu ihrem Präfix. Statt dann einen 400 zu
//! riskieren, fordert jeder Request `prefix_mismatch_behavior: "drop_block"` an:
//! die API verwirft solche Blöcke und der Lauf geht weiter.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::openai::{describe_error, http_agent};
use super::{Chunk, ChunkStream, Delta, Llm, Message, ToolCallDelta, Usage};

/// Modell, wenn `ANTHROPIC_MODEL` nicht gesetzt ist.
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-opus-5";

const API_VERSION: &str = "2023-06-01";
/// Beta, die `thinking.block_binding` erlaubt (siehe Moduldoku).
const BETA_THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";
/// Obergrenze der Ausgabe je Call. Großzügig: beim Streaming gibt es kein
/// HTTP-Timeout-Problem, und ein zu knappes Limit schneidet eine Antwort mitten
/// in einem Tool-Aufruf ab.
const MAX_TOKENS_STREAM: u64 = 64_000;
/// Ohne Streaming (Kompaktierung) kleiner, damit der Call in der Lesegrenze des
/// HTTP-Agenten bleibt.
const MAX_TOKENS_COMPLETE: u64 = 16_000;

/// Anthropic-LLM über die Messages API.
pub struct AnthropicLlm {
    url: String,
    api_key: String,
    model: String,
    /// `output_config.effort` (`low` … `max`); `None` = Default des Modells.
    effort: Option<String>,
    /// `output_config.format` (Structured Outputs, `--schema`); `None` = freie Antwort.
    output_format: Option<Value>,
    /// Adaptives Denken anfordern? Aus für Modelle, die es nicht kennen (Haiku).
    thinking: bool,
    /// Die Original-Content-Blöcke der Antworten, die Denk-Blöcke enthielten —
    /// Schlüssel ist der [`fingerprint`] der Assistant-Nachricht, wie sie im
    /// Verlauf steht. `Arc`, weil Sub-Agenten dasselbe LLM teilen und ihre
    /// Streams auf eigenen Threads laufen.
    ///
    /// Wächst mit der Sitzung (eine Antwort je Eintrag); das ist dieselbe
    /// Größenordnung wie der Verlauf selbst und endet mit dem Prozess.
    replay: Arc<Mutex<HashMap<String, Value>>>,
    agent: ureq::Agent,
}

impl AnthropicLlm {
    /// Standard-Endpunkt `https://api.anthropic.com`.
    pub fn new(api_key: &str, model: &str) -> Self {
        AnthropicLlm {
            url: "https://api.anthropic.com/v1/messages".to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            effort: None,
            output_format: None,
            // Haiku kennt kein adaptives Denken — die Anfrage würde abgelehnt.
            thinking: !model.contains("haiku"),
            replay: Arc::new(Mutex::new(HashMap::new())),
            agent: http_agent(),
        }
    }

    /// Anderer Endpunkt (Proxy, Gateway): API-Wurzel ohne `/v1/messages`.
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        self
    }

    /// Setzt `output_config.effort` (`low`, `medium`, `high`, `xhigh`, `max`).
    pub fn with_effort(mut self, effort: &str) -> Self {
        self.effort = Some(effort.to_string());
        self
    }

    /// Erzwingt die Antwortstruktur über `output_config.format`
    /// (`{"type": "json_schema", "schema": …}`). Das Schema muss die
    /// Einschränkungen der API erfüllen (siehe [`crate::schema::native_compatible`]).
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_format = Some(json!({"type": "json_schema", "schema": schema}));
        self
    }

    fn body(&self, messages: &[Value], tools: Option<&[Value]>, stream: bool) -> Value {
        let replay = self.replay.lock().unwrap();
        let (system, msgs) = convert_messages(messages, &replay);
        drop(replay);
        let mut body = json!({
            "model": self.model,
            "max_tokens": if stream { MAX_TOKENS_STREAM } else { MAX_TOKENS_COMPLETE },
            "messages": msgs,
        });
        if !system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if let Some(t) = tools.filter(|t| !t.is_empty()) {
            body["tools"] = Value::Array(convert_tools(t));
        }
        if self.thinking {
            body["thinking"] = json!({
                "type": "adaptive",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"},
            });
        }
        if let Some(effort) = &self.effort {
            body["output_config"]["effort"] = json!(effort);
        }
        if let Some(format) = &self.output_format {
            body["output_config"]["format"] = format.clone();
        }
        if stream {
            body["stream"] = json!(true);
        }
        body
    }

    fn request(&self) -> ureq::Request {
        let req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", API_VERSION);
        if self.thinking {
            req.set("anthropic-beta", BETA_THINKING_BINDING)
        } else {
            req
        }
    }
}

impl Llm for AnthropicLlm {
    fn complete(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<Message, String> {
        let body = self.body(messages, tools, false);
        let resp = self.request().send_json(body).map_err(describe_error)?;
        let v: Value = resp.into_json().map_err(|e| e.to_string())?;
        if v["stop_reason"] == "refusal" {
            return Err(refusal_error(&v["stop_details"]));
        }
        let blocks = v["content"].as_array().cloned().unwrap_or_default();
        let (text, tool_calls) = openai_view(&blocks);
        remember_blocks(&self.replay, &blocks);
        Ok(Message {
            content: Some(text),
            tool_calls,
        })
    }

    fn stream(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        let body = self.body(messages, tools, true);
        let resp = self.request().send_json(body).map_err(describe_error)?;
        let mut lines = BufReader::new(resp.into_reader()).lines();
        let mut state = StreamState {
            replay: self.replay.clone(),
            ..StreamState::default()
        };
        let iter = std::iter::from_fn(move || loop {
            if state.finished {
                return None;
            }
            match lines.next() {
                Some(Ok(line)) => {
                    // SSE: `event: …` und `data: {json}`; der Typ steht auch
                    // im JSON, die `event:`-Zeile ist daher entbehrlich.
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(ev) = serde_json::from_str::<Value>(payload.trim()) else {
                        continue;
                    };
                    match state.handle(&ev) {
                        Ok(Some(chunk)) => return Some(Ok(chunk)),
                        Ok(None) => continue,
                        Err(e) => {
                            state.finished = true;
                            return Some(Err(e));
                        }
                    }
                }
                Some(Err(e)) => {
                    state.finished = true;
                    return Some(Err(format!("Stream-Lesefehler: {e}")));
                }
                None => {
                    state.finished = true;
                    return Some(Err(
                        "Stream endete ohne 'message_stop' — abgeschnitten".to_string()
                    ));
                }
            }
        });
        Ok(Box::new(iter))
    }
}

/// Merkt sich die Original-Blöcke einer Antwort — aber nur, wenn Denk-Blöcke
/// darin stecken. Ohne sie ist die Rückübersetzung aus dem Verlauf verlustfrei,
/// und der Eintrag wäre nur Speicher.
fn remember_blocks(replay: &Mutex<HashMap<String, Value>>, blocks: &[Value]) {
    let denkt = blocks
        .iter()
        .any(|b| matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")));
    if !denkt {
        return;
    }
    let (text, tool_calls) = openai_view(blocks);
    let key = fingerprint(&text, tool_calls.iter().filter_map(|tc| tc["id"].as_str()));
    replay
        .lock()
        .unwrap()
        .insert(key, Value::Array(blocks.to_vec()));
}

/// Schlüssel, unter dem eine Antwort im Verlauf wiedergefunden wird: die IDs
/// ihrer Tool-Aufrufe (eindeutig je Aufruf), sonst ihr Text.
fn fingerprint<'a>(text: &str, tool_ids: impl Iterator<Item = &'a str>) -> String {
    let ids: Vec<&str> = tool_ids.collect();
    if ids.is_empty() {
        format!("text:{text}")
    } else {
        format!("tools:{}", ids.join(","))
    }
}

/// Die Antwort-Blöcke so, wie der Loop sie sieht: der zusammengesetzte Text und
/// die Tool-Aufrufe im OpenAI-Format (`arguments` als JSON-String).
fn openai_view(blocks: &[Value]) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": b["id"],
                "type": "function",
                "function": {"name": b["name"], "arguments": b["input"].to_string()},
            })),
            _ => {}
        }
    }
    (text, calls)
}

fn refusal_error(details: &Value) -> String {
    let kategorie = details["category"].as_str().unwrap_or("unbekannt");
    format!(
        "Das Modell hat die Anfrage abgelehnt (refusal, Kategorie: {kategorie}) — \
         anderes Modell wählen (ANTHROPIC_MODEL/--model) oder den Auftrag umformulieren"
    )
}

/// Tool-Schemas: OpenAI-`function`-Hülle → Anthropic `{name, description,
/// input_schema}`. Das letzte Tool trägt die Cache-Marke: Tools stehen im
/// Prompt ganz vorn und ändern sich innerhalb einer Sitzung kaum.
fn convert_tools(tools: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = tools
        .iter()
        .map(|t| {
            let f = &t["function"];
            json!({
                "name": f["name"],
                "description": f["description"],
                "input_schema": if f["parameters"].is_object() {
                    f["parameters"].clone()
                } else {
                    json!({"type": "object"})
                },
            })
        })
        .collect();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    out
}

/// Der Text einer OpenAI-Nachricht — `content` als String oder als Liste von
/// Text-Teilen.
fn text_of(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Verlauf (OpenAI-Format) → `(system, messages)` der Messages API.
///
/// - Führende `system`-Nachrichten werden zum System-Prompt. Eine spätere (etwa
///   eine Zusammenfassung mitten im Verlauf) wird als markierter User-Text
///   eingereiht — mitten im Verlauf kennen nicht alle Modelle die Rolle.
/// - `tool`-Nachrichten werden zu `tool_result`-Blöcken einer User-Nachricht;
///   aufeinanderfolgende Nachrichten derselben Rolle werden zusammengelegt,
///   denn die API verlangt den Wechsel user/assistant.
/// - Eine Assistant-Nachricht, deren Original-Blöcke in `replay` stehen, geht
///   unverändert (mit Denk-Blöcken) zurück.
/// - Die letzte Nachricht bekommt die Cache-Marke auf ihren letzten Block.
fn convert_messages(messages: &[Value], replay: &HashMap<String, Value>) -> (String, Vec<Value>) {
    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m["role"].as_str().unwrap_or("") {
            "system" if out.is_empty() => {
                let t = text_of(m);
                if !t.is_empty() {
                    system.push(t);
                }
            }
            "system" => push_block(
                &mut out,
                "user",
                json!({"type": "text", "text": format!("[System-Hinweis]\n{}", text_of(m))}),
            ),
            "user" => {
                let t = text_of(m);
                if !t.is_empty() {
                    push_block(&mut out, "user", json!({"type": "text", "text": t}));
                }
            }
            "tool" => push_block(
                &mut out,
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": m["tool_call_id"],
                    "content": text_of(m),
                }),
            ),
            "assistant" => {
                for block in assistant_blocks(m, replay) {
                    push_block(&mut out, "assistant", block);
                }
            }
            _ => {}
        }
    }
    // Die API verlangt eine User-Nachricht am Anfang — nach einer Kompaktierung
    // mit behaltenem Ende kann der Verlauf mit einer Antwort beginnen.
    if out.first().is_some_and(|m| m["role"] == "assistant") {
        out.insert(
            0,
            json!({"role": "user", "content": [{"type": "text", "text": "(Fortsetzung)"}]}),
        );
    }
    if let Some(last) = out
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|c| c.last_mut())
    {
        // Denk-Blöcke tragen keine Cache-Marke; sie stehen nie am Ende einer
        // Nachricht, die der Agent als letzte schickt (das ist User/Tool).
        if !matches!(
            last["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        ) {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    (system.join("\n\n"), out)
}

/// Die Content-Blöcke einer Assistant-Nachricht: die gemerkten Originale, sonst
/// aus Text und Tool-Aufrufen neu gebaut.
fn assistant_blocks(m: &Value, replay: &HashMap<String, Value>) -> Vec<Value> {
    let text = text_of(m);
    let calls = m["tool_calls"].as_array().cloned().unwrap_or_default();
    let key = fingerprint(&text, calls.iter().filter_map(|tc| tc["id"].as_str()));
    if let Some(Value::Array(original)) = replay.get(&key) {
        return original.clone();
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    for tc in &calls {
        let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
        // Das Modell hat ein Objekt geschickt; was nicht parst, wird zum
        // leeren Objekt — genau so hat es der Loop auch ausgeführt.
        let input: Value = serde_json::from_str(args)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": tc["id"],
            "name": tc["function"]["name"],
            "input": input,
        }));
    }
    blocks
}

/// Hängt einen Block an die letzte Nachricht, wenn sie dieselbe Rolle hat, sonst
/// als neue Nachricht.
fn push_block(out: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = out.last_mut().filter(|m| m["role"] == role) {
        if let Some(content) = last["content"].as_array_mut() {
            content.push(block);
            return;
        }
    }
    out.push(json!({"role": role, "content": [block]}));
}

/// Zustand eines laufenden SSE-Streams.
#[derive(Default)]
struct StreamState {
    /// Die Blöcke der Antwort, wie sie ankommen (Index = `index` des Ereignisses).
    blocks: Vec<Value>,
    /// Die Teil-JSONs der `tool_use`-Blöcke, bis `content_block_stop` sie parst.
    partial_json: HashMap<usize, String>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_details: Value,
    finished: bool,
    replay: Arc<Mutex<HashMap<String, Value>>>,
}

impl StreamState {
    /// Ein SSE-Ereignis verarbeiten. `Ok(Some)` = ein Chunk für den Loop.
    fn handle(&mut self, ev: &Value) -> Result<Option<Chunk>, String> {
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &ev["message"]["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                // `input_tokens` ist nur der ungecachte Rest; die Summe aller
                // drei ist der ganze Prompt.
                self.usage.input_tokens = n("input_tokens")
                    + n("cache_creation_input_tokens")
                    + n("cache_read_input_tokens");
                self.usage.cached_input_tokens = n("cache_read_input_tokens");
                self.usage.output_tokens = n("output_tokens");
                Ok(None)
            }
            "content_block_start" => {
                let index = ev["index"].as_u64().unwrap_or(0) as usize;
                let block = ev["content_block"].clone();
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                }
                let chunk = (block["type"] == "tool_use").then(|| Chunk {
                    delta: Delta {
                        content: None,
                        tool_calls: vec![ToolCallDelta {
                            index,
                            id: block["id"].as_str().map(String::from),
                            name: block["name"].as_str().map(String::from),
                            arguments: None,
                        }],
                    },
                    usage: None,
                    truncated: false,
                });
                self.blocks[index] = block;
                Ok(chunk)
            }
            "content_block_delta" => {
                let index = ev["index"].as_u64().unwrap_or(0) as usize;
                let d = &ev["delta"];
                let Some(block) = self.blocks.get_mut(index) else {
                    return Ok(None);
                };
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        append(block, "text", t);
                        Ok(Some(Chunk::text(t)))
                    }
                    "thinking_delta" => {
                        append(block, "thinking", d["thinking"].as_str().unwrap_or(""));
                        Ok(None)
                    }
                    "signature_delta" => {
                        append(block, "signature", d["signature"].as_str().unwrap_or(""));
                        Ok(None)
                    }
                    "input_json_delta" => {
                        let part = d["partial_json"].as_str().unwrap_or("");
                        self.partial_json.entry(index).or_default().push_str(part);
                        Ok(Some(Chunk {
                            delta: Delta {
                                content: None,
                                tool_calls: vec![ToolCallDelta {
                                    index,
                                    id: None,
                                    name: None,
                                    arguments: Some(part.to_string()),
                                }],
                            },
                            usage: None,
                            truncated: false,
                        }))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_stop" => {
                let index = ev["index"].as_u64().unwrap_or(0) as usize;
                if let (Some(block), Some(json)) =
                    (self.blocks.get_mut(index), self.partial_json.remove(&index))
                {
                    block["input"] = serde_json::from_str(&json).unwrap_or_else(|_| json!({}));
                }
                Ok(None)
            }
            "message_delta" => {
                if let Some(out) = ev["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = out;
                }
                if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                    self.stop_details = ev["delta"]["stop_details"].clone();
                }
                Ok(None)
            }
            "message_stop" => {
                self.finished = true;
                if self.stop_reason.as_deref() == Some("refusal") {
                    return Err(refusal_error(&self.stop_details));
                }
                let blocks: Vec<Value> = std::mem::take(&mut self.blocks)
                    .into_iter()
                    .filter(|b| !b.is_null())
                    .collect();
                remember_blocks(&self.replay, &blocks);
                Ok(Some(Chunk {
                    delta: Delta::default(),
                    usage: Some(self.usage),
                    truncated: self.stop_reason.as_deref() == Some("max_tokens"),
                }))
            }
            "error" => {
                let e = &ev["error"];
                Err(format!(
                    "Anthropic-Fehler ({}): {}",
                    e["type"].as_str().unwrap_or("?"),
                    e["message"].as_str().unwrap_or("")
                ))
            }
            // `ping` und künftige Ereignistypen: nichts zu tun.
            _ => Ok(None),
        }
    }
}

/// Hängt `text` an das String-Feld `key` eines Blocks an.
fn append(block: &mut Value, key: &str, text: &str) {
    let alt = block[key].as_str().unwrap_or("").to_string();
    block[key] = Value::String(alt + text);
}

/// Baut einen Anthropic-LLM aus der Umgebung: `ANTHROPIC_API_KEY` (Pflicht),
/// `ANTHROPIC_MODEL` (Default [`ANTHROPIC_DEFAULT_MODEL`]), optional
/// `ANTHROPIC_BASE_URL` und `ANTHROPIC_EFFORT`.
pub fn anthropic_from_env() -> Result<AnthropicLlm, String> {
    let get = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let key = get("ANTHROPIC_API_KEY").ok_or_else(|| "ANTHROPIC_API_KEY fehlt".to_string())?;
    let model = get("ANTHROPIC_MODEL").unwrap_or_else(|| ANTHROPIC_DEFAULT_MODEL.to_string());
    let mut llm = AnthropicLlm::new(&key, &model);
    if let Some(base) = get("ANTHROPIC_BASE_URL") {
        llm = llm.with_base_url(&base);
    }
    if let Some(effort) = get("ANTHROPIC_EFFORT") {
        llm = llm.with_effort(&effort);
    }
    Ok(llm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(state: &mut StreamState, events: &[Value]) -> Vec<Result<Option<Chunk>, String>> {
        events.iter().map(|e| state.handle(e)).collect()
    }

    /// Ein Tool-Zug im OpenAI-Format wird zu Anthropic-Blöcken: System nach
    /// oben, `tool_use` mit geparstem `input`, Tool-Ergebnisse als
    /// `tool_result` in EINER User-Nachricht, Cache-Marke am Ende.
    #[test]
    fn verlauf_wird_uebersetzt() {
        let msgs = vec![
            json!({"role": "system", "content": "Du bist hilfreich."}),
            json!({"role": "user", "content": "2+3 und 4+5?"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "add", "arguments": "{\"a\":2,\"b\":3}"}},
                {"id": "b", "type": "function", "function": {"name": "add", "arguments": "kaputt"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "5"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "9"}),
        ];
        let (system, out) = convert_messages(&msgs, &HashMap::new());
        assert_eq!(system, "Du bist hilfreich.");
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0]["input"], json!({"a": 2, "b": 3}));
        assert_eq!(out[1]["content"][1]["input"], json!({}));
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[2]["content"][1]["tool_use_id"], "b");
        assert_eq!(out[2]["content"][1]["cache_control"]["type"], "ephemeral");
        assert!(out[2]["content"][0].get("cache_control").is_none());
    }

    /// Nach einer Kompaktierung kann der Verlauf mit einer Antwort beginnen;
    /// eine spätere System-Nachricht wird zu markiertem User-Text.
    #[test]
    fn verlauf_beginnt_immer_mit_user() {
        let msgs = vec![
            json!({"role": "system", "content": "S"}),
            json!({"role": "assistant", "content": "Zwischenstand"}),
            json!({"role": "system", "content": "Zusammenfassung"}),
        ];
        let (_, out) = convert_messages(&msgs, &HashMap::new());
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert!(out[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Zusammenfassung"));
    }

    /// Der Stream liefert Text- und Tool-Deltas, am Ende den Verbrauch (Cache
    /// eingerechnet) — und die Antwort mit Denk-Block wird gemerkt, sodass der
    /// nächste Request sie unverändert zurückschickt.
    #[test]
    fn stream_mit_denkblock_wird_gemerkt_und_zurueckgespielt() {
        let mut state = StreamState::default();
        let out = feed(
            &mut state,
            &[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 20, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Ich rechne."}}),
                json!({"type": "content_block_stop", "index": 1}),
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "add", "input": {}}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}),
                json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "2}"}}),
                json!({"type": "content_block_stop", "index": 2}),
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
                json!({"type": "message_stop"}),
            ],
        );
        let chunks: Vec<Chunk> = out.into_iter().filter_map(|r| r.unwrap()).collect();
        assert_eq!(chunks[0].delta.content.as_deref(), Some("Ich rechne."));
        assert_eq!(chunks[1].delta.tool_calls[0].name.as_deref(), Some("add"));
        assert_eq!(
            chunks[2].delta.tool_calls[0].arguments.as_deref(),
            Some("{\"a\":")
        );
        assert_eq!(
            chunks.last().unwrap().usage,
            Some(Usage {
                input_tokens: 125,
                output_tokens: 42,
                cached_input_tokens: 100
            })
        );

        // So steht die Antwort danach im Verlauf …
        let verlauf = vec![
            json!({"role": "user", "content": "rechne"}),
            json!({"role": "assistant", "content": "Ich rechne.", "tool_calls": [
                {"id": "t1", "type": "function", "function": {"name": "add", "arguments": "{\"a\":2}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "2"}),
        ];
        // … und so geht sie wieder hinaus: mit dem unveränderten Denk-Block.
        let replay = state.replay.lock().unwrap();
        let (_, out) = convert_messages(&verlauf, &replay);
        assert_eq!(out[1]["content"][0]["type"], "thinking");
        assert_eq!(out[1]["content"][0]["signature"], "SIG");
        assert_eq!(out[1]["content"][2]["input"], json!({"a": 2}));
    }

    /// Ablehnung und Fehlerereignis sind Fehler, kein reguläres Ende; ein
    /// Stream ohne `message_stop` auch (siehe `stream`).
    #[test]
    fn refusal_und_fehler_brechen_ab() {
        let mut state = StreamState::default();
        let out = feed(
            &mut state,
            &[
                json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"category": "cyber"}}, "usage": {"output_tokens": 0}}),
                json!({"type": "message_stop"}),
            ],
        );
        assert!(out[1].as_ref().unwrap_err().contains("cyber"));
        let mut state = StreamState::default();
        let e = state
            .handle(&json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))
            .unwrap_err();
        assert!(e.contains("overloaded_error"));
    }

    /// `stop_reason: "max_tokens"` meldet der Schluss-Chunk als abgeschnitten;
    /// ein reguläres Ende nicht.
    #[test]
    fn max_tokens_meldet_abgeschnitten() {
        for (grund, erwartet) in [("max_tokens", true), ("end_turn", false)] {
            let mut state = StreamState::default();
            let out = feed(
                &mut state,
                &[
                    json!({"type": "message_delta", "delta": {"stop_reason": grund}, "usage": {"output_tokens": 5}}),
                    json!({"type": "message_stop"}),
                ],
            );
            let schluss = out[1].as_ref().unwrap().as_ref().unwrap();
            assert_eq!(schluss.truncated, erwartet, "{grund}");
        }
    }

    /// Ende-zu-Ende gegen einen lokalen Pseudo-Server: Header und Body gehen
    /// richtig hinaus, die SSE-Zeilen kommen als Chunks zurück.
    #[test]
    fn stream_gegen_lokalen_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Kopf lesen, dann den Body laut Content-Length.
            let (kopf, rest) = loop {
                let n = sock.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some(pos) = text.find("\r\n\r\n") {
                    break (text[..pos].to_string(), req[pos + 4..].to_vec());
                }
            };
            let len: usize = kopf
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = rest;
            while body.len() < len {
                let n = sock.read(&mut buf).unwrap();
                body.extend_from_slice(&buf[..n]);
            }
            let sse = [
                r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hallo"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
                r#"{"type":"message_stop"}"#,
            ]
            .iter()
            .map(|d| format!("event: x\ndata: {d}\n\n"))
            .collect::<String>();
            write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{sse}",
                sse.len()
            )
            .unwrap();
            (kopf, String::from_utf8(body).unwrap())
        });
        let llm = AnthropicLlm::new("geheim", "claude-opus-5")
            .with_base_url(&format!("http://127.0.0.1:{port}"));
        let chunks: Vec<Chunk> = llm
            .stream(&[json!({"role": "user", "content": "hi"})], None)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let (kopf, body) = server.join().unwrap();
        let kopf = kopf.to_lowercase();
        assert!(kopf.starts_with("post /v1/messages"));
        assert!(kopf.contains("x-api-key: geheim"));
        assert!(kopf.contains("anthropic-version: 2023-06-01"));
        assert!(kopf.contains(BETA_THINKING_BINDING));
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(chunks[0].delta.content.as_deref(), Some("Hallo"));
        assert_eq!(chunks[1].usage.unwrap().output_tokens, 3);
    }

    /// Denken nur, wo das Modell es kann; Beta-Kopf und `block_binding` gehen
    /// nur zusammen hinaus (ohne Kopf lehnt die API das Feld ab).
    #[test]
    fn request_body_passt_zum_modell() {
        let tools = vec![
            json!({"type": "function", "function": {"name": "x", "description": "d", "parameters": {"type": "object"}}}),
        ];
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let opus = AnthropicLlm::new("k", "claude-opus-5").with_effort("high");
        let b = opus.body(&msgs, Some(&tools), true);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(b["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["max_tokens"], MAX_TOKENS_STREAM);
        let haiku = AnthropicLlm::new("k", "claude-haiku-4-5");
        assert!(haiku.body(&msgs, None, false).get("thinking").is_none());
        assert_eq!(
            AnthropicLlm::new("k", "m")
                .with_base_url("http://proxy:8080/")
                .url,
            "http://proxy:8080/v1/messages"
        );
    }

    /// `--schema`: das Format landet NEBEN einem gesetzten `effort` im selben
    /// `output_config` — keins darf das andere überschreiben.
    #[test]
    fn output_schema_und_effort_teilen_sich_output_config() {
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let schema = json!({"type": "object", "properties": {}, "additionalProperties": false});
        let llm = AnthropicLlm::new("k", "claude-opus-5")
            .with_effort("low")
            .with_output_schema(schema.clone());
        let b = llm.body(&msgs, None, true);
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(b["output_config"]["format"]["schema"], schema);
        // Ohne beides kein leeres `output_config`.
        let b = AnthropicLlm::new("k", "claude-opus-5").body(&msgs, None, true);
        assert!(b.get("output_config").is_none());
    }
}
