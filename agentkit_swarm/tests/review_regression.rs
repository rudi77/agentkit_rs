//! Regressionstests zu den Befunden des Code-Reviews am Abschlusspfad und am
//! Lebenszyklus. Offline, mit gescriptetem `FakeLlm` (eine Instanz je Agent,
//! siehe `integration.rs`).

use agentkit::llm::{chunk_stream, Chunk, ChunkStream, Llm, Message};
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy};
use agentkit_swarm::tools::MAX_PROPOSALS_PER_AGENT;
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn test_agent(turns: Vec<Vec<Chunk>>) -> (Arc<FakeLlm>, Agent) {
    let llm = Arc::new(FakeLlm::new(turns));
    let agent = Agent::builder(llm.clone())
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();
    (llm, agent)
}

fn seen_text(llm: &FakeLlm) -> String {
    serde_json::to_string(&*llm.seen_messages.lock().unwrap()).unwrap()
}

/// Der Vorschlagende konnte sich selbst zustimmen: bei Quorum 1 stand der
/// Konsens, ohne dass irgendjemand den Vorschlag geprüft hatte. Jetzt weist
/// `swarm_vote` das mit einer erklärenden Meldung ab, und der Schwarm endet
/// NICHT per Konsens, solange kein anderes Mitglied zustimmt.
#[test]
fn eigene_stimme_oeffnet_das_quorum_nicht() {
    // Kickoff ist msg-1, der Vorschlag von `a` also msg-2.
    let (a_llm, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::tool(
            0,
            "v",
            "swarm_vote",
            r#"{"proposal_id":"msg-2","approve":true}"#,
        )],
        vec![Chunk::text("warte auf die anderen")],
    ]);
    let (_, b) = test_agent(vec![vec![Chunk::text("gesehen, noch keine Meinung")]]);

    let handle = SwarmBuilder::new("selbststimme")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .vote_window(Duration::ZERO)
        .max_idle(Duration::from_millis(400))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Schließt ab.").unwrap();
    let result = handle.join();

    assert_eq!(result.reason, CompletionReason::Idle, "{result:#?}");
    assert_eq!(result.proposals.len(), 1);
    assert!(result.proposals[0].approvals.is_empty());
    let gesehen = seen_text(&a_llm);
    assert!(gesehen.contains("eigener_vorschlag"), "{gesehen}");
    // Die Meldung nennt den Weg, der stattdessen funktioniert.
    assert!(gesehen.contains("anderen Mitglieds"), "{gesehen}");
}

/// Die Gegenprobe zum Test darüber (und zu Issue #35): im Zweier-Schwarm
/// reicht weiterhin EINE Stimme des anderen Mitglieds für den Konsens.
#[test]
fn stimme_des_anderen_schliesst_den_zweier_schwarm_ab() {
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::text("vorgeschlagen")],
    ]);
    let (_, b) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "v",
            "swarm_vote",
            r#"{"proposal_id":"msg-2","approve":true}"#,
        )],
        vec![Chunk::text("zugestimmt")],
    ]);

    let handle = SwarmBuilder::new("zweier")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_runtime(Duration::from_secs(10))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Schließt ab.").unwrap();
    let result = handle.join();

    match &result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            assert_eq!(proposal.from, "a");
            assert_eq!(*approvals, 1);
        }
        other => panic!("Konsens erwartet, war {other:?}"),
    }
    assert_eq!(result.proposals[0].approvals, vec!["b".to_string()]);
}

/// Vorschläge sind budgetfrei — ohne eigene Grenze konnte ein Mitglied
/// beliebig viele einreichen. Nach `MAX_PROPOSALS_PER_AGENT` kommt ein weicher
/// Fehler mit Handlungshinweis.
#[test]
fn vorschlaege_je_mitglied_sind_gedeckelt() {
    let zu_viele = MAX_PROPOSALS_PER_AGENT + 1;
    let mut a_turns: Vec<Vec<Chunk>> = (0..zu_viele)
        .map(|i| {
            vec![Chunk::tool(
                0,
                &format!("p{i}"),
                "swarm_propose",
                r#"{"proposal":"noch einer"}"#,
            )]
        })
        .collect();
    a_turns.push(vec![Chunk::text("genug")]);
    let (a_llm, a) = test_agent(a_turns);
    let (_, b) = test_agent(
        (0..zu_viele)
            .map(|_| vec![Chunk::text("gesehen")])
            .collect(),
    );

    let a = {
        let mut a = a;
        a.max_steps = zu_viele + 2;
        a
    };
    let handle = SwarmBuilder::new("deckel")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .mailbox_capacity(zu_viele + 4)
        .max_idle(Duration::from_millis(400))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Schlage vor.").unwrap();
    let result = handle.join();

    assert_eq!(
        result.proposals.len(),
        MAX_PROPOSALS_PER_AGENT,
        "{result:#?}"
    );
    let gesehen = seen_text(&a_llm);
    assert!(gesehen.contains("vorschlagslimit_erreicht"), "{gesehen}");
}

