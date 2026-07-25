//! Beispiel — Codemod-Schwarm: mehrere Agenten ändern GLEICHZEITIG echte
//! Dateien in einem gemeinsamen Workspace (netzfreies Regel-Modell, kein Netz).
//!
//!     cargo run --example codemod_swarm
//!
//! **Wann nimmt man dafür einen Schwarm?** Wenn dieselbe mechanische Änderung
//! über viele Dateien läuft und der Engpass die Anzahl der Dateien ist, nicht
//! die Schwierigkeit der Änderung. Der Koordinator sucht die Fundstellen,
//! **teilt sie disjunkt auf**, zwei Umbauer arbeiten parallel — und ein Prüfer
//! sagt am Ende, ob wirklich nichts übrig geblieben ist.
//!
//! Anders als die übrigen Beispiele ist dieser Schwarm nicht nur Gerede: die
//! Agenten bekommen die **echten Datei-Werkzeuge** von agentkit
//! ([`agentkit::coding::CodingTools`]) auf einem Wegwerf-Workspace unter
//! `temp_dir()`. Am Ende druckt das Beispiel den `grep`-Befund vorher/nachher.
//!
//! Drei Dinge, auf die es dabei ankommt:
//!
//! 1. **Ein Workspace für alle.** Der Schwarm gibt einem NICHT umsonst
//!    Isolation — parallel schreiben ist nur sicher, solange die Zuständigkeiten
//!    disjunkt sind. Genau das leistet die Aufteilung des Koordinators; wären
//!    es dieselben Dateien, hätte man ein Race, keinen Schwarm.
//! 2. **Werkzeuge je Rolle.** Koordinator und Prüfer bekommen nur `grep` und
//!    `read_file`, die Umbauer zusätzlich `edit_file`. Wer nicht schreiben
//!    können soll, bekommt das Werkzeug gar nicht erst.
//! 3. **Zwei Event-Ebenen.** Der Trace unten kommt vom `agentkit`-EventBus
//!    (Werkzeug-Aufrufe, `source` = Agent-ID); die Schwarm-Ebene
//!    (Zustellungen, Konsens) läuft parallel dazu über den `SwarmEventBus`.
//!
//! Echter Einsatz: dasselbe mit `openai_from_env()` statt `RegelLlm`, oder
//! direkt aus der Executable heraus, indem der Agent sich per `swarm`-Tool
//! einen solchen Schwarm baut (dort ist `read_only` der Default — Umbauer
//! brauchen ausdrücklich `"tools": "alle"`).

use agentkit::coding::CodingTools;
use agentkit::llm::{Chunk, ChunkStream, Llm, Message};
use agentkit::{Agent, EventBus, EventData, Strategy, ToolRegistry, TOOL_CALL};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Die zu migrierende „Codebasis" — je Datei genau EIN veralteter Aufruf
/// (`edit_file` verlangt eine eindeutige Fundstelle).
const DATEIEN: [(&str, &str); 4] = [
    (
        "src/api/kunden.rs",
        "pub fn kunden_laden(id: u64) -> Kunde {\n    let antwort = http_get_alt(&pfad(id));\n    Kunde::aus(antwort)\n}\n",
    ),
    (
        "src/api/rechnungen.rs",
        "pub fn rechnungen_holen(kunde: u64) -> Vec<Rechnung> {\n    let antwort = http_get_alt(&format!(\"/rechnungen/{kunde}\"));\n    Rechnung::liste(antwort)\n}\n",
    ),
    (
        "src/jobs/mahnlauf.rs",
        "pub fn mahnungen_faellig() -> Vec<Mahnung> {\n    let antwort = http_get_alt(\"/mahnungen?faellig=1\");\n    Mahnung::liste(antwort)\n}\n",
    ),
    (
        "src/jobs/export.rs",
        "pub fn export_anstossen(lauf: &str) -> Status {\n    let antwort = http_get_alt(&format!(\"/export/{lauf}\"));\n    Status::aus(antwort)\n}\n",
    ),
];

