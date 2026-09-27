//! agentkit als **MCP-Server** — die Gegenrichtung zu [`crate::mcp`].
//!
//! Stellt eine beliebige [`ToolRegistry`] über stdio-JSON-RPC (eine Nachricht je
//! Zeile, wie der Client in `mcp.rs`) als MCP-Tools bereit. Damit kann ein
//! anderer Agent — Claude Code, Cursor, ein zweites agentkit — agentkits
//! Werkzeuge nutzen: den ganzen Coding-Agenten als ein Tool, den Wissensgraphen
//! als gemeinsames Gedächtnis oder die Sandbox-Tools direkt. Welche Tools es
//! sind, entscheidet der Aufrufer (`agentkit mcp-serve`); dieses Modul kennt
//! nur das Protokoll.
//!
//! Synchron und sequenziell: eine Anfrage nach der anderen. Ein MCP-Client
//! wartet ohnehin auf das Ergebnis seines Tool-Aufrufs, und ohne async-Runtime
//! wäre Nebenläufigkeit hier Aufwand ohne Nutzer.
//!
//! **stdout gehört dem Protokoll.** Alles andere — Statusmeldungen, die Spur
//! eines delegierten Agenten — muss auf stderr; eine einzige fremde Zeile auf
//! stdout zerbricht die Verbindung.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::tools::{ToolEffect, ToolRegistry};

/// Protokollversion, wenn der Client keine nennt.
const DEFAULT_PROTOCOL: &str = "2025-06-18";

/// Bedient MCP-Anfragen von `input`, bis der Client die Verbindung schließt
/// (EOF). Antworten gehen zeilenweise nach `output`.
pub fn serve(
    registry: &ToolRegistry,
    input: impl BufRead,
    mut output: impl Write,
    version: &str,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(antwort) = handle_line(registry, &line, version) {
            writeln!(output, "{antwort}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Eine Zeile → die Antwort (`None` für Benachrichtigungen ohne `id`).
fn handle_line(registry: &ToolRegistry, line: &str, version: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(&Value::Null, -32700, &format!("Parse-Fehler: {e}"))),
    };
    // Ohne `id` ist es eine Benachrichtigung (`notifications/initialized`,
    // `notifications/cancelled`) — darauf wird nie geantwortet.
    let id = msg.get("id")?.clone();
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(DEFAULT_PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "agentkit", "version": version},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": list_tools(registry)}),
        "tools/call" => call_tool(registry, params),
        other => return Some(error(&id, -32601, &format!("unbekannte Methode '{other}'"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Die Registry als MCP-Tool-Liste. Die deklarierte Wirkung wird zur
/// Annotation — derselbe Weg, auf dem `mcp.rs` sie beim Einlesen übernimmt.
fn list_tools(registry: &ToolRegistry) -> Vec<Value> {
    registry
        .schemas()
        .unwrap_or_default()
        .iter()
        .map(|s| {
            let f = &s["function"];
            let name = f["name"].as_str().unwrap_or("");
            let mut tool = json!({
                "name": name,
                "description": f["description"],
                "inputSchema": f["parameters"],
            });
            match registry.effect(name) {
                Some(ToolEffect::ReadOnly) => {
                    tool["annotations"] = json!({"readOnlyHint": true});
                }
                Some(ToolEffect::Destructive) => {
                    tool["annotations"] = json!({"readOnlyHint": false, "destructiveHint": true});
                }
                None => {}
            }
            tool
        })
        .collect()
}

/// Ein Tool ausführen. Tool-Fehler sind KEINE Protokollfehler: nach MCP gehen
/// sie als `isError: true` im Ergebnis zurück, damit das aufrufende Modell sie
/// sieht und reagieren kann.
fn call_tool(registry: &ToolRegistry, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = match &params["arguments"] {
        Value::Null => json!({}),
        a => a.clone(),
    };
    let (text, is_error) = if !registry.has(name) {
        (format!("unbekanntes Tool '{name}'"), true)
    } else {
        match registry.call(name, args) {
            Ok(s) => (s, false),
            Err(e) => (e, true),
        }
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.add(
            "add",
            "Addiert.",
            json!({"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}}),
            |a: Value| Ok((a["a"].as_i64().unwrap_or(0) + a["b"].as_i64().unwrap_or(0)).to_string()),
        );
        reg.declare("add", ToolEffect::ReadOnly);
        reg.add("kaputt", "Scheitert.", json!({"type": "object"}), |_| {
            Err("geht nicht".into())
        });
        reg
    }

    fn dialog(lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut out = Vec::new();
        serve(&registry(), input.as_bytes(), &mut out, "9.9.9").unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Handshake, Liste und Aufruf — so, wie ihn ein Client wie Claude Code
    /// fährt. Die Benachrichtigung bekommt keine Antwort.
    #[test]
    fn handshake_liste_und_aufruf() {
        let out = dialog(&[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "add", "arguments": {"a": 2, "b": 3}}}),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(out[0]["result"]["serverInfo"]["version"], "9.9.9");
        let tools = out[1]["result"]["tools"].as_array().unwrap();
        let add = tools.iter().find(|t| t["name"] == "add").unwrap();
        assert_eq!(add["annotations"]["readOnlyHint"], true);
        assert_eq!(add["inputSchema"]["type"], "object");
        assert_eq!(out[2]["id"], 3);
        assert_eq!(out[2]["result"]["content"][0]["text"], "5");
        assert_eq!(out[2]["result"]["isError"], false);
    }

    /// Tool-Fehler und unbekannte Tools sind Ergebnisse mit `isError`, eine
    /// unbekannte Methode und kaputtes JSON sind Protokollfehler.
    #[test]
    fn fehler_landen_an_der_richtigen_stelle() {
        let out = dialog(&[
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "kaputt"}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gibtsnicht"}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
        ]);
        assert_eq!(out[0]["result"]["isError"], true);
        assert_eq!(out[0]["result"]["content"][0]["text"], "geht nicht");
        assert_eq!(out[1]["result"]["isError"], true);
        assert_eq!(out[2]["error"]["code"], -32601);
        let mut raw = Vec::new();
        serve(&registry(), "{kaputt\n".as_bytes(), &mut raw, "1").unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }
}