/// Ein ohne `join`/`stop` verworfenes Handle ließ seine Actor-Threads für
/// immer weiterlaufen — sie halten die Sender ihrer Nachbarn selbst.
#[test]
fn verworfenes_handle_beendet_die_actors() {
    let (_, a) = test_agent(vec![]);
    let (_, b) = test_agent(vec![]);
    let handle = SwarmBuilder::new("verworfen")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    drop(handle);

    let mut gestoppt = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while gestoppt.len() < 2 && Instant::now() < deadline {
        if let Ok(SwarmEvent::ActorStopped { agent }) =
            events.recv_timeout(Duration::from_millis(50))
        {
            gestoppt.push(agent);
        }
    }
    gestoppt.sort();
    assert_eq!(gestoppt, vec!["a".to_string(), "b".to_string()]);
}

/// Test-Modell für das Mitglied, dessen Turn beim Abschluss noch LÄUFT: es
/// meldet, dass sein Turn begonnen hat, wartet, bis der Vorschlag eingereicht
/// ist (vorher verbrauchten seine Proben Nachricht-IDs, und die ID des
/// Vorschlags wäre nicht mehr vorhersagbar), und sendet dann so lange Proben,
/// bis eine mit `schwarm_beendet` zurückkommt — erst dann endet der Turn.
///
/// Damit hängt der Test an EREIGNISSEN statt an Schlafzeiten: der Turn läuft
/// nachweislich vor dem Konsens an und endet nachweislich erst nach dem
/// vermerkten Abschluss, ganz gleich wie die Threads gerade drankommen.
struct ProbeLlm {
    gestartet: Mutex<Option<Sender<()>>>,
    vorschlag_da: Mutex<Option<Receiver<()>>>,
    aufrufe: AtomicUsize,
}

