//! agentkit als **ACP-Agent** (Agent Client Protocol) — der Weg in Editoren wie
//! Zed, die einen Agenten als Kindprozess starten und über stdio mit ihm reden.
//!
//! JSON-RPC, eine Nachricht je Zeile. Der Editor ruft `initialize`,
//! `session/new` (mit dem Projektverzeichnis) und `session/prompt`; der Agent
//! antwortet mit `session/update`-Benachrichtigungen (Text, Tool-Aufrufe, Plan)
//! und am Ende mit dem `stopReason`. Braucht `run_shell` eine Freigabe, fragt
//! der Agent per `session/request_permission` zurück — der Editor zeigt den
//! Dialog.
//!
//! **Warum Threads, obwohl das Repo synchron ist:** während ein Auftrag läuft,
//! muss der Agent weiter zuhören — auf `session/cancel` und auf die Antwort zu
//! einer eigenen Freigabe-Frage. Ein Lese-Thread verteilt deshalb die Zeilen:
//! Antworten an die wartende Rückfrage, alles andere an die Hauptschleife.
//! Jeder Auftrag läuft auf einem eigenen Thread; das ist dasselbe Muster wie
//! der Worker-Thread der CLI (`run_task`), keine async-Runtime.
//!
//! Der Kern bleibt unberührt: das Modul ist ein Frontend wie CLI und TUI — es
//! abonniert den [`EventBus`] und übersetzt [`AgentEvent`]s in Protokoll-
//! Nachrichten. Den Agenten selbst baut der Aufrufer ([`AgentFactory`]), damit
//! er dieselben Werkzeuge bekommt wie auf der Kommandozeile.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::agent::{new_cancel, Agent, Cancel};
use crate::coding::ApproveFn;
use crate::events::{AgentEvent, EventBus, EventData, DONE};
use crate::memory::truncate;
use crate::strategy::{run_with_strategy, RunStrategy};

/// Protokollversion (ACP zählt ganzzahlig).
const PROTOCOL_VERSION: u64 = 1;

/// So viel eines Tool-Ergebnisses geht in die Anzeige des Editors.
const MAX_TOOL_OUTPUT: usize = 4_000;

/// Abstand der Lebenszeichen für laufende Tool-Aufrufe.
///
/// ACP-Clients wie omnigent brechen einen Prompt nach einer Leerlauf-Frist ab
/// (dort 300 s ohne jede Nachricht des Agenten). Ein einzelnes langes Tool
/// (großes `grep`, `run_shell`) sendet zwischen `tool_call` und Ergebnis nichts —
/// ohne Heartbeat reißt die Frist, obwohl der Agent arbeitet. 30 s lässt zehn
/// Nachrichten Spielraum und ist für den Client nicht spürbar.
const TOOL_HEARTBEAT: Duration = Duration::from_secs(30);

/// Baut den Agenten einer neuen Sitzung: Arbeitsverzeichnis des Editors und der
/// Freigabe-Callback, der beim Editor nachfragt.
pub type AgentFactory = Arc<dyn Fn(&str, ApproveFn) -> Result<Agent, String> + Send + Sync>;

/// Die Leitung zum Editor: schreiben (unter einem Lock, weil Auftrags-Threads
/// und Hauptschleife gleichzeitig senden) und auf Antworten eigener Anfragen
/// warten.
struct Wire {
    out: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
}

impl Wire {
    fn send(&self, msg: &Value) {
        let mut out = self.out.lock().unwrap();
        // Ein geschlossener stdout heißt: der Editor ist weg. Dann gibt es
        // niemanden mehr, dem ein Fehler zu melden wäre.
        let _ = writeln!(out, "{msg}").and_then(|()| out.flush());
    }

    fn respond(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        );
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Eine eigene Anfrage an den Editor — blockiert, bis die Antwort kommt.
    /// `Err`, wenn die Verbindung vorher endet.
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let antwort = rx
            .recv()
            .map_err(|_| "Verbindung geschlossen".to_string())?;
        match antwort.get("error") {
            Some(e) => Err(e.to_string()),
            None => Ok(antwort["result"].clone()),
        }
    }
}