const ALT: &str = "http_get_alt(";
const NEU: &str = "http_get(";

fn main() {
    let workspace = std::env::temp_dir().join(format!("agentkit_codemod_{}", std::process::id()));
    for (pfad, inhalt) in DATEIEN {
        let ziel = workspace.join(pfad);
        std::fs::create_dir_all(ziel.parent().unwrap()).expect("Workspace anlegen");
        std::fs::write(&ziel, inhalt).expect("Datei schreiben");
    }
    let ws = workspace.to_str().expect("Workspace-Pfad").to_string();
    let lesend = CodingTools::new(&ws, false);
    println!("Workspace: {ws}");
    println!(
        "\nVorher:\n{}\n",
        lesend.grep(r"http_get_alt\(", ".", "**/*", 20).unwrap()
    );

    // Der gemeinsame agentkit-Bus: alle Agenten publizieren hier ihre
    // Werkzeug-Aufrufe, getaggt mit `source` = Agent-ID.
    let bus = EventBus::new();
    let events = bus.subscribe();
    let drucker = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            if let (&TOOL_CALL, EventData::ToolCall { name, args }) = (&event.etype, &event.data) {
                let ziel = args["path"]
                    .as_str()
                    .or_else(|| args["to"].as_str())
                    .unwrap_or("");
                println!("[{:<11}] 🔧 {name} {ziel}", event.source);
            }
        }
    });

    let handle = SwarmBuilder::new("codemod")
        .agent("koordinator", koordinator(&ws))
        .agent("worker_a", umbauer(&ws))
        .agent("worker_b", umbauer(&ws))
        .agent("pruefer", pruefer(&ws))
        // Stern: die Umbauer kennen einander nicht — sie müssen es auch nicht,
        // ihre Dateimengen sind disjunkt.
        .connect_bidirectional("koordinator", "worker_a")
        .connect_bidirectional("koordinator", "worker_b")
        .connect_bidirectional("koordinator", "pruefer")
        // Der Prüfer schlägt den Abschluss vor, der Koordinator bestätigt.
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_runtime(Duration::from_secs(30))
        .agent_bus(bus.clone())
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    handle
        .send_initial(
            "koordinator",
            format!(
                "Migriere die Codebasis von {ALT} auf {NEU}. Finde die Fundstellen, teile sie \
                     auf worker_a und worker_b auf und lass das Ergebnis vom pruefer verifizieren."
            )
            .as_str(),
        )
        .expect("Initialaufgabe");
    let result = handle.join();
    // Alle Actor-Threads sind zurück, also auch ihre Bus-Klone weg: sobald das
    // Original fällt, endet der Drucker-Thread von selbst.
    drop(bus);
    drucker.join().unwrap();

    println!(
        "\nNachher:\n{}\n",
        lesend.grep(r"http_get_alt\(", ".", "**/*", 20).unwrap()
    );
    println!("{}", lesend.read_file("src/api/kunden.rs").unwrap());
    match &result.reason {
        CompletionReason::Consensus { proposal, .. } => {
            println!("=== Abgeschlossen ===\n{}", proposal.content)
        }
        other => println!("=== Ende ohne Konsens: {other:?} ==="),
    }
    let mut turns: Vec<_> = result.turns.iter().collect();
    turns.sort();
    println!("Zustellungen: {}, Turns: {turns:?}", result.messages_sent);

    std::fs::remove_dir_all(&workspace).ok();
}

