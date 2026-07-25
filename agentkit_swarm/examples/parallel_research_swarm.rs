//! Beispiel — Recherche-Schwarm: drei Spezialisten arbeiten ECHT gleichzeitig
//! (netzfreies Regel-Modell, kein Netz, kein Key).
//!
//!     cargo run --example parallel_research_swarm
//!
//! **Wann nimmt man dafür einen Schwarm?** Wenn eine Frage in mehrere
//! unabhängige Teilfragen zerfällt, deren Beantwortung Zeit kostet (Doku lesen,
//! Codebasis durchsuchen, Betriebsdaten prüfen) — und niemand auf das Ergebnis
//! der anderen warten muss. Ein einzelner Agent arbeitet die drei Bereiche
//! nacheinander ab; ein Schwarm legt sie auf drei Threads. Jeder Actor ist ein
//! eigener OS-Thread, die Nebenläufigkeit ist also keine Metapher.
//!
//! Das Beispiel misst den Unterschied und druckt ihn am Ende aus: die
//! Spezialisten „arbeiten" hier per `sleep` (300/450/600 ms), also 1350 ms
//! Arbeit insgesamt — die Wanduhr sollte ungefähr beim langsamsten Beitrag
//! landen, nicht bei der Summe. Der Live-Trace zeigt dieselbe Aussage: die
//! drei Anfragen gehen im selben Millisekundenbereich raus, die Antworten
//! kommen gestaffelt zurück.
//!
//! Topologie: Stern. Die Spezialisten kennen einander nicht — sie besitzen
//! schlicht keine `AgentActorRef` aufeinander. Abschluss: der Koordinator
//! schlägt das Ergebnis vor, alle drei stimmen zu (Quorum 3).
//!
//! Echter Einsatz: dieselbe Struktur mit einem echten Modell entsteht durch
//! `Agent::builder(openai_from_env()?)` statt des `RegelLlm` unten — oder zur
//! Laufzeit über das `swarm`-Tool (`topologie: "stern"`, siehe
//! `dynamic_swarm.rs`).

use agentkit::llm::{Chunk, ChunkStream, Llm, Message};
use agentkit::{Agent, Strategy};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Die Spezialisten: ID, simulierte Arbeitszeit, ihr Befund.
const SPEZIALISTEN: [(&str, u64, &str); 3] = [
    (
        "doku",
        300,
        "Doku: Die neue Retry-API garantiert Idempotenz nur mit Idempotency-Key im Header.",
    ),
    (
        "codebase",
        450,
        "Codebasis: 14 Aufrufstellen, davon 3 ohne Retry-Schutz (mcp/client.rs).",
    ),
    (
        "betrieb",
        600,
        "Betrieb: 2 % der Aufrufe laufen in Timeouts; das Kontingent trägt die Umstellung.",
    ),
];

fn main() {
    let arbeit: u64 = SPEZIALISTEN.iter().map(|(_, ms, _)| ms).sum();
    let langsamster = SPEZIALISTEN.iter().map(|(_, ms, _)| *ms).max().unwrap();

    let mut builder = SwarmBuilder::new("recherche").agent("koordinator", koordinator());
    for (id, dauer, befund) in SPEZIALISTEN {
        builder = builder
            .agent(id, spezialist(dauer, befund))
            .connect_bidirectional("koordinator", id);
    }
    let handle = builder
        .completion(CompletionPolicy::Consensus {
            required_approvals: SPEZIALISTEN.len(),
        })
        // Sicherheitsnetz: ohne Laufzeitgrenze wartet `join()` auf den Konsens
        // — bei einem echten Modell auch dann, wenn er nie zustande kommt.
        .max_runtime(Duration::from_secs(30))
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    // Trace mit Zeitstempeln — daran sieht man die Nebenläufigkeit direkt.
    let events = handle.events();
    let start = Instant::now();
    let consumer = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                // Ein Broadcast erscheint einmal je Zustellung — gleiche
                // Nachricht-ID, drei Empfänger, ein Budget-Posten pro Kopie.
                SwarmEvent::MessageQueued { message } => println!(
                    "[{:>5} ms] {:<12} → {:<12} {:<12} {}",
                    start.elapsed().as_millis(),
                    message.from,
                    match &message.to {
                        agentkit_swarm::Recipient::Agent(id) => id.clone(),
                        agentkit_swarm::Recipient::Broadcast => "(alle)".to_string(),
                    },
                    message.kind.as_str(),
                    message.id,
                ),
                SwarmEvent::SwarmCompleted { .. } => break,
                _ => {}
            }
        }
    });

    handle
        .send_initial(
            "koordinator",
            "Sollen wir auf die neue Retry-API umstellen? Hol dir die Befunde deiner Spezialisten.",
        )
        .expect("Initialaufgabe");
    let result = handle.join();
    let laufzeit = start.elapsed().as_millis();
    consumer.join().unwrap();

    println!();
    match &result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => println!(
            "=== Konsens ({approvals} Zustimmungen) ===\n{}\n",
            proposal.content
        ),
        other => println!("=== Ende ohne Konsens: {other:?} ===\n"),
    }
    println!("Arbeit der Spezialisten zusammen: {arbeit} ms (so lange bräuchte ein Solo-Agent)");
    println!(
        "Langsamster Einzelbeitrag:        {langsamster} ms (die untere Schranke des Schwarms)"
    );
    println!("Tatsächliche Laufzeit:            {laufzeit} ms");
    println!("Zustellungen: {}, Turns: {:?}", result.messages_sent, {
        let mut turns: Vec<_> = result.turns.iter().collect();
        turns.sort();
        turns
    });
}