/// Eine Sitzung: der Agent (während eines Auftrags ausgeliehen) und der
/// Stop-Knopf des laufenden Auftrags.
struct Session {
    agent: Mutex<Option<Agent>>,
    cancel: Mutex<Option<Cancel>>,
}

/// Bedient einen Editor über `input`/`output`, bis dieser die Verbindung
/// schließt. Laufende Aufträge werden danach noch zu Ende geführt.
pub fn serve(
    factory: AgentFactory,
    strategy: RunStrategy,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
    version: &str,
) {
    let wire = Arc::new(Wire {
        out: Mutex::new(Box::new(output)),
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
    });
    let inbox = spawn_reader(input, wire.clone());
    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    let mut auftraege: Vec<std::thread::JoinHandle<()>> = Vec::new();

    for msg in inbox {
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        match (msg["method"].as_str().unwrap_or(""), id) {
            ("initialize", Some(id)) => wire.respond(
                &id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": false,
                        "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                    },
                    "authMethods": [],
                    "agentInfo": {"name": "agentkit", "version": version},
                }),
            ),
            ("authenticate", Some(id)) => wire.respond(&id, json!({})),
            ("session/new", Some(id)) => {
                let cwd = params["cwd"].as_str().unwrap_or(".").to_string();
                let sid = format!("sitzung-{}", sessions.len() + 1);
                let approve = permission_asker(wire.clone(), sid.clone());
                match factory(&cwd, approve) {
                    Ok(agent) => {
                        sessions.insert(
                            sid.clone(),
                            Arc::new(Session {
                                agent: Mutex::new(Some(agent)),
                                cancel: Mutex::new(None),
                            }),
                        );
                        wire.respond(&id, json!({"sessionId": sid}));
                    }
                    Err(e) => wire.error(&id, -32603, &e),
                }
            }
            ("session/prompt", Some(id)) => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let Some(session) = sessions.get(&sid).cloned() else {
                    wire.error(&id, -32602, &format!("unbekannte Sitzung '{sid}'"));
                    continue;
                };
                let Some(agent) = session.agent.lock().unwrap().take() else {
                    wire.error(&id, -32602, "in dieser Sitzung läuft bereits ein Auftrag");
                    continue;
                };
                let cancel = new_cancel();
                *session.cancel.lock().unwrap() = Some(cancel.clone());
                let task = prompt_text(&params["prompt"]);
                let wire = wire.clone();
                auftraege.push(std::thread::spawn(move || {
                    let mut agent = agent;
                    let ergebnis = run_prompt(&mut agent, &task, &sid, &cancel, &strategy, &wire);
                    *session.agent.lock().unwrap() = Some(agent);
                    *session.cancel.lock().unwrap() = None;
                    match ergebnis {
                        Ok(stop) => wire.respond(&id, json!({"stopReason": stop})),
                        Err(e) => wire.error(&id, -32603, &e),
                    }
                }));
            }
            ("session/cancel", _) => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                if let Some(c) = sessions.get(sid).and_then(|s| s.cancel.lock().unwrap().clone()) {
                    c.store(true, Ordering::SeqCst);
                }
            }
            (other, Some(id)) => wire.error(&id, -32601, &format!("unbekannte Methode '{other}'")),
            // Unbekannte Benachrichtigungen: nichts zu tun.
            (_, None) => {}
        }
    }
    for t in auftraege {
        let _ = t.join();
    }
}

/// Liest Zeilen vom Editor. Antworten auf eigene Anfragen gehen direkt an den
/// Wartenden; alles andere in die zurückgegebene Warteschlange. Am Ende der
/// Verbindung werden wartende Rückfragen gelöst (Sender fallen weg → `Err`),
/// sonst hinge ein Auftrag für immer an einer Freigabe.
fn spawn_reader(input: impl BufRead + Send + 'static, wire: Arc<Wire>) -> Receiver<Value> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in input.lines() {
            let Ok(line) = line else { break };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("method").is_none() {
                if let Some(id) = msg["id"].as_u64() {
                    if let Some(waiter) = wire.pending.lock().unwrap().remove(&id) {
                        let _ = waiter.send(msg);
                    }
                }
                continue;
            }
            if tx.send(msg).is_err() {
                break;
            }
        }
        wire.pending.lock().unwrap().clear();
    });
    rx
}

