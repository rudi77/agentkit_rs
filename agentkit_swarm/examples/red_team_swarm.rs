//! Beispiel — Freigabe-Tor mit Gegenspieler: ein Vorschlag fällt durch und
//! muss nachgebessert werden (netzfreies Regel-Modell, kein Netz, kein Key).
//!
//!     cargo run --example red_team_swarm
//!
//! **Wann nimmt man dafür einen Schwarm?** Wenn das Ergebnis nicht nur
//! *erzeugt*, sondern *verteidigt* werden muss — Sicherheitsfreigabe,
//! Architekturentscheid, Change Advisory Board. Der Trick ist nicht, dass
//! mehrere Modelle etwas sagen, sondern dass der Abschluss an ein **Quorum**
//! gebunden ist: Solange Angreifer und Freigabe nicht zustimmen, ist der
//! Schwarm nicht fertig. Das ist ein deterministisches Tor, das der Autor
//! nicht selbst öffnen kann — kein Orchestrator entscheidet „klingt gut".
//!
//! Ablauf (der Autor startet, danach steuert sich das selbst):
//!
//! 1. `autor` schlägt **Fassung 1** vor (`swarm_propose`).
//! 2. `angreifer` und `freigabe` stimmen mit **Nein** und schicken je eine
//!    Kritik zurück — in EINEM Zug, zwei Werkzeug-Aufrufe.
//! 3. Der Autor arbeitet beide Kritiken ein und schlägt **Fassung 2** vor.
//! 4. Diesmal stimmen beide zu → Quorum 2 erreicht, Schwarm fertig.
//!
//! Zwei Dinge, die ein Orchestrator mit Sub-Agenten so nicht kann: die Prüfer
//! **erinnern sich** an Fassung 1, wenn Fassung 2 kommt (Agent-Memory lebt über
//! alle Nachrichten), und sie reden **direkt** miteinander und mit dem Autor,
//! statt über einen Vermittler.
//!
//! Am Ende zählt das Beispiel Vorschläge und Stimmen aus — man sieht schwarz
//! auf weiß, dass Fassung 1 durchgefallen ist.

use agentkit::llm::{Chunk, ChunkStream, Llm, Message};
use agentkit::{Agent, Strategy};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const FASSUNG_1: &str = "Fassung 1: Access-Tokens werden vor dem Loggen gehasht.";
const FASSUNG_2: &str = "Fassung 2: Tokens werden gehasht geloggt, Refresh-Tokens aus \
     Fehlermeldungen redigiert, und die Signaturschlüssel rotieren alle 24 h.";

fn main() {
    let handle = SwarmBuilder::new("freigabe")
        .agent("autor", autor())
        .agent(
            "angreifer",
            pruefer(
                "Fassung 1 reicht nicht: der Refresh-Token steht weiterhin im Klartext in der \
                 Fehlermeldung des Token-Endpunkts.",
            ),
        )
        .agent(
            "freigabe",
            pruefer(
                "Ohne Rotation der Signaturschlüssel bekommt das keine Freigabe — ein \
                 abgeflossener Schlüssel bleibt sonst unbegrenzt gültig.",
            ),
        )
        // Vollvermascht: die Prüfer könnten sich auch untereinander abstimmen.
        .connect_bidirectional("autor", "angreifer")
        .connect_bidirectional("autor", "freigabe")
        .connect_bidirectional("angreifer", "freigabe")
        // Beide Prüfer müssen zustimmen — der Autor stimmt gar nicht mit ab.
        .completion(CompletionPolicy::Consensus {
            required_approvals: 2,
        })
        .max_runtime(Duration::from_secs(30))
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    let events = handle.events();
    let consumer = std::thread::spawn(move || {
        let (mut vorschlaege, mut ja, mut nein) = (0usize, 0usize, 0usize);
        while let Ok(event) = events.recv() {
            match &event {
                SwarmEvent::ProposalCreated { message } => {
                    vorschlaege += 1;
                    println!("[vorschlag] {} ({})", message.content, message.id);
                }
                SwarmEvent::VoteSubmitted { message } => {
                    let zustimmung = serde_json::from_str::<Value>(&message.content)
                        .ok()
                        .and_then(|v| v["zustimmung"].as_bool())
                        .unwrap_or(false);
                    if zustimmung {
                        ja += 1;
                    } else {
                        nein += 1;
                    }
                    println!(
                        "[stimme   ] {:<10} {} zu {}",
                        message.from,
                        if zustimmung { "JA  " } else { "NEIN" },
                        message.correlation_id.clone().unwrap_or_default()
                    );
                }
                SwarmEvent::MessageQueued { message } if message.kind.as_str() == "critique" => {
                    println!("[kritik   ] {:<10} {}", message.from, message.content);
                }
                SwarmEvent::SwarmCompleted { .. } => break,
                _ => {}
            }
        }
        (vorschlaege, ja, nein)
    });

    handle
        .send_initial(
            "autor",
            "Härte den Umgang mit Tokens ab und hol dir die Freigabe von angreifer und freigabe.",
        )
        .expect("Initialaufgabe");
    let result = handle.join();
    let (vorschlaege, ja, nein) = consumer.join().unwrap();

    println!();
    match &result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => println!(
            "=== Freigegeben ({approvals} Zustimmungen) ===\n{}\n",
            proposal.content
        ),
        other => println!("=== Ende ohne Freigabe: {other:?} ===\n"),
    }
    println!(
        "{vorschlaege} Vorschläge, {ja} Zustimmungen, {nein} Ablehnungen — \
         {} Vorschlag ist am Quorum gescheitert.",
        vorschlaege - 1
    );
    println!("Zustellungen: {}", result.messages_sent);
}