/// Der Koordinator: sucht die Fundstellen, teilt sie disjunkt auf, schickt am
/// Ende den Prüfer los und bestätigt dessen Abschluss-Vorschlag.
fn koordinator(ws: &str) -> Agent {
    let fertige = AtomicUsize::new(0);
    regel_agent(werkzeuge(ws, &["grep", "read_file"]), move |s| {
        match (kopf(&s.nachricht, "Art"), s.schritt) {
            ("task", 0) => vec![Chunk::tool(
                0,
                "g1",
                "grep",
                &json!({"pattern": r"http_get_alt\(", "glob": "**/*.rs"}).to_string(),
            )],
            // Aus dem grep-Ergebnis ("pfad:zeile: text") die Dateien ziehen,
            // halbieren, zwei Aufträge in EINEM Zug rausschicken.
            ("task", 1) => {
                let dateien: Vec<&str> = s
                    .ergebnis
                    .lines()
                    .filter_map(|zeile| zeile.split(':').next())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let (links, rechts) = dateien.split_at(dateien.len().div_ceil(2));
                vec![
                    auftrag(0, "s1", "worker_a", links),
                    auftrag(1, "s2", "worker_b", rechts),
                ]
            }
            // Erst wenn beide Umbauer fertig sind, lohnt die Verifikation.
            ("reply", 0) => {
                if fertige.fetch_add(1, Ordering::SeqCst) + 1 < 2 {
                    vec![Chunk::text("Teil erledigt, warte auf den zweiten Umbauer.")]
                } else {
                    vec![Chunk::tool(
                        0,
                        "s3",
                        "swarm_send",
                        &json!({
                            "to": "pruefer",
                            "kind": "request",
                            "content": format!("Beide Teile sind umgestellt. Verifiziere: es darf kein {ALT} mehr geben."),
                        })
                        .to_string(),
                    )]
                }
            }
            ("proposal", 0) => vec![Chunk::tool(
                0,
                "v1",
                "swarm_vote",
                &json!({
                    "proposal_id": kopf(&s.nachricht, "Nachricht-ID"),
                    "approve": true,
                    "comment": "Verifikation liegt vor",
                })
                .to_string(),
            )],
            _ => vec![Chunk::text("ok")],
        }
    })
}

/// Ein Umbauer: ändert genau die ihm zugeteilten Dateien und meldet zurück.
fn umbauer(ws: &str) -> Agent {
    regel_agent(werkzeuge(ws, &["read_file", "edit_file"]), |s| {
        let dateien = zugeteilte_dateien(&s.nachricht);
        match (kopf(&s.nachricht, "Art"), s.schritt) {
            // Ein `edit_file` je Datei — alle in einer Antwort.
            ("task", 0) => dateien
                .iter()
                .enumerate()
                .map(|(i, datei)| {
                    Chunk::tool(
                        i,
                        &format!("e{i}"),
                        "edit_file",
                        &json!({"path": datei, "old": ALT, "new": NEU}).to_string(),
                    )
                })
                .collect(),
            ("task", 1) => vec![Chunk::tool(
                0,
                "r1",
                "swarm_reply",
                &json!({
                    "content": format!("{} Dateien umgestellt: {}", dateien.len(), dateien.join(", ")),
                    "kind": "reply",
                })
                .to_string(),
            )],
            _ => vec![Chunk::text("ok")],
        }
    })
}

/// Der Prüfer: sucht selbst nach Restvorkommen und schlägt erst dann den
/// Abschluss vor — die Behauptung der Umbauer genügt ihm nicht.
fn pruefer(ws: &str) -> Agent {
    regel_agent(werkzeuge(ws, &["grep", "read_file"]), |s| {
        match (kopf(&s.nachricht, "Art"), s.schritt) {
            ("request", 0) => vec![Chunk::tool(
                0,
                "g2",
                "grep",
                &json!({"pattern": r"http_get_alt\(", "glob": "**/*.rs"}).to_string(),
            )],
            ("request", 1) if s.ergebnis.contains("keine Treffer") => vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                &json!({"proposal": format!("Migration vollständig: kein {ALT} mehr im Workspace.")})
                    .to_string(),
            )],
            // Restvorkommen zurückmelden statt abzuschließen.
            ("request", 1) => vec![Chunk::tool(
                0,
                "r2",
                "swarm_reply",
                &json!({"content": format!("Noch offen:\n{}", s.ergebnis), "kind": "critique"})
                    .to_string(),
            )],
            _ => vec![Chunk::text("ok")],
        }
    })
}