/// Der Freigabe-Callback einer Sitzung: fragt den Editor. „Immer erlauben"
/// gilt für das Programm (erstes Wort) in dieser Sitzung — wie `[i]mmer` in
/// der CLI.
fn permission_asker(wire: Arc<Wire>, sid: String) -> ApproveFn {
    let erlaubt: Arc<Mutex<HashSet<String>>> = Arc::default();
    let zaehler = Arc::new(AtomicU64::new(1));
    Arc::new(move |command: &str| {
        let programm = command.split_whitespace().next().unwrap_or("").to_string();
        if erlaubt.lock().unwrap().contains(&programm) {
            return true;
        }
        let n = zaehler.fetch_add(1, Ordering::SeqCst);
        let antwort = wire.request(
            "session/request_permission",
            json!({
                "sessionId": sid,
                "toolCall": {
                    "toolCallId": format!("freigabe-{n}"),
                    "title": format!("Shell: {command}"),
                    "kind": "execute",
                    "status": "pending",
                    "rawInput": {"command": command},
                },
                "options": [
                    {"optionId": "allow_once", "name": "Erlauben", "kind": "allow_once"},
                    {"optionId": "allow_always", "name": format!("Immer erlauben ({programm})"), "kind": "allow_always"},
                    {"optionId": "reject_once", "name": "Ablehnen", "kind": "reject_once"},
                ],
            }),
        );
        match antwort
            .ok()
            .as_ref()
            .and_then(|r| r["outcome"]["optionId"].as_str())
        {
            Some("allow_once") => true,
            Some("allow_always") => {
                erlaubt.lock().unwrap().insert(programm);
                true
            }
            // Abgelehnt, abgebrochen oder Verbindung weg: nicht ausführen.
            _ => false,
        }
    })
}

/// Führt einen Auftrag aus und streamt seine Ereignisse als `session/update`.
/// `Ok(stopReason)` oder `Err` mit der Fehlermeldung, wenn das Modell nicht
/// erreichbar war.
fn run_prompt(
    agent: &mut Agent,
    task: &str,
    sid: &str,
    cancel: &Cancel,
    strategy: &RunStrategy,
    wire: &Arc<Wire>,
) -> Result<&'static str, String> {
    let bus = EventBus::new();
    let events = bus.subscribe();
    let forward = {
        let (wire, sid) = (wire.clone(), sid.to_string());
        std::thread::spawn(move || forward_events(events, &wire, &sid))
    };
    let final_ = run_with_strategy(agent, task, &bus, -1, Some(cancel), strategy);
    // Der Bus muss weg, sonst endet der Weiterleiter nie, falls das Root-DONE
    // ausbleibt (siehe `EventBus::subscribe`).
    drop(bus);
    let letzter_fehler = forward.join().unwrap_or_default();
    match final_.as_str() {
        "(abgebrochen)" => Ok("cancelled"),
        "(max_steps erreicht)" => Ok("max_turn_requests"),
        "(keine Antwort)" => Err(letzter_fehler.unwrap_or_else(|| final_.clone())),
        _ => Ok("end_turn"),
    }
}

/// Übersetzt den Ereignisstrom bis zum Root-DONE. Gibt den letzten harten
/// Fehler (Modell/Netz) zurück — der wird zur Fehlermeldung des Auftrags.
fn forward_events(events: Receiver<AgentEvent>, wire: &Wire, sid: &str) -> Option<String> {
    forward_events_with_heartbeat(events, wire, sid, TOOL_HEARTBEAT)
}