/// Der Koordinator: verteilt die Frage per Broadcast, sammelt drei Befunde und
/// schlägt danach den Abschluss vor.
fn koordinator() -> Agent {
    let eingegangen = AtomicUsize::new(0);
    regel_agent(move |nachricht| match kopf(nachricht, "Art") {
        "task" => vec![Chunk::tool(
            0,
            "b1",
            "swarm_broadcast",
            r#"{"content":"Prüft für eure Domäne: Sollen wir auf die neue Retry-API umstellen? Antwortet mit EINEM Befund.","kind":"request"}"#,
        )],
        // Erst wenn alle drei geantwortet haben, steht das Gesamtbild.
        // (Die Spezialisten antworten mit `kind: observation`.)
        "observation" | "reply" => {
            if eingegangen.fetch_add(1, Ordering::SeqCst) + 1 < SPEZIALISTEN.len() {
                vec![Chunk::text("Befund notiert, warte auf die übrigen.")]
            } else {
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Umstellen — aber in zwei Schritten: erst Idempotency-Key ergänzen, dann die 3 ungeschützten Aufrufstellen in mcp/client.rs nachziehen."}"#,
                )]
            }
        }
        _ => vec![Chunk::text("ok")],
    })
}

/// Ein Spezialist: arbeitet auf Anfrage `dauer` Millisekunden und antwortet mit
/// seinem Befund; einem Abschluss-Vorschlag stimmt er zu.
fn spezialist(dauer: u64, befund: &'static str) -> Agent {
    regel_agent(move |nachricht| match kopf(nachricht, "Art") {
        "request" => {
            // Steht hier für echte Arbeit (Doku lesen, grep, Metriken abfragen).
            std::thread::sleep(Duration::from_millis(dauer));
            vec![Chunk::tool(
                0,
                "r1",
                "swarm_reply",
                &json!({"content": befund, "kind": "observation"}).to_string(),
            )]
        }
        // Die Vorschlag-ID steht im Kopf der eingegangenen Nachricht — nichts
        // ist hier fest verdrahtet, ein echtes Modell liest sie genauso ab.
        "proposal" => vec![Chunk::tool(
            0,
            "v1",
            "swarm_vote",
            &json!({
                "proposal_id": kopf(nachricht, "Nachricht-ID"),
                "approve": true,
                "comment": "durch meinen Befund gedeckt",
            })
            .to_string(),
        )],
        _ => vec![Chunk::text("ok")],
    })
}

// ---------------------------------------------------------------------------
// Ein netzfreies Modell, das wie ein echtes LLM nur den Verlauf sieht.
// ---------------------------------------------------------------------------

/// Baut einen Agenten, dessen „Modell" eine Regel über der eingegangenen
/// Schwarm-Nachricht ist. Nach dem ersten Werkzeug ist der Zug beendet — genau
/// ein Werkzeug je Nachricht.
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
        Ok(Box::new(chunks.into_iter()))
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