/// Der Autor: schlägt vor, sammelt Kritik, schlägt nachgebessert erneut vor.
fn autor() -> Agent {
    let kritiken = AtomicUsize::new(0);
    regel_agent(move |nachricht| match kopf(nachricht, "Art") {
        "task" => vec![vorschlag(FASSUNG_1)],
        // Erst wenn BEIDE Kritiken da sind, lohnt die nächste Fassung.
        "critique" => {
            if kritiken.fetch_add(1, Ordering::SeqCst) + 1 < 2 {
                vec![Chunk::text(
                    "Kritik notiert, warte auf die zweite Rückmeldung.",
                )]
            } else {
                vec![vorschlag(FASSUNG_2)]
            }
        }
        _ => vec![Chunk::text("ok")],
    })
}

/// Ein Prüfer: lehnt die erste Fassung mit Begründung ab, gibt die zweite frei.
/// Der `swarm_vote`-Aufruf und die Kritik gehen in EINEM Zug raus (zwei
/// Werkzeuge in einer Antwort) — die Ablehnung ist die Stimme, die Kritik der
/// Weg zur nächsten Fassung.
fn pruefer(kritik: &'static str) -> Agent {
    regel_agent(move |nachricht| {
        if kopf(nachricht, "Art") != "proposal" {
            return vec![Chunk::text("ok")];
        }
        let id = kopf(nachricht, "Nachricht-ID");
        if nachricht.contains("Fassung 2") {
            vec![stimme(id, true, "Einwand eingearbeitet.")]
        } else {
            vec![
                stimme(id, false, kritik),
                Chunk::tool(
                    1,
                    "k1",
                    "swarm_reply",
                    &json!({"content": kritik, "kind": "critique"}).to_string(),
                ),
            ]
        }
    })
}

fn vorschlag(text: &str) -> Chunk {
    Chunk::tool(
        0,
        "p1",
        "swarm_propose",
        &json!({ "proposal": text }).to_string(),
    )
}

fn stimme(vorschlag_id: &str, zustimmung: bool, begruendung: &str) -> Chunk {
    Chunk::tool(
        0,
        "v1",
        "swarm_vote",
        &json!({
            "proposal_id": vorschlag_id,
            "approve": zustimmung,
            "comment": begruendung,
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Ein netzfreies Modell, das wie ein echtes LLM nur den Verlauf sieht.
// ---------------------------------------------------------------------------

/// Baut einen Agenten, dessen „Modell" eine Regel über der eingegangenen
/// Schwarm-Nachricht ist. Nach der ersten Werkzeug-Runde ist der Zug beendet.
fn regel_agent(regel: impl Fn(&str) -> Vec<Chunk> + Send + Sync + 'static) -> Agent {
    Agent::builder(Arc::new(RegelLlm {
        regel: Box::new(regel),
    }))
    .strategy(Strategy::Plain)
    .retry_backoff_ms(0)
    .build()
}

/// Die Regel eines Agenten: eingegangene Schwarm-Nachricht → Werkzeug-Aufruf.
type Regel = Box<dyn Fn(&str) -> Vec<Chunk> + Send + Sync>;

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
        let (nachricht, schritt) = eingang(messages);
        let chunks = if schritt == 0 {
            (self.regel)(&nachricht)
        } else {
            vec![Chunk::text("erledigt")]
        };
        Ok(Box::new(chunks.into_iter().map(Ok)))
    }
}

/// Die gerade bearbeitete Schwarm-Nachricht und der Schritt innerhalb des Turns
/// (0 = erster Modell-Aufruf, danach eine Runde je Werkzeug-Ergebnis).
fn eingang(messages: &[Value]) -> (String, usize) {
    fn rolle(m: &Value) -> &str {
        m["role"].as_str().unwrap_or("")
    }
    let start = messages
        .iter()
        .rposition(|m| rolle(m) == "user")
        .unwrap_or(0);
    let schritt = messages[start + 1..]
        .iter()
        .filter(|m| rolle(m) == "assistant")
        .count();
    let text = messages[start]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (text, schritt)
}

/// Liest ein Feld aus dem `[SWARM MESSAGE]`-Kopf ("Art", "Von", "Nachricht-ID").
fn kopf<'a>(nachricht: &'a str, feld: &str) -> &'a str {
    nachricht
        .lines()
        .find_map(|zeile| zeile.strip_prefix(&format!("{feld}: ")))
        .unwrap_or("")
        .trim()
}