/// [`forward_events`] mit einstellbarem Heartbeat-Abstand (Tests nehmen
/// Millisekunden statt [`TOOL_HEARTBEAT`]). Solange ein Tool-Aufruf offen ist,
/// bekommt er je Intervall ein `in_progress`-Update; ohne offenen Aufruf bleibt
/// die Leitung still.
///
/// Der Takt hängt an einer festen Frist, nicht am letzten Ereignis: Ereignisse,
/// die nicht auf die Leitung gehen (Text eines Sub-Agenten, `step`), kommen
/// während eines langen `task` laufend an — eine Frist, die sich an jedem davon
/// neu startete, liefe nie ab, und der Client sähe trotzdem nichts.
fn forward_events_with_heartbeat(
    events: Receiver<AgentEvent>,
    wire: &Wire,
    sid: &str,
    heartbeat: Duration,
) -> Option<String> {
    let update = |u: Value| wire.notify("session/update", json!({"sessionId": sid, "update": u}));
    let mut gestreamt = false;
    let mut letzter_fehler = None;
    // Aufrufe, die ein `tool_call` hatten, aber noch kein Ergebnis (Reihenfolge
    // des Eintreffens, damit die Heartbeats deterministisch sind). Ein Heartbeat
    // kann nie NACH dem Abschluss-Update seines Aufrufs liegen: beides schreibt
    // nur dieser Thread, und der Aufruf fliegt vor dem Abschluss hier raus.
    let mut offen: Vec<String> = Vec::new();
    let mut faellig = Instant::now() + heartbeat;
    loop {
        let empfangen = events.recv_timeout(faellig.saturating_duration_since(Instant::now()));
        // Auch nach einem Ereignis prüfen: bei dauerndem Nachschub kommt
        // `recv_timeout` nie mit `Timeout` zurück.
        if Instant::now() >= faellig {
            for id in &offen {
                update(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": "in_progress",
                }));
            }
            faellig = Instant::now() + heartbeat;
        }
        let ev = match empfangen {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let src = ev.source.as_str();
        match &ev.data {
            // Nur der Haupt-Agent spricht in den Chat; Helfer erscheinen über
            // ihre Tool-Aufrufe.
            EventData::TextDelta(t) if src.is_empty() => {
                gestreamt = true;
                update(
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}),
                );
            }
            EventData::Final(t) if src.is_empty() && !gestreamt && !t.is_empty() => {
                update(
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}),
                );
            }
            EventData::ToolCall { name, args } => {
                let id = tool_call_id(&ev);
                offen.push(id.clone());
                update(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": tool_title(src, name, args),
                    "kind": tool_kind(name),
                    "status": "in_progress",
                    "rawInput": args,
                }));
            }
            EventData::ToolResult { result, .. } => {
                let id = tool_call_id(&ev);
                offen.retain(|o| *o != id);
                update(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": truncate(result, MAX_TOOL_OUTPUT)}}],
                }));
            }
            EventData::Error {
                name: Some(_),
                error,
            } if !ev.call_id.is_empty() => {
                let id = tool_call_id(&ev);
                offen.retain(|o| *o != id);
                update(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": "failed",
                    "content": [{"type": "content", "content": {"type": "text", "text": error}}],
                }));
            }
            EventData::Error { name: None, error } if src.is_empty() => {
                letzter_fehler = Some(error.clone());
            }
            EventData::Plan(steps) => update(json!({
                "sessionUpdate": "plan",
                "entries": steps.iter().map(|s| json!({
                    "content": s.step,
                    "priority": "medium",
                    "status": match s.status.as_str() {
                        "done" => "completed",
                        "in_progress" => "in_progress",
                        _ => "pending",
                    },
                })).collect::<Vec<_>>(),
            })),
            _ if ev.etype == DONE && src.is_empty() => break,
            _ => {}
        }
    }
    letzter_fehler
}

/// Eindeutig über alle Agenten: Sub-Agenten vergeben ihre IDs unabhängig vom
/// Orchestrator, gleiche IDs aus zwei Quellen sind also möglich.
fn tool_call_id(ev: &AgentEvent) -> String {
    if ev.source.is_empty() {
        ev.call_id.clone()
    } else {
        format!("{}#{}", ev.source, ev.call_id)
    }
}

