//! MCP-Anbindung — Tools über das Model Context Protocol statt aus lokalem Code.
//!
//! Derselbe Agent-Loop, nur kommen Schema & Ausführung von einem MCP-Server.
//! Pythons Variante braucht eine asyncio-Schleife im Hintergrund-Thread; in Rust
//! genügt eine **synchrone** stdio-Session: der stdio-Transport ist
//! zeilengetrenntes JSON-RPC, das sich direkt über `std::process` lesen/schreiben
//! lässt. Eine `Mutex`-geschützte Session macht `call_tool` thread-safe (parallele
//! Tool-Calls), ohne async-Runtime.

use crate::agent::Agent;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    /// Komfort: verbindet anhand einer [`McpServerSpec`] (Config-Eintrag).
    pub fn connect_spec(spec: &McpServerSpec) -> Result<Self, String> {
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        let env_ref = if spec.env.is_empty() {
            None
        } else {
            Some(spec.env.as_slice())
        };
        MCPClient::connect(&spec.command, &args, env_ref)
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

// ----------------------------------------------------------- Konfiguration & Hub
//
// Damit Frontends (Unix-Pipe, REPL, TUI) MCP-Server *deklarativ* einbinden und je
// Agent **ein-/ausschalten** können, kommt hier eine schlanke Schicht über dem
// `MCPClient` dazu:
//
// - `.mcp.json` (Claude-Code-Format `{"mcpServers": {…}}`) deklariert die Server.
// - `McpHub` hält die (einmal aufgebauten) Sessions und je Server ein **atomares**
//   `enabled`-Flag. Clients sind nach `connect` unveränderlich; nur das Flag wird
//   umgeschaltet — daher lässt sich der Hub `Arc`-teilen: das `task`-Tool liest beim
//   Spawnen eines Sub-Agenten die gerade AKTIVEN Server (live), das Frontend toggelt.
// - `register_enabled` klinkt die Tools der aktiven Server (namespaced
//   `mcp__<server>__<tool>`) in eine `ToolRegistry` ein — für Haupt- UND Sub-Agenten.

/// Deklaration EINES MCP-Servers (ein Eintrag in `.mcp.json`).
#[derive(Clone, Debug)]
pub struct McpServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// `"disabled": true` in der Config -> standardmäßig aus (ohne expliziten Wunsch).
    pub disabled: bool,
}

/// Namens-Präfix für die Tools eines Servers: `mcp__<server>__<tool>` (wie Claude
/// Code) — verhindert Kollisionen mit lokalen Tools und zwischen Servern.
pub fn mcp_prefix(server: &str) -> String {
    format!("mcp__{server}__")
}