/// Ein Umbau-Auftrag an einen Umbauer; die Dateiliste steht als Kopfzeile im
/// Text, damit der Empfänger sie ohne Rateübung wieder herausliest.
fn auftrag(index: usize, id: &str, an: &str, dateien: &[&str]) -> Chunk {
    Chunk::tool(
        index,
        id,
        "swarm_send",
        &json!({
            "to": an,
            "kind": "task",
            "content": format!(
                "Ersetze in deinen Dateien jedes {ALT} durch {NEU} (ein edit_file je Datei).\n\
                 Dateien: {}",
                dateien.join(", ")
            ),
        })
        .to_string(),
    )
}

fn zugeteilte_dateien(nachricht: &str) -> Vec<&str> {
    kopf(nachricht, "Dateien")
        .split(", ")
        .filter(|d| !d.is_empty())
        .collect()
}

/// Die Werkzeug-Teilmenge einer Rolle — Sandbox auf den Workspace, keine
/// Rückfrage (`run_shell` ist ohnehin nicht dabei).
fn werkzeuge(workspace: &str, nur: &[&str]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    CodingTools::new(workspace, false).register(&mut registry, Some(nur));
    registry
}

// ---------------------------------------------------------------------------
// Ein netzfreies Modell, das wie ein echtes LLM nur den Verlauf sieht.
// ---------------------------------------------------------------------------

/// Was das Modell beim Aufruf sieht — der Verlauf auf das Wesentliche
/// eingedampft.
struct Sicht {
    /// Die gerade bearbeitete Schwarm-Nachricht (`[SWARM MESSAGE]`-Block).
    nachricht: String,
    /// Modell-Schritt innerhalb des Turns (0 = erster Aufruf, danach eine
    /// Runde je Werkzeug-Ergebnis).
    schritt: usize,
    /// Ergebnis des zuletzt gelaufenen Werkzeugs (leer im Schritt 0).
    ergebnis: String,
}

fn regel_agent(
    tools: ToolRegistry,
    regel: impl Fn(&Sicht) -> Vec<Chunk> + Send + Sync + 'static,
) -> Agent {
    Agent::builder(Arc::new(RegelLlm {
        regel: Box::new(regel),
    }))
    .tools(tools)
    .strategy(Strategy::Plain)
    .retry_backoff_ms(0)
    .build()
}

/// Die Regel eines Agenten: Sicht auf den Verlauf → Werkzeug-Aufruf.
type Regel = Box<dyn Fn(&Sicht) -> Vec<Chunk> + Send + Sync>;

struct RegelLlm {
    regel: Regel,
}

impl Llm for RegelLlm {
    fn complete(&self, _m: &[Value], _t: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _t: Option<&[Value]>) -> Result<ChunkStream, String> {
        Ok(Box::new((self.regel)(&sicht(messages)).into_iter().map(Ok)))
    }
}

fn sicht(messages: &[Value]) -> Sicht {
    fn rolle(m: &Value) -> &str {
        m["role"].as_str().unwrap_or("")
    }
    fn text(m: &Value) -> String {
        m["content"].as_str().unwrap_or("").to_string()
    }
    let start = messages
        .iter()
        .rposition(|m| rolle(m) == "user")
        .unwrap_or(0);
    let seither = &messages[start + 1..];
    Sicht {
        nachricht: text(&messages[start]),
        schritt: seither.iter().filter(|m| rolle(m) == "assistant").count(),
        ergebnis: seither
            .iter()
            .rev()
            .find(|m| rolle(m) == "tool")
            .map(text)
            .unwrap_or_default(),
    }
}

/// Liest ein Feld aus dem `[SWARM MESSAGE]`-Kopf ("Art", "Nachricht-ID") oder
/// eine gleich aufgebaute Zeile aus dem Nachrichtentext ("Dateien").
fn kopf<'a>(nachricht: &'a str, feld: &str) -> &'a str {
    nachricht
        .lines()
        .find_map(|zeile| zeile.strip_prefix(&format!("{feld}: ")))
        .unwrap_or("")
        .trim()
}
