//! MCP-Anbindung — Tools über das Model Context Protocol statt aus lokalem Code.
//!
//! Derselbe Agent-Loop, nur kommen Schema & Ausführung von einem MCP-Server.
//! Pythons Variante braucht eine asyncio-Schleife im Hintergrund-Thread; in Rust
//! genügt eine **synchrone** stdio-Session: der stdio-Transport ist
//! zeilengetrenntes JSON-RPC, das sich direkt über `std::process` lesen/schreiben
//! lässt. Eine `Mutex`-geschützte Session macht `call_tool` thread-safe (parallele
//! Tool-Calls), ohne async-Runtime.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// MCP-Tool-Ergebnis -> Text fürs Modell.
fn mcp_text(result: &Value) -> String {
    if let Some(arr) = result.get("content").and_then(Value::as_array) {
        let parts: Vec<String> = arr
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str).map(String::from))
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    result
        .get("content")
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// MCP-Tool-Definitionen -> OpenAI-Tool-Schemas.
pub fn mcp_tools_to_schemas(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.get("name").and_then(Value::as_str).unwrap_or(""),
                    "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": t.get("inputSchema").cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                },
            })
        })
        .collect()
}

struct Session {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _child: Child,
}

struct Inner {
    session: Mutex<Session>,
    id: AtomicU64,
}

impl Inner {
    /// Ein JSON-RPC-Request mit Antwort (blockierend bis zur passenden `id`).
    fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut sess = self.session.lock().unwrap();
        writeln!(sess.stdin, "{req}").map_err(|e| e.to_string())?;
        sess.stdin.flush().map_err(|e| e.to_string())?;

        // Zeilen lesen, bis die Antwort mit unserer id kommt (Notifications überspringen).
        loop {
            let mut line = String::new();
            let n = sess
                .stdout
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("MCP-Server hat die Verbindung geschlossen".to_string());
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(err.to_string());
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // andere Nachricht (z. B. Notification) -> ignorieren
        }
    }

    /// Eine JSON-RPC-Notification (ohne Antwort).
    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let note = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut sess = self.session.lock().unwrap();
        writeln!(sess.stdin, "{note}").map_err(|e| e.to_string())?;
        sess.stdin.flush().map_err(|e| e.to_string())
    }
}

/// Persistente Verbindung zu EINEM MCP-Server (stdio-Transport).
#[derive(Clone)]
pub struct MCPClient {
    inner: Arc<Inner>,
    /// rohe MCP-Tool-Definitionen
    pub tools: Vec<Value>,
}

impl MCPClient {
    /// Startet den Server-Prozess und führt den Protokoll-Handshake aus.
    pub fn connect(
        command: &str,
        args: &[&str],
        env: Option<&[(String, String)]>,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(env) = env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("kein stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("kein stdout")?);

        let inner = Arc::new(Inner {
            session: Mutex::new(Session {
                stdin,
                stdout,
                _child: child,
            }),
            id: AtomicU64::new(1),
        });

        // Handshake: initialize -> initialized -> tools/list.
        inner.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "agentkit-rs", "version": "0.1.0"},
            }),
        )?;
        inner.notify("notifications/initialized", json!({}))?;
        let listed = inner.rpc("tools/list", json!({}))?;
        let tools = listed
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(MCPClient { inner, tools })
    }

    pub fn schemas(&self) -> Vec<Value> {
        mcp_tools_to_schemas(&self.tools)
    }

    pub fn call_tool(&self, name: &str, args: Value) -> Result<String, String> {
        let result = self
            .inner
            .rpc("tools/call", json!({"name": name, "arguments": args}))?;
        Ok(mcp_text(&result))
    }

    /// Klinkt die Server-Tools in eine ToolRegistry ein (optional namespaced).
    pub fn register(&self, registry: &mut ToolRegistry, prefix: &str) {
        for t in &self.tools {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let parameters = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let inner = self.inner.clone();
            let server_name = name.clone();
            registry.add(
                &format!("{prefix}{name}"),
                &description,
                parameters,
                move |args: Value| {
                    let result = inner.rpc(
                        "tools/call",
                        json!({"name": server_name, "arguments": args}),
                    )?;
                    Ok(mcp_text(&result))
                },
            );
        }
    }
}