/// Eine Zeile für den Editor: Werkzeug und sein wichtigstes Argument.
fn tool_title(source: &str, name: &str, args: &Value) -> String {
    let haupt = ["command", "path", "pattern", "query", "name"]
        .iter()
        .find_map(|k| args[k].as_str())
        .map(|v| format!(": {}", truncate(v, 80)))
        .unwrap_or_default();
    let wer = source.split(':').next().filter(|s| !s.is_empty());
    match wer {
        Some(w) => format!("[{w}] {name}{haupt}"),
        None => format!("{name}{haupt}"),
    }
}

/// ACP-Kategorie eines Werkzeugs — der Editor wählt danach Symbol und Darstellung.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "read_pdf" | "list_files" | "git_status" | "git_diff" | "git_log"
        | "git_show" | "read_skill" | "list_skills" | "recall" => "read",
        "grep" | "glob_files" => "search",
        "write_file" | "edit_file" => "edit",
        "run_shell" => "execute",
        "update_plan" => "think",
        _ => "other",
    }
}

/// Der Auftragstext aus den Content-Blöcken des Editors. Eingebettete Dateien
/// kommen als markierter Block dazu, Verweise als Pfad — lesen kann der Agent
/// sie selbst.
fn prompt_text(prompt: &Value) -> String {
    let teile: Vec<String> = prompt
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b["type"].as_str()? {
                    "text" => b["text"].as_str().map(String::from),
                    "resource_link" => Some(format!("[Datei: {}]", b["uri"].as_str()?)),
                    "resource" => {
                        let r = &b["resource"];
                        Some(format!(
                            "Datei {}:\n```\n{}\n```",
                            r["uri"].as_str().unwrap_or("?"),
                            r["text"].as_str()?
                        ))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    teile.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Chunk;
    use crate::testing::FakeLlm;
    use crate::ToolRegistry;
    use std::io::{BufReader, Read};

    /// Eingabe, die der Test zeilenweise nachschiebt — so kann er auf eine
    /// Rückfrage des Agenten antworten.
    struct Kanal(Receiver<Vec<u8>>, Vec<u8>);
    impl Read for Kanal {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(bytes) => self.1 = bytes,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.1.len());
            buf[..n].copy_from_slice(&self.1[..n]);
            self.1.drain(..n);
            Ok(n)
        }
    }

    /// Ausgabe, die der Test zeilenweise mitliest.
    #[derive(Clone)]
    struct Mitschnitt(Sender<Value>, Arc<Mutex<Vec<u8>>>);
    impl Write for Mitschnitt {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut puffer = self.1.lock().unwrap();
            puffer.extend_from_slice(buf);
            while let Some(pos) = puffer.iter().position(|b| *b == b'\n') {
                let zeile: Vec<u8> = puffer.drain(..=pos).collect();
                let _ = self.0.send(serde_json::from_slice(&zeile).unwrap());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Ein ganzer Zug wie mit Zed: Sitzung anlegen, Auftrag, Tool-Aufruf mit
    /// Freigabe-Rückfrage, gestreamte Antwort, `stopReason`. Die Rückfrage
    /// beantwortet der Test — der Auftrag wartet so lange.
    #[test]
    fn zug_mit_freigabe_rueckfrage() {
        let factory: AgentFactory = Arc::new(|cwd: &str, approve: ApproveFn| {
            assert_eq!(cwd, "/projekt");
            let llm = Arc::new(FakeLlm::new(vec![
                vec![Chunk::tool(0, "c1", "shell", r#"{"command":"ls -la"}"#)],
                vec![Chunk::text("Fertig: "), Chunk::text("3 Dateien.")],
            ]));
            let mut reg = ToolRegistry::new();
            reg.add(
                "shell",
                "Shell.",
                json!({"type": "object"}),
                move |a: Value| {
                    let cmd = a["command"].as_str().unwrap_or("");
                    Ok(if approve(cmd) { "a b c" } else { "ABGELEHNT" }.to_string())
                },
            );
            Ok(Agent::new(llm, reg))
        });
        let (ein_tx, ein_rx) = channel::<Vec<u8>>();
        let (aus_tx, aus_rx) = channel::<Value>();
        let input = BufReader::new(Kanal(ein_rx, Vec::new()));
        let output = Mitschnitt(aus_tx, Arc::default());
        let server = std::thread::spawn(move || {
            serve(factory, RunStrategy::default(), input, output, "1.0")
        });
        let senden = |v: Value| ein_tx.send(format!("{v}\n").into_bytes()).unwrap();
        let naechste = || {
            aus_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
        };

        senden(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}),
        );
        assert_eq!(naechste()["result"]["protocolVersion"], 1);
        senden(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/projekt", "mcpServers": []}}),
        );
        let sid = naechste()["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        senden(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {
            "sessionId": sid, "prompt": [{"type": "text", "text": "Was liegt hier?"}]}}),
        );

        // Die Ankündigung des Tool-Aufrufs und die Rückfrage kommen aus zwei
        // Threads (Weiterleiter und Agent) — ihre Reihenfolge ist offen.
        let mut gesehen = Vec::new();
        let frage = loop {
            let m = naechste();
            if m["method"] == "session/request_permission" {
                break m;
            }
            gesehen.push(m);
        };
        assert_eq!(frage["params"]["toolCall"]["rawInput"]["command"], "ls -la");
        senden(
            json!({"jsonrpc": "2.0", "id": frage["id"], "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}}),
        );

        let mut text = String::new();
        let antwort = loop {
            let m = naechste();
            if m["id"] == 3 {
                break m;
            }
            gesehen.push(m.clone());
            let u = &m["params"]["update"];
            if u["sessionUpdate"] == "tool_call_update" {
                assert_eq!(u["content"][0]["content"]["text"], "a b c");
            }
            if u["sessionUpdate"] == "agent_message_chunk" {
                text.push_str(u["content"]["text"].as_str().unwrap());
            }
        };
        assert!(gesehen
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "tool_call"
                && m["params"]["update"]["title"] == "shell: ls -la"));
        assert_eq!(antwort["result"]["stopReason"], "end_turn");
        assert_eq!(text, "Fertig: 3 Dateien.");
        drop(ein_tx);
        server.join().unwrap();
    }

    /// Ein langer Tool-Aufruf hält die Leitung warm: nach `tool_call` ohne
    /// Ergebnis kommt je Intervall ein `in_progress`-Update mit derselben ID
    /// (inkl. Sub-Agent-Präfix); ohne offenen Aufruf und nach dem Ergebnis bleibt
    /// es still.
    #[test]
    fn heartbeat_nur_fuer_offene_tool_aufrufe() {
        let (aus_tx, aus_rx) = channel::<Value>();
        let wire = Arc::new(Wire {
            out: Mutex::new(Box::new(Mitschnitt(aus_tx, Arc::default()))),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });
        let bus = EventBus::new();
        let events = bus.subscribe();
        let forward = {
            let wire = wire.clone();
            std::thread::spawn(move || {
                forward_events_with_heartbeat(events, &wire, "s1", Duration::from_millis(40))
            })
        };
        let stille = |dauer| aus_rx.recv_timeout(Duration::from_millis(dauer)).is_err();
        let naechste = || aus_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        // Kein Tool offen: viele Intervalle lang nichts.
        assert!(stille(300), "Heartbeat ohne offenen Tool-Aufruf");

        bus.publish(
            AgentEvent::with_meta(
                crate::events::TOOL_CALL,
                EventData::ToolCall {
                    name: "grep".into(),
                    args: json!({"pattern": "x"}),
                },
                -1,
                "explorer:a".into(),
            )
            .with_call_id("c1"),
        );
        assert_eq!(naechste()["params"]["update"]["sessionUpdate"], "tool_call");
        for _ in 0..2 {
            let u = naechste()["params"]["update"].clone();
            assert_eq!(u["sessionUpdate"], "tool_call_update");
            assert_eq!(u["toolCallId"], "explorer:a#c1");
            assert_eq!(u["status"], "in_progress");
        }

        bus.publish(
            AgentEvent::with_meta(
                crate::events::TOOL_RESULT,
                EventData::ToolResult {
                    name: "grep".into(),
                    result: "ok".into(),
                },
                -1,
                "explorer:a".into(),
            )
            .with_call_id("c1"),
        );
        // Ein bereits unterwegs befindlicher Heartbeat darf noch vor dem
        // `completed` liegen, danach nicht mehr.
        loop {
            let u = naechste()["params"]["update"].clone();
            if u["status"] == "completed" {
                break;
            }
            assert_eq!(u["status"], "in_progress");
        }
        assert!(stille(300), "Heartbeat nach dem Tool-Ergebnis");

        bus.publish(AgentEvent::new(DONE, EventData::Done));
        drop(bus);
        assert_eq!(forward.join().unwrap(), None);
    }

    /// Ein Sub-Agent, der während eines offenen `task` nur Text streamt, erzeugt
    /// laufend Ereignisse, aber keine Zeile auf der Leitung. Der Heartbeat muss
    /// trotzdem kommen — sonst liefe der Client in seine Leerlauf-Frist.
    #[test]
    fn heartbeat_trotz_dauerndem_subagent_text() {
        let (aus_tx, aus_rx) = channel::<Value>();
        let wire = Arc::new(Wire {
            out: Mutex::new(Box::new(Mitschnitt(aus_tx, Arc::default()))),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });
        let bus = EventBus::new();
        let events = bus.subscribe();
        let forward = {
            let wire = wire.clone();
            std::thread::spawn(move || {
                forward_events_with_heartbeat(events, &wire, "s1", Duration::from_millis(40))
            })
        };
        let naechste = || aus_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        bus.publish(
            AgentEvent::new(
                crate::events::TOOL_CALL,
                EventData::ToolCall {
                    name: "task".into(),
                    args: json!({"prompt": "x"}),
                },
            )
            .with_call_id("t1"),
        );
        assert_eq!(naechste()["params"]["update"]["sessionUpdate"], "tool_call");

        // Text deutlich dichter als das Intervall, bis der Heartbeat da ist
        // (höchstens 10 s, damit ein Fehlschlag nicht ewig hängt).
        let fertig = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sprecher = {
            let (bus, fertig) = (bus.clone(), fertig.clone());
            std::thread::spawn(move || {
                let ende = Instant::now() + Duration::from_secs(10);
                while !fertig.load(Ordering::Relaxed) && Instant::now() < ende {
                    bus.publish(AgentEvent::with_meta(
                        crate::events::TEXT_DELTA,
                        EventData::TextDelta("…".into()),
                        -1,
                        "explorer:a".into(),
                    ));
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let u = naechste()["params"]["update"].clone();
        fertig.store(true, Ordering::Relaxed);
        sprecher.join().unwrap();
        assert_eq!(u["sessionUpdate"], "tool_call_update");
        assert_eq!(u["toolCallId"], "t1");
        assert_eq!(u["status"], "in_progress");

        bus.publish(AgentEvent::new(DONE, EventData::Done));
        drop(bus);
        assert_eq!(forward.join().unwrap(), None);
    }

    #[test]
    fn prompt_bloecke_werden_zum_auftrag() {
        let t = prompt_text(&json!([
            {"type": "text", "text": "Erkläre das."},
            {"type": "resource_link", "uri": "file:///a.rs", "name": "a.rs"},
            {"type": "resource", "resource": {"uri": "file:///b.rs", "text": "fn b() {}"}},
            {"type": "image", "data": "…"}
        ]));
        assert_eq!(
            t,
            "Erkläre das.\n\n[Datei: file:///a.rs]\n\nDatei file:///b.rs:\n```\nfn b() {}\n```"
        );
        assert_eq!(tool_kind("edit_file"), "edit");
        assert_eq!(
            tool_title("explorer:x", "grep", &json!({"pattern": "fn"})),
            "[explorer] grep: fn"
        );
    }
}
