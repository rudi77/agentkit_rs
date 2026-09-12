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
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Wie lange auf die Antwort auf einen Handshake-Request gewartet wird. Kurz:
/// wer sich nicht zügig meldet, darf den Start des Agenten nicht aufhalten.
/// Per `AGENTKIT_MCP_HANDSHAKE_TIMEOUT` (Sekunden) überschreibbar — ein per
/// Paketmanager gestarteter Server (`uv run`, `npx -y`) löst beim ERSTEN Start
/// Abhängigkeiten auf, das dauert deutlich länger als der Warmstart.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Wie lange auf das Ergebnis eines `tools/call` gewartet wird. Großzügig — ein
/// MCP-Tool darf echte Arbeit tun (dieselbe Größenordnung wie `run_shell`).
/// Per `AGENTKIT_MCP_CALL_TIMEOUT` (Sekunden) überschreibbar.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Obergrenze für einen aus der Umgebung gelesenen Timeout-Wert (1 Tag). Ohne sie
/// würde ein sehr großer, aber syntaktisch gültiger Wert (Tippfehler, z. B. eine
/// Ziffer zu viel) als `Duration` bis in `Inner::rpc` durchgereicht, wo
/// `Instant::now() + timeout` bei einer zu großen `Duration` mit Overflow paniert.
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Parst den Sekundenwert einer Timeout-Umgebungsvariable. Leer, fehlend, nicht als
/// Ganzzahl lesbar, `0` oder größer als [`MAX_TIMEOUT_SECS`] -> still `default` (ein
/// Tippfehler in der Variable soll den Server-Start nicht verhindern, ein Zahlendreher
/// nicht in den `Instant`-Overflow laufen). Nimmt den Wert als `Option<&str>` entgegen
/// statt selbst `std::env::var` aufzurufen — Env-Variablen sind prozessglobal, das
/// hält die Parse-Logik ohne `set_var` testbar.
fn parse_timeout_secs(value: Option<&str>, default: Duration) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&secs| (1..=MAX_TIMEOUT_SECS).contains(&secs))
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Liest eine Timeout-Umgebungsvariable und fällt auf `default` zurück. Wird pro
/// `connect`/Aufruf neu gelesen — kein globaler Cache nötig, da `connect` einmal
/// pro Server läuft und ein Tool-Call kein heißer Pfad ist.
fn timeout_from_env(var: &str, default: Duration) -> Duration {
    parse_timeout_secs(std::env::var(var).ok().as_deref(), default)
}

/// Das Call-Timeout für `tools/call`, jedes Mal frisch aus der Umgebung gelesen
/// (`AGENTKIT_MCP_CALL_TIMEOUT`) — EINE Stelle statt einer Kopie in `call_tool`
/// und in der `register`-Closure.
fn call_timeout() -> Duration {
    timeout_from_env("AGENTKIT_MCP_CALL_TIMEOUT", CALL_TIMEOUT)
}

/// Ergänzt eine Handshake-Timeout-Meldung (erkannt am Marker [`TIMEOUT_MARKER`] aus
/// `Inner::rpc`) um Ursache und Ausweg. Andere Fehler (z. B. `CLOSED`) bleiben
/// unverändert — der Hinweis gehört nur zum Handshake, nicht zu jedem `tools/call`.
fn mit_kaltstart_hinweis(err: String) -> String {
    if err.contains(TIMEOUT_MARKER) {
        format!(
            "{err} — vermutlich Kaltstart eines paketmanager-gestarteten Servers \
             ('uv run'/'npx -y' löst beim ersten Start Abhängigkeiten auf): Server \
             einmal manuell starten oder AGENTKIT_MCP_HANDSHAKE_TIMEOUT hochsetzen"
        )
    } else {
        err
    }
}

/// Eine Meldung für "der Server ist weg", egal ob es beim Schreiben oder beim
/// Lesen auffällt.
const CLOSED: &str = "MCP-Server hat die Verbindung geschlossen";

/// Fester Teilstring der Timeout-Meldung aus [`Inner::rpc`]. AN EINER STELLE
/// definiert und sowohl beim Formatieren dort als auch beim Erkennen in
/// [`mit_kaltstart_hinweis`] verwendet — sonst könnten Erzeugung und Erkennung
/// unbemerkt auseinanderlaufen (z. B. bei einer künftigen Umformulierung).
const TIMEOUT_MARKER: &str = "MCP-Timeout";

struct Session {
    stdin: ChildStdin,
    /// Gelesene stdout-Zeilen des Servers. Ein eigener Lese-Thread statt eines
    /// direkten `read_line`: nur so lässt sich das Warten überhaupt begrenzen —
    /// `ChildStdout` kennt portabel kein Lese-Timeout. Ohne das blockierte ein
    /// hängender Server unbegrenzt, und zwar unter dem Session-Mutex: alle
    /// anderen MCP-Aufrufe standen mit still.
    lines: Receiver<String>,
    _child: Child,
}

struct Inner {
    session: Mutex<Session>,
    id: AtomicU64,
}