/// Liest eine `.mcp.json`: `{"mcpServers": {name: {command, args?, env?, disabled?}}}`.
/// Liefert die Server alphabetisch sortiert (stabile Reihenfolge in Listen/Panel).
pub fn load_mcp_config(path: &str) -> Result<Vec<McpServerSpec>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let root: Value =
        serde_json::from_str(&text).map_err(|e| format!("{path}: ungültiges JSON: {e}"))?;
    let map = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{path}: erwarte ein Objekt 'mcpServers'"))?;

    let mut out = Vec::new();
    for (name, v) in map {
        let command = v
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{path}: Server '{name}' ohne 'command'"))?
            .to_string();
        let args = v
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let env = v
            .get("env")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let disabled = v.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        out.push(McpServerSpec {
            name: name.clone(),
            command,
            args,
            env,
            disabled,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Sucht eine MCP-Config: zuerst `<workspace>/.mcp.json`, dann `./.mcp.json`, dann die
/// `mcp.json`-Varianten. Gibt den ersten existierenden Pfad zurück.
pub fn discover_mcp_config(workspace: &str) -> Option<String> {
    let candidates = [
        format!("{workspace}/.mcp.json"),
        ".mcp.json".to_string(),
        format!("{workspace}/mcp.json"),
        "mcp.json".to_string(),
    ];
    candidates
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
}

/// EIN Server im [`McpHub`]: Deklaration + (ggf.) verbundene Session + Enable-Flag.
pub struct McpServer {
    pub spec: McpServerSpec,
    /// `Some`, wenn der Handshake glückte; sonst steht der Grund in `error`.
    pub client: Option<MCPClient>,
    /// Atomar, damit der Hub `&self`-umschaltbar und `Arc`-teilbar bleibt.
    pub enabled: AtomicBool,
    pub error: Option<String>,
}

impl McpServer {
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Anzahl angebotener Tools (0, falls nicht verbunden).
    pub fn tool_count(&self) -> usize {
        self.client.as_ref().map_or(0, |c| c.tools.len())
    }
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    fn set(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

/// Sammlung der MCP-Server eines Laufs. `Arc`-geteilt zwischen Frontend (schaltet um)
/// und `task`-Tool (liest beim Sub-Agent-Spawn die aktiven Server). Nach `connect` sind
/// die Clients fix; nur die `enabled`-Flags ändern sich.
#[derive(Default)]
pub struct McpHub {
    pub servers: Vec<McpServer>,
}

impl McpHub {
    /// Ein leerer Hub (kein MCP) — der Default für Aufrufer ohne MCP-Wunsch.
    pub fn empty() -> Self {
        McpHub::default()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Verbindet die deklarierten Server. `want_enabled(name)` legt den Startzustand
    /// fest. Mit `connect_all` werden auch (zunächst) deaktivierte Server schon
    /// verbunden — so sind sie später per Toggle **ohne Reconnect** zuschaltbar
    /// (interaktiv: REPL/TUI). Verbindungsfehler werden je Server gemerkt, nicht
    /// propagiert (ein kaputter Server legt nicht den ganzen Agenten lahm).
    pub fn connect(
        specs: Vec<McpServerSpec>,
        want_enabled: impl Fn(&str) -> bool,
        connect_all: bool,
    ) -> Self {
        let mut servers = Vec::new();
        for spec in specs {
            let want = want_enabled(&spec.name);
            let (client, error) = if want || connect_all {
                match MCPClient::connect_spec(&spec) {
                    Ok(c) => (Some(c), None),
                    Err(e) => (None, Some(e)),
                }
            } else {
                (None, None)
            };
            // Nicht verbundene Server können nicht aktiv sein.
            let enabled = want && client.is_some();
            servers.push(McpServer {
                spec,
                client,
                enabled: AtomicBool::new(enabled),
                error,
            });
        }
        McpHub { servers }
    }

    /// Komfort fürs Frontend: lädt `.mcp.json` (explizit `config_path` oder per Discovery
    /// im `workspace`/CWD), bestimmt den Startzustand (Allowlist `enable` ODER alle
    /// nicht-`disabled`) und verbindet. Fehlt eine Config, ist das Ergebnis ein leerer Hub
    /// (kein Fehler); ein Parse-/IO-Fehler wird als `Err` gemeldet. Logging macht der Aufrufer.
    pub fn from_config(
        workspace: &str,
        config_path: Option<&str>,
        enable: &[String],
        connect_all: bool,
    ) -> Result<McpHub, String> {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match discover_mcp_config(workspace) {
                Some(p) => p,
                None => return Ok(McpHub::empty()),
            },
        };
        let specs = load_mcp_config(&path)?;
        if specs.is_empty() {
            return Ok(McpHub::empty());
        }
        let enable_set: Vec<String> = if enable.is_empty() {
            specs
                .iter()
                .filter(|s| !s.disabled)
                .map(|s| s.name.clone())
                .collect()
        } else {
            enable.to_vec()
        };
        Ok(McpHub::connect(
            specs,
            move |name| enable_set.iter().any(|n| n == name),
            connect_all,
        ))
    }

    /// Klinkt die Tools aller AKTIVEN (verbundenen + enabled) Server in `reg` ein —
    /// namespaced `mcp__<server>__<tool>`.
    pub fn register_enabled(&self, reg: &mut ToolRegistry) {
        for s in &self.servers {
            if s.is_enabled() {
                if let Some(c) = &s.client {
                    c.register(reg, &mcp_prefix(&s.spec.name));
                }
            }
        }
    }

    /// Klinkt die aktiven MCP-Tools in einen frisch gebauten Agenten ein und gibt seine
    /// MCP-freie **Basis-Registry** zurück (Snapshot VOR dem Einklinken). Frontends heben
    /// die Basis auf und verdrahten damit beim Live-Umschalten neu (siehe [`rewire`]).
    pub fn apply(&self, agent: &mut Agent) -> ToolRegistry {
        let base = agent.tools.clone();
        self.register_enabled(&mut agent.tools);
        base
    }

    /// Verdrahtet `agent.tools` aus seiner MCP-freien `base` neu mit den GERADE aktiven
    /// Server-Tools — die kanonische Toggle-Operation für REPL/TUI.
    pub fn rewire(&self, agent: &mut Agent, base: &ToolRegistry) {
        let mut reg = base.clone();
        self.register_enabled(&mut reg);
        agent.tools = reg;
    }

    pub fn find(&self, name: &str) -> Option<&McpServer> {
        self.servers.iter().find(|s| s.spec.name == name)
    }

    /// Schaltet einen Server um. Fehler, wenn unbekannt oder (beim Einschalten) nicht
    /// verbunden. Wirkt sofort auf neu gespawnte Sub-Agenten; den Haupt-Agenten muss
    /// das Frontend danach neu verdrahten (`register_enabled` auf die Basis-Registry).
    pub fn set_enabled(&self, name: &str, on: bool) -> Result<bool, String> {
        let s = self
            .find(name)
            .ok_or_else(|| format!("unbekannter MCP-Server '{name}'"))?;
        if on && s.client.is_none() {
            let why = s
                .error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            return Err(format!("'{name}' ist nicht verbunden{why}"));
        }
        s.set(on);
        Ok(on)
    }
}