impl Llm for ProbeLlm {
    fn complete(&self, _messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("zusammengefasst".into()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        if let Some(tx) = self.gestartet.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.vorschlag_da.lock().unwrap().take() {
            rx.recv_timeout(Duration::from_secs(30))
                .map_err(|e| format!("der Vorschlag kam nie an: {e}"))?;
        }
        let beendet = messages
            .last()
            .is_some_and(|m| m.to_string().contains("schwarm_beendet"));
        if beendet {
            return Ok(chunk_stream(vec![Chunk::text("Turn zu Ende gebracht")]));
        }
        // Notbremse: ohne Abschluss soll der Test scheitern, nicht hängen.
        let n = self.aufrufe.fetch_add(1, Ordering::SeqCst);
        if n > 20_000 {
            return Err("der Schwarm wurde nie abgeschlossen".into());
        }
        std::thread::sleep(Duration::from_millis(1));
        Ok(chunk_stream(vec![Chunk::tool(
            0,
            &format!("probe-{n}"),
            "swarm_send",
            r#"{"to":"a","content":"probe"}"#,
        )]))
    }
}

/// Das Ausklingen nach dem Konsens ist für LAUFENDE Turns da. Was noch
/// ungelesen in einer Mailbox lag, begann vorher frische Turns — LLM-Aufrufe
/// nach dem Abschluss, deren Sends alle `schwarm_beendet` bekamen.
#[test]
fn ausklingen_beginnt_keine_neuen_turns() {
    // Kickoff msg-1, "eins" msg-2, "zwei" msg-3, Vorschlag msg-4. Die Reihenfolge
    // steht fest, weil `a` sie in EINEM Turn nacheinander abarbeitet.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"c","content":"eins"}"#,
        )],
        vec![Chunk::tool(
            0,
            "s2",
            "swarm_send",
            r#"{"to":"c","content":"zwei"}"#,
        )],
        vec![Chunk::tool(
            0,
            "p",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::text("verteilt und vorgeschlagen")],
    ]);

    // `b` bekommt den Vorschlag, gibt damit `c` die Proben frei und stimmt erst
    // zu, wenn der Turn von `c` nachweislich läuft.
    let (vorschlag_tx, vorschlag_rx) = channel::<()>();
    let (gestartet_tx, gestartet_rx) = channel::<()>();
    let gestartet_rx = Mutex::new(gestartet_rx);
    let mut b_tools = agentkit::ToolRegistry::new();
    b_tools.add(
        "warte_auf_c",
        "Testtool: blockiert, bis der Turn von c begonnen hat.",
        serde_json::json!({"type":"object","properties":{}}),
        move |_| {
            let _ = vorschlag_tx.send(());
            gestartet_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .map_err(|e| format!("c hat nie begonnen: {e}"))?;
            Ok("c arbeitet".into())
        },
    );
    let b_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "w", "warte_auf_c", "{}")],
        vec![Chunk::tool(
            0,
            "v",
            "swarm_vote",
            r#"{"proposal_id":"msg-4","approve":true}"#,
        )],
        vec![Chunk::text("zugestimmt")],
    ]));
    let b = Agent::builder(b_llm)
        .tools(b_tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    // Der Turn von `c` zu "eins" läuft über den Konsens hinweg; "zwei" (und die
    // Kopie des Vorschlags) liegen derweil ungelesen in seiner Mailbox.
    let mut c = Agent::builder(Arc::new(ProbeLlm {
        gestartet: Mutex::new(Some(gestartet_tx)),
        vorschlag_da: Mutex::new(Some(vorschlag_rx)),
        aufrufe: AtomicUsize::new(0),
    }))
    .strategy(Strategy::Plain)
    .retry_backoff_ms(0)
    .build();
    c.max_steps = 100_000;

    let handle = SwarmBuilder::new("ausklingen")
        .agent("a", a)
        .agent("b", b)
        .agent("c", c)
        .connect_bidirectional("a", "b")
        .connect_bidirectional("a", "c")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .vote_window(Duration::ZERO)
        // Die Proben dürfen weder Budget noch Kettenlänge erschöpfen.
        .max_messages(1_000_000)
        .max_runtime(Duration::from_secs(60))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle
        .send_initial("a", "Verteile und schlage vor.")
        .unwrap();
    let result = handle.join();

    assert!(
        matches!(result.reason, CompletionReason::Consensus { .. }),
        "{:?}",
        result.reason
    );
    // Der laufende Turn wurde abgewartet, kein weiterer begonnen.
    assert_eq!(result.turns.get("c"), Some(&1), "{:?}", result.turns);
    // Das Liegengebliebene ist nicht verschwunden, sondern abgelegt.
    assert!(
        result
            .dead_letters
            .iter()
            .any(|d| d.message.content == "zwei"),
        "'zwei' fehlt in den Dead Letters"
    );
}

/// Eine Ablehnung weckt den Urheber budgetfrei. Wiederholte Ablehnungen
/// DESSELBEN Vorschlags taten das jedes Mal — zwei Mitglieder konnten sich so
/// ohne Budget und ohne Hop-Grenze endlos gegenseitig wecken.
#[test]
fn nur_die_erste_ablehnung_weckt_den_urheber() {
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p",
            "swarm_propose",
            r#"{"proposal":"Wurf"}"#,
        )],
        vec![Chunk::text("vorgeschlagen")],
        vec![Chunk::text("Einwand gelesen")],
        vec![Chunk::text("darf nie laufen")],
    ]);
    let ablehnung = r#"{"proposal_id":"msg-2","approve":false,"comment":"Beleg fehlt"}"#;
    let (b_llm, b) = test_agent(vec![
        vec![Chunk::tool(0, "v1", "swarm_vote", ablehnung)],
        vec![Chunk::tool(0, "v2", "swarm_vote", ablehnung)],
        vec![Chunk::text("abgelehnt")],
    ]);

    let handle = SwarmBuilder::new("einwand")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_idle(Duration::from_millis(400))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Schlage vor.").unwrap();
    let result = handle.join();

    // Kickoff + EIN Einwand.
    assert_eq!(result.turns.get("a"), Some(&2), "{result:#?}");
    let gesehen = seen_text(&b_llm);
    assert!(gesehen.contains("zugestellt"), "{gesehen}");
    assert!(gesehen.contains("bereits_zugestellt"), "{gesehen}");
}