impl Inner {
    /// Ein JSON-RPC-Request mit Antwort (blockierend bis zur passenden `id`,
    /// höchstens aber `timeout`).
    fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut sess = self.session.lock().unwrap();
        // Ein toter Server fällt je nach Timing beim Schreiben (EPIPE) oder erst
        // beim Lesen auf — beides ist dieselbe Lage und meldet sich gleich.
        let mut schreiben = || -> std::io::Result<()> {
            writeln!(sess.stdin, "{req}")?;
            sess.stdin.flush()
        };
        if schreiben().is_err() {
            return Err(CLOSED.to_string());
        }

        // Zeilen lesen, bis die Antwort mit unserer id kommt (Notifications
        // überspringen). Die Frist gilt für den GANZEN Request, nicht je Zeile —
        // sonst könnte ein Server sie mit Notifications endlos verlängern.
        let deadline = Instant::now() + timeout;
        loop {
            let rest = deadline.saturating_duration_since(Instant::now());
            let line = match sess.lines.recv_timeout(rest) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "{TIMEOUT_MARKER} nach {}s bei '{method}'",
                        timeout.as_secs()
                    ))
                }
                Err(RecvTimeoutError::Disconnected) => return Err(CLOSED.to_string()),
            };
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

/// Liest stdout des Servers zeilenweise in einen Kanal. Endet mit EOF, einem
/// Lesefehler oder wenn niemand mehr zuhört (die [`Session`] wurde verworfen).
fn spawn_reader(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(std::mem::take(&mut line)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
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
        let stdout = child.stdout.take().ok_or("kein stdout")?;

        let inner = Arc::new(Inner {
            session: Mutex::new(Session {
                stdin,
                lines: spawn_reader(stdout),
                _child: child,
            }),
            id: AtomicU64::new(1),
        });

        // Handshake: initialize -> initialized -> tools/list.
        let handshake_timeout =
            timeout_from_env("AGENTKIT_MCP_HANDSHAKE_TIMEOUT", HANDSHAKE_TIMEOUT);
        inner
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "agentkit-rs", "version": "0.1.0"},
                }),
                handshake_timeout,
            )
            .map_err(mit_kaltstart_hinweis)?;
        inner.notify("notifications/initialized", json!({}))?;
        let listed = inner
            .rpc("tools/list", json!({}), handshake_timeout)
            .map_err(mit_kaltstart_hinweis)?;
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
        let result = self.inner.rpc(
            "tools/call",
            json!({"name": name, "arguments": args}),
            call_timeout(),
        )?;
        Ok(mcp_text(&result))
    }

    /// Klinkt die Server-Tools in eine ToolRegistry ein (namespaced), aber nur jene,
    /// deren Name in `keep` steht (der aktive Tool-Filter, siehe [`aktive_tools`]).
    ///
    /// Parametererweiterung statt einer zusätzlichen `register_filtered`-Methode: im
    /// gesamten Repo (inkl. agentkit_app, agentkit_swarm, agentkit_work, agentkit_viz,
    /// Beispiele, Tests) gibt es genau EINEN Aufrufer — [`McpHub::register_enabled`] —,
    /// eine zweite Methode für einen einzigen Nutzer wäre Abstraktion ohne Bedarf
    /// (CODING_GUIDELINES.md, Regel 2 "Rule of Three").
    pub fn register(&self, registry: &mut ToolRegistry, prefix: &str, keep: &BTreeSet<String>) {
        for t in &self.tools {
            let name = tool_name(t).to_string();
            if !keep.contains(&name) {
                continue;
            }
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
                        call_timeout(),
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
    /// Allowlist der Tool-Namen aus `"tools": [...]` in der Config. Leer/fehlend
    /// heißt "alle Tools des Servers" — ein Server mit Dutzenden Tools (Azure-MCP:
    /// 70) muss nicht alle in die Registry bringen, siehe [`aktive_tools`].
    pub tools: Vec<String>,
}

/// Ermittelt aus dem Tool-Angebot eines Servers und seiner Allowlist die aktiven
/// Tools sowie die Allowlist-Einträge, die der Server GAR NICHT anbietet.
///
/// Leere `allowlist` -> alle `angeboten` sind aktiv, keine unbekannten Einträge.
/// Sonst -> aktiv ist die Schnittmenge; der zweite Rückgabewert (sortiert, ohne
/// Duplikate) ist für eine Warnung im Frontend gedacht — ein Tippfehler in der
/// Allowlist soll ein Tool nicht stillschweigend verschlucken, sondern auffallen.
fn aktive_tools(angeboten: &[String], allowlist: &[String]) -> (BTreeSet<String>, Vec<String>) {
    if allowlist.is_empty() {
        return (angeboten.iter().cloned().collect(), Vec::new());
    }
    let angeboten_set: BTreeSet<&str> = angeboten.iter().map(String::as_str).collect();
    let mut aktiv = BTreeSet::new();
    let mut unbekannt = BTreeSet::new();
    for name in allowlist {
        if angeboten_set.contains(name.as_str()) {
            aktiv.insert(name.clone());
        } else {
            unbekannt.insert(name.clone());
        }
    }
    (aktiv, unbekannt.into_iter().collect())
}

/// Der Name EINER rohen MCP-Tool-Definition (leer, wenn das Feld fehlt). An einer
/// Stelle definiert, weil sowohl die Registrierung ([`MCPClient::register`]) als
/// auch die Namensliste ([`tool_names_of`]) dieselbe Extraktion brauchen — sonst
/// müsste ein geändertes Fallback-Verhalten an zwei Stellen nachgezogen werden.
fn tool_name(t: &Value) -> &str {
    t.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Tool-Namen aus rohen MCP-Tool-Definitionen (wie in `MCPClient::tools`),
/// alphabetisch sortiert — stabile Reihenfolge in Listen und Panels.
fn tool_names_of(tools: &[Value]) -> Vec<String> {
    // Namenlose Definitionen fallen raus: ihr Function-Name wäre `mcp__<server>__`
    // und damit ohnehin nicht aufrufbar — als leere Zeile in Liste und Panel wären
    // sie nur verwirrend.
    let mut namen: Vec<String> = tools
        .iter()
        .map(|t| tool_name(t).to_string())
        .filter(|n| !n.is_empty())
        .collect();
    namen.sort();
    namen
}

/// Warnzeilen für benutzerweite Server, die die projekt-/explizite Ebene unter
/// DEMSELBEN Namen ersetzt hat — eine je Kollision, in der beide Kommandos stehen.
///
/// Sicherheitsrelevant, deshalb nicht still: die Präzedenz „Projekt gewinnt" ist
/// gewollt, aber ein MCP-Server wird beim Start ohne Rückfrage als Prozess
/// ausgeführt (anders als `run_shell` gibt es hier keinen `ApproveFn`). Eine
/// `.mcp.json` aus einem fremden Repository kann damit den Namen eines Servers
/// übernehmen, dem der Anwender global vertraut — und über den Namen greift auch
/// die `--mcp`-Allowlist. Wer die Übernahme sieht, kann sie prüfen; still wäre
/// sie ununterscheidbar vom eigenen Server. Kollisionen mit identischem Kommando
/// bleiben unerwähnt: dort ändert sich nichts, was zu prüfen wäre.
fn shadow_warnings(user: &[McpServerSpec], project: &[McpServerSpec]) -> Vec<String> {
    let zeile = |s: &McpServerSpec| {
        if s.args.is_empty() {
            s.command.clone()
        } else {
            format!("{} {}", s.command, s.args.join(" "))
        }
    };
    project
        .iter()
        .filter_map(|p| {
            let u = user.iter().find(|u| u.name == p.name)?;
            (zeile(u) != zeile(p)).then(|| {
                format!(
                    "MCP '{}': die Projekt-Config ersetzt den benutzerweiten Server \
                     ('{}' statt '{}') — Kommando prüfen, MCP-Server starten ohne Rückfrage",
                    p.name,
                    zeile(p),
                    zeile(u)
                )
            })
        })
        .collect()
}

/// Welche Einträge der `tools`-Allowlist der Server gar nicht anbietet (= Tippfehler).
/// Reine Funktion, damit beide Fälle ohne laufenden Server-Prozess testbar sind:
/// `verbunden == false` liefert IMMER eine leere Liste, weil ein nicht gestarteter
/// Server nichts anbietet und dann auch die vollständig korrekte Allowlist als
/// „unbekannt" gälte — die Warnung wäre schlicht falsch.
fn unbekannte_tools(verbunden: bool, angeboten: &[String], allowlist: &[String]) -> Vec<String> {
    if !verbunden {
        return Vec::new();
    }
    aktive_tools(angeboten, allowlist).1
}

/// Wie ein Frontend die Tool-Anzahl eines Servers benennt: `"{aktiv}/{angeboten}
/// Tools"`, sobald ein Tool-Filter greift, sonst nur `"{angeboten} Tools"`.
/// Bewusst hier und nicht im Frontend: REPL und TUI trafen dieselbe Entscheidung
/// vorher unabhängig voneinander und liefen bei der ersten Änderung auseinander.
pub fn tool_count_label(aktiv: usize, angeboten: usize) -> String {
    if aktiv < angeboten {
        format!("{aktiv}/{angeboten} Tools")
    } else {
        format!("{angeboten} Tools")
    }
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
        // Warum: die Tools eines Servers werden als `mcp__<server>__<tool>`
        // registriert (`mcp_prefix`) und landen so 1:1 als Function-Name in der
        // OpenAI-API. Ein Name mit Leerzeichen/Sonderzeichen erzeugt einen
        // Function-Namen, den die API ablehnt — ohne diese Prüfung scheitert das
        // erst spät (beim ersten Tool-Call) und unverständlich.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "{path}: ungültiger MCP-Servername '{name}' — erlaubt sind nur \
                 Buchstaben, Ziffern, '_' und '-'"
            ));
        }
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
        let tools = v
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        out.push(McpServerSpec {
            name: name.clone(),
            command,
            args,
            env,
            disabled,
            tools,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Sucht die PROJEKT-Ebene der MCP-Config: zuerst `<workspace>/.mcp.json`, dann
/// `./.mcp.json`, dann die `mcp.json`-Varianten. Gibt den ersten existierenden Pfad
/// zurück. Ergänzt wird das um die benutzerweite Ebene ([`user_mcp_config`]), die
/// [`McpHub::from_config`] unabhängig davon immer mit einbezieht.
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

/// Pfad der benutzerweiten `.mcp.json` (`<config_dir>/.mcp.json`, siehe
/// [`crate::config::config_dir`] — `$AGENTKIT_HOME`, sonst `~/.agentkit` bzw.
/// `%USERPROFILE%\.agentkit`). `None`, wenn kein Konfigurationsverzeichnis
/// ermittelbar ist ODER die Datei dort nicht existiert.
pub fn user_mcp_config() -> Option<String> {
    let dir = crate::config::config_dir()?;
    let path = dir.join(".mcp.json");
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// EIN Server im [`McpHub`]: Deklaration + (ggf.) verbundene Session + Enable-Flag.
pub struct McpServer {
    pub spec: McpServerSpec,
    /// `Some`, wenn der Handshake glückte; sonst steht der Grund in `error`.
    pub client: Option<MCPClient>,
    /// Atomar, damit der Hub `&self`-umschaltbar und `Arc`-teilbar bleibt.
    pub enabled: AtomicBool,
    pub error: Option<String>,
    /// Aktuell aktive Tools dieses Servers (Schnittmenge aus Angebot und
    /// `spec.tools`, siehe [`aktive_tools`]). `Mutex` statt `AtomicBool`, weil es
    /// eine Menge ist, kein Flag — gesperrt wird nur beim Umschalten
    /// ([`McpHub::set_tool_enabled`]) und beim Neuaufbau der Registry
    /// ([`McpHub::register_enabled`]), also kein heißer Pfad: ein Tool-*Call*
    /// nimmt diesen Lock nicht.
    active_tools: Mutex<BTreeSet<String>>,
    /// Alle vom Server angebotenen Tool-Namen, alphabetisch — EINMAL beim Connect
    /// berechnet. Das Angebot eines verbundenen Servers ändert sich zur Laufzeit
    /// nie, die TUI liest es aber pro Frame und pro Tastendruck; ohne den Cache
    /// würde dort jedes Mal die ganze Liste neu alloziert und sortiert.
    tool_names: Vec<String>,
}

impl McpServer {
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Anzahl angebotener Tools (0, falls nicht verbunden).
    pub fn tool_count(&self) -> usize {
        self.client.as_ref().map_or(0, |c| c.tools.len())
    }
    /// Alle vom Server angebotenen Tool-Namen, alphabetisch (leer, falls nicht
    /// verbunden).
    pub fn tool_names(&self) -> &[String] {
        &self.tool_names
    }
    /// Einträge der `tools`-Allowlist, die der Server gar nicht anbietet — ein
    /// Tippfehler, vor dem das Frontend warnen soll. **Nur bei verbundenem
    /// Server aussagekräftig**: ein Server, der nicht startet, bietet gar nichts
    /// an, und die vollständige (korrekte!) Allowlist sähe dann wie ein einziger
    /// Tippfehler aus. Deshalb hier leer, solange nicht verbunden — der Grund
    /// steht dann ohnehin in `error`.
    pub fn unknown_tools(&self) -> Vec<String> {
        unbekannte_tools(self.is_connected(), &self.tool_names, &self.spec.tools)
    }
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    /// Ist `tool` in der aktiven Menge dieses Servers?
    pub fn is_tool_enabled(&self, tool: &str) -> bool {
        self.active_tools.lock().unwrap().contains(tool)
    }
    /// Anzahl aktuell aktiver Tools (0, falls nicht verbunden oder die Allowlist
    /// nichts trifft).
    pub fn active_tool_count(&self) -> usize {
        self.active_tools.lock().unwrap().len()
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
    /// Fertige Warnzeilen zu benutzerweiten Servern, die die Projekt-Ebene unter
    /// demselben Namen mit einem ANDEREN Kommando ersetzt hat (siehe
    /// [`shadow_warnings`]). Der Hub trägt sie nur; ausgeben muss sie das
    /// Frontend — wie bei allen anderen Meldungen hier auch.
    pub shadow_warnings: Vec<String>,
}

/// Merged die benutzerweite und die projekt-/explizite Server-Liste zu einer
/// Ebene. Bei gleichem Servernamen gewinnt `project` (näher am Arbeitsverzeichnis
/// = spezifischer) — `user` liefert nur den Fallback für Server, die das Projekt
/// nicht kennt. Ergebnis alphabetisch sortiert (stabile Reihenfolge in
/// Listen/Panel, wie [`load_mcp_config`]). Reine Funktion OHNE `config_dir()`-
/// Aufruf — so bleibt sie ohne `std::env::set_var` testbar (Env-Variablen sind
/// prozessglobal und `set_var` ist in neueren Rust-Versionen `unsafe`).
fn merge_specs(user: Vec<McpServerSpec>, project: Vec<McpServerSpec>) -> Vec<McpServerSpec> {
    let mut by_name: std::collections::BTreeMap<String, McpServerSpec> =
        user.into_iter().map(|s| (s.name.clone(), s)).collect();
    for s in project {
        by_name.insert(s.name.clone(), s);
    }
    // BTreeMap iteriert bereits in Schlüsselreihenfolge (= alphabetisch nach Name).
    by_name.into_values().collect()
}

/// Vergleicht zwei Pfade auf "dieselbe Datei" — kanonisiert, mit Fallback auf
/// String-Vergleich, falls `canonicalize` scheitert (z. B. Datei existiert
/// nicht). Nötig, weil `AGENTKIT_HOME` auf dasselbe Verzeichnis wie der
/// Workspace zeigen kann — dann wäre die benutzerweite `.mcp.json` dieselbe
/// Datei wie die projekt-/explizite und dürfte nicht doppelt geladen werden.
fn same_file(a: &str, b: &str) -> bool {
    match (
        std::path::Path::new(a).canonicalize(),
        std::path::Path::new(b).canonicalize(),
    ) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
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
            // Nicht verbunden -> kein Angebot -> leere aktive Menge (siehe
            // `aktive_tools`-Doku). Verbunden -> Schnittmenge aus Angebot und
            // `spec.tools`-Allowlist (leere Allowlist = alle Tools aktiv).
            let angeboten = client
                .as_ref()
                .map(|c| tool_names_of(&c.tools))
                .unwrap_or_default();
            let (aktiv, _unbekannt) = aktive_tools(&angeboten, &spec.tools);
            servers.push(McpServer {
                spec,
                client,
                enabled: AtomicBool::new(enabled),
                error,
                active_tools: Mutex::new(aktiv),
                tool_names: angeboten,
            });
        }
        McpHub {
            servers,
            shadow_warnings: Vec::new(),
        }
    }

    /// Komfort fürs Frontend: lädt die MCP-Config aus ZWEI Ebenen und merged sie
    /// (siehe [`merge_specs`]) — benutzerweit ([`user_mcp_config`],
    /// `<config_dir>/.mcp.json`) und projekt-/explizit (`config_path`, sonst
    /// Auto-Discovery im `workspace`/CWD via [`discover_mcp_config`]). Bei
    /// Namenskollision gewinnt die Projekt-/explizite Ebene. Die benutzerweite
    /// Ebene ist immer dabei, auch wenn `config_path` explizit gesetzt ist — nur
    /// `--no-mcp` im Frontend schaltet MCP komplett ab (indem `from_config` dann
    /// gar nicht erst gerufen wird). Zeigen beide Ebenen auf dieselbe Datei (z. B.
    /// `AGENTKIT_HOME` == CWD), wird sie nur einmal geladen. Bestimmt danach den
    /// Startzustand (Allowlist `enable` ODER alle nicht-`disabled`) und verbindet.
    /// Fehlen beide Ebenen, ist das Ergebnis ein leerer Hub (kein Fehler); ein
    /// Parse-/IO-Fehler in einer der beiden Configs wird als `Err` gemeldet.
    /// Logging macht der Aufrufer.
    pub fn from_config(
        workspace: &str,
        config_path: Option<&str>,
        enable: &[String],
        connect_all: bool,
    ) -> Result<McpHub, String> {
        let project_path = match config_path {
            Some(p) => Some(p.to_string()),
            None => discover_mcp_config(workspace),
        };
        // Die benutzerweite Ebene ist per Definition IMMER dabei (auch wenn
        // config_path explizit gesetzt ist) — nur `--no-mcp` im Frontend schaltet
        // MCP komplett ab, indem `from_config` dann gar nicht erst gerufen wird.
        let mut user_path = user_mcp_config();
        // Dedupe über den Pfad: zeigen beide Ebenen auf dieselbe Datei (z. B.
        // AGENTKIT_HOME == CWD), wird sie nur einmal geladen.
        if let (Some(u), Some(p)) = (&user_path, &project_path) {
            if same_file(u, p) {
                user_path = None;
            }
        }

        // Eine fehlende Ebene ist kein Fehler, ein Parse-/IO-Fehler der vorhandenen
        // schon (`?`) — deshalb `transpose`: Option<Result<…>> -> Result<Option<…>>.
        let load = |p: &Option<String>| -> Result<Vec<McpServerSpec>, String> {
            Ok(p.as_deref()
                .map(load_mcp_config)
                .transpose()?
                .unwrap_or_default())
        };
        let (user_specs, project_specs) = (load(&user_path)?, load(&project_path)?);
        let warnungen = shadow_warnings(&user_specs, &project_specs);
        let specs = merge_specs(user_specs, project_specs);
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
        let mut hub = McpHub::connect(
            specs,
            move |name| enable_set.iter().any(|n| n == name),
            connect_all,
        );
        hub.shadow_warnings = warnungen;
        Ok(hub)
    }

    /// Klinkt die Tools aller AKTIVEN (verbundenen + enabled) Server in `reg` ein —
    /// namespaced `mcp__<server>__<tool>`, gefiltert auf die aktive Tool-Menge des
    /// jeweiligen Servers ([`McpServer::is_tool_enabled`]).
    pub fn register_enabled(&self, reg: &mut ToolRegistry) {
        for s in &self.servers {
            if s.is_enabled() {
                if let Some(c) = &s.client {
                    let aktiv = s.active_tools.lock().unwrap();
                    c.register(reg, &mcp_prefix(&s.spec.name), &aktiv);
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

    /// Schaltet EIN Tool eines Servers um (der Tool-Filter, analog [`set_enabled`]
    /// auf Server-Ebene). Wirkt sofort auf neu gespawnte Sub-Agenten; den
    /// Haupt-Agenten muss das Frontend danach neu verdrahten (`register_enabled`
    /// auf die Basis-Registry, siehe [`rewire`]).
    ///
    /// Fehler (deutsch), wenn der Server unbekannt ist, nicht verbunden ist, oder
    /// das Tool gar nicht angeboten wird — ein Tippfehler in `tool` soll nicht
    /// stillschweigend ins Leere laufen.
    pub fn set_tool_enabled(&self, server: &str, tool: &str, on: bool) -> Result<(), String> {
        let s = self
            .find(server)
            .ok_or_else(|| format!("unbekannter MCP-Server '{server}'"))?;
        if !s.is_connected() {
            return Err(format!("'{server}' ist nicht verbunden"));
        }
        if !s.tool_names.iter().any(|n| n == tool) {
            return Err(format!("Server '{server}' bietet kein Tool '{tool}' an"));
        }
        let mut aktiv = s.active_tools.lock().unwrap();
        if on {
            aktiv.insert(tool.to_string());
        } else {
            aktiv.remove(tool);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, command: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: Vec::new(),
            disabled: false,
            tools: Vec::new(),
        }
    }

    /// Leere Allowlist -> alle angebotenen Tools sind aktiv, keine unbekannten.
    #[test]
    fn aktive_tools_leere_allowlist_aktiviert_alles() {
        let angeboten = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (aktiv, unbekannt) = aktive_tools(&angeboten, &[]);
        assert_eq!(
            aktiv,
            ["a", "b", "c"].map(String::from).into_iter().collect()
        );
        assert!(unbekannt.is_empty());
    }

    /// Nicht-leere Allowlist -> aktive Menge ist die Schnittmenge mit dem Angebot.
    #[test]
    fn aktive_tools_bildet_schnittmenge() {
        let angeboten = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let allowlist = vec!["b".to_string(), "c".to_string()];
        let (aktiv, unbekannt) = aktive_tools(&angeboten, &allowlist);
        assert_eq!(aktiv, ["b", "c"].map(String::from).into_iter().collect());
        assert!(unbekannt.is_empty());
    }

    /// Allowlist-Einträge, die der Server nicht anbietet, landen sortiert im
    /// zweiten Rückgabewert — ein Tippfehler soll nicht stillschweigend verschluckt
    /// werden, sondern im Frontend als Warnung auffallen.
    #[test]
    fn aktive_tools_meldet_unbekannte_namen_sortiert() {
        let angeboten = vec!["a".to_string()];
        let allowlist = vec!["z".to_string(), "a".to_string(), "m".to_string()];
        let (aktiv, unbekannt) = aktive_tools(&angeboten, &allowlist);
        assert_eq!(aktiv, ["a"].map(String::from).into_iter().collect());
        assert_eq!(unbekannt, vec!["m".to_string(), "z".to_string()]);
    }

    /// Trifft die Allowlist gar nichts vom Angebot, bleibt die aktive Menge leer.
    #[test]
    fn aktive_tools_ohne_treffer_ist_aktive_menge_leer() {
        let angeboten = vec!["a".to_string(), "b".to_string()];
        let allowlist = vec!["x".to_string(), "y".to_string()];
        let (aktiv, unbekannt) = aktive_tools(&angeboten, &allowlist);
        assert!(aktiv.is_empty());
        assert_eq!(unbekannt, vec!["x".to_string(), "y".to_string()]);
    }

    /// Bei gleichem Namen gewinnt die Projekt-Ebene — `user` liefert nur den
    /// Fallback für Server, die die Projekt-Ebene nicht kennt. Ergebnis alphabetisch.
    #[test]
    fn merge_specs_projekt_gewinnt_bei_namenskollision() {
        let user = vec![spec("git", "user-git"), spec("only-user", "u")];
        let project = vec![spec("git", "project-git"), spec("only-project", "p")];
        let merged = merge_specs(user, project);
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["git", "only-project", "only-user"]);
        let git = merged.iter().find(|s| s.name == "git").unwrap();
        assert_eq!(git.command, "project-git");
    }

    /// Namen, die nur in einer der beiden Listen vorkommen, bleiben erhalten (Union).
    #[test]
    fn merge_specs_union_ohne_kollision() {
        let user = vec![spec("fs", "u")];
        let project = vec![spec("git", "p")];
        let merged = merge_specs(user, project);
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["fs", "git"]);
    }

    /// Ersetzt die Projekt-Config einen benutzerweiten Server durch ein ANDERES
    /// Kommando, muss das sichtbar werden — ein MCP-Server startet ohne Rückfrage.
    #[test]
    fn shadow_warnings_meldet_ersetzte_benutzerweite_server() {
        let user = vec![spec("filesystem", "mein-fs-server")];
        let project = vec![spec("filesystem", "boeses-programm")];
        let warnungen = shadow_warnings(&user, &project);
        assert_eq!(warnungen.len(), 1);
        assert!(warnungen[0].contains("filesystem"), "{}", warnungen[0]);
        assert!(warnungen[0].contains("boeses-programm"), "{}", warnungen[0]);
        assert!(warnungen[0].contains("mein-fs-server"), "{}", warnungen[0]);
    }

    /// Kein Rauschen, wo nichts zu prüfen ist: gleiches Kommando in beiden Ebenen
    /// (und Namen, die nur in einer Ebene stehen) erzeugen keine Warnung.
    #[test]
    fn shadow_warnings_schweigt_ohne_echte_aenderung() {
        let user = vec![spec("fs", "gleiches-kommando"), spec("nur-user", "x")];
        let project = vec![spec("fs", "gleiches-kommando"), spec("nur-projekt", "y")];
        assert!(shadow_warnings(&user, &project).is_empty());
    }

    /// Ein Server, der nicht startet, bietet nichts an — dann darf die (womöglich
    /// völlig korrekte) Allowlist NICHT als Tippfehler gemeldet werden. Vorher
    /// bekam man in genau dieser Lage „unbekannte Tools (Tippfehler?)" für jeden
    /// Eintrag, obwohl nur der Server fehlte.
    #[test]
    fn unbekannte_tools_schweigt_wenn_der_server_nicht_verbunden_ist() {
        let allowlist = vec!["storage".to_string(), "keyvault".to_string()];
        assert!(unbekannte_tools(false, &[], &allowlist).is_empty());
    }

    /// Bei verbundenem Server bleibt die Tippfehler-Erkennung scharf.
    #[test]
    fn unbekannte_tools_meldet_tippfehler_bei_verbundenem_server() {
        let angeboten = vec!["storage".to_string(), "keyvault".to_string()];
        let allowlist = vec!["storage".to_string(), "storrage".to_string()];
        assert_eq!(
            unbekannte_tools(true, &angeboten, &allowlist),
            vec!["storrage".to_string()]
        );
    }

    /// Regressionstest für den Ursprungs-Bug: ein Server, der gar nicht startet
    /// (`Command::spawn` scheitert), darf `McpHub::connect` NICHT dazu bringen,
    /// die komplette (evtl. völlig korrekte) `tools`-Allowlist als "unbekannt"
    /// zu melden — vorher wurde die leere Verbindung als "bietet nichts an"
    /// gelesen, und `aktive_tools` meldete dann jeden Allowlist-Eintrag als
    /// Tippfehler. Über den echten `connect`-Pfad statt nur die reine Funktion
    /// [`unbekannte_tools`] direkt zu testen — der Bug saß am Zusammenspiel.
    #[test]
    fn connect_meldet_bei_nicht_verbundenem_server_keine_unbekannten_tools() {
        let mut kaputt = spec("kaputt", "es-gibt-dieses-programm-nicht-xyz");
        kaputt.tools = vec!["irgendein_tool".to_string(), "noch_eins".to_string()];
        let hub = McpHub::connect(vec![kaputt], |_| true, false);
        let server = hub.find("kaputt").expect("Server steht im Hub");
        assert!(!server.is_connected());
        assert!(server.unknown_tools().is_empty());
    }

    /// Ohne Filter nennt das Label nur die Anzahl.
    #[test]
    fn tool_count_label_ohne_filter_zeigt_nur_die_anzahl() {
        assert_eq!(tool_count_label(5, 5), "5 Tools");
        assert_eq!(tool_count_label(0, 0), "0 Tools");
    }

    /// Mit Filter zeigt das Label aktiv von angeboten.
    #[test]
    fn tool_count_label_mit_filter_zeigt_aktiv_von_angeboten() {
        assert_eq!(tool_count_label(2, 70), "2/70 Tools");
        assert_eq!(tool_count_label(0, 3), "0/3 Tools");
    }

    /// Ein gültiger Servername (Buchstaben, Ziffern, `_`, `-`) lädt fehlerfrei.
    #[test]
    fn load_mcp_config_erlaubt_gueltigen_namen() {
        let dir =
            std::env::temp_dir().join(format!("agentkit_mcpvalidname_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"my-server_1": {"command": "uvx"}}}"#,
        )
        .unwrap();
        let specs = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "my-server_1");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ein Servername mit Leerzeichen liefert `Err` mit einem sinnvollen Hinweis.
    #[test]
    fn load_mcp_config_lehnt_namen_mit_leerzeichen_ab() {
        let dir =
            std::env::temp_dir().join(format!("agentkit_mcpinvalidname_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"mein server": {"command": "uvx"}}}"#,
        )
        .unwrap();
        let err = load_mcp_config(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("mein server") || err.contains("Buchstaben"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ein gültiger Wert wird als Sekunden übernommen.
    #[test]
    fn parse_timeout_secs_gueltiger_wert() {
        let d = parse_timeout_secs(Some("42"), Duration::from_secs(15));
        assert_eq!(d, Duration::from_secs(42));
    }

    /// Ungültige Werte (kein Integer, negativ, leer, Null, zu groß) fallen still
    /// auf den Default zurück — ein Tippfehler in der Env-Variable darf weder den
    /// Server-Start verhindern noch (bei einer zu großen Zahl) `Inner::rpc` in
    /// einen `Instant`-Overflow laufen lassen.
    #[test]
    fn parse_timeout_secs_ungueltiger_wert_faellt_auf_default_zurueck() {
        let default = Duration::from_secs(15);
        assert_eq!(
            parse_timeout_secs(Some("nicht-numerisch"), default),
            default
        );
        assert_eq!(parse_timeout_secs(Some("-5"), default), default);
        assert_eq!(parse_timeout_secs(Some(""), default), default);
        assert_eq!(parse_timeout_secs(Some("0"), default), default);
        assert_eq!(
            parse_timeout_secs(Some(&(MAX_TIMEOUT_SECS + 1).to_string()), default),
            default
        );
    }

    /// Fehlende Variable (kein `Some`) -> Default.
    #[test]
    fn parse_timeout_secs_fehlende_variable_faellt_auf_default_zurueck() {
        let default = Duration::from_secs(120);
        assert_eq!(parse_timeout_secs(None, default), default);
    }

    /// Der Kaltstart-Hinweis wird nur an eine Timeout-Meldung angehängt, nicht an
    /// andere Fehler (z. B. `CLOSED`) — der Marker koppelt Erzeugung (`Inner::rpc`)
    /// und Erkennung, ohne dass die Formulierung sonst irgendwo dupliziert wird.
    #[test]
    fn mit_kaltstart_hinweis_nur_bei_timeout_meldung() {
        let timeout_err = format!("{TIMEOUT_MARKER} nach 15s bei 'initialize'");
        let angereichert = mit_kaltstart_hinweis(timeout_err.clone());
        assert!(angereichert.starts_with(&timeout_err));
        assert!(angereichert.contains("AGENTKIT_MCP_HANDSHAKE_TIMEOUT"));

        let sonstiger_fehler = CLOSED.to_string();
        assert_eq!(
            mit_kaltstart_hinweis(sonstiger_fehler.clone()),
            sonstiger_fehler
        );
    }

    /// Ein Server-Prozess, der stdout füttert oder eben nicht — als Session verpackt.
    /// Die Skripte laufen bewusst nur wenige Sekunden: `Child::drop` beendet den
    /// Prozess nicht, ein langes `sleep` bliebe also als Waise im Testlauf stehen.
    #[cfg(unix)]
    fn fake_server(shell: &str) -> Inner {
        let mut child = Command::new("sh")
            .args(["-c", shell])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh startbar");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Inner {
            session: Mutex::new(Session {
                stdin,
                lines: spawn_reader(stdout),
                _child: child,
            }),
            id: AtomicU64::new(1),
        }
    }

    /// Ein Server, der nie antwortet, darf den Aufrufer nicht festhalten — vorher
    /// blockierte `read_line` unbegrenzt, und zwar unter dem Session-Mutex.
    #[test]
    #[cfg(unix)]
    fn rpc_bricht_nach_der_frist_ab() {
        let inner = fake_server("sleep 3");
        let start = Instant::now();
        let err = inner
            .rpc("tools/list", json!({}), Duration::from_millis(200))
            .unwrap_err();
        assert!(err.contains("Timeout"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hat zu lange gewartet"
        );
    }

    /// Notifications vor der Antwort werden übersprungen, die Antwort kommt an.
    #[test]
    #[cfg(unix)]
    fn rpc_ueberspringt_notifications_und_findet_die_antwort() {
        let inner = fake_server(
            r#"printf '{"jsonrpc":"2.0","method":"note"}\n{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'; sleep 2"#,
        );
        let out = inner
            .rpc("tools/list", json!({}), Duration::from_secs(5))
            .expect("Antwort");
        assert_eq!(out["ok"], true);
    }

    /// Stirbt der Server, endet der Lese-Thread und `rpc` meldet das sofort —
    /// statt bis zur Frist zu warten.
    #[test]
    #[cfg(unix)]
    fn rpc_meldet_geschlossene_verbindung_sofort() {
        let inner = fake_server("exit 0");
        let start = Instant::now();
        let err = inner
            .rpc("tools/list", json!({}), Duration::from_secs(30))
            .unwrap_err();
        assert!(err.contains("geschlossen"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
