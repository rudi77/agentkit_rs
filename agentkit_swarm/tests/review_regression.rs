//! Regressionstests zu den Befunden des Code-Reviews am Abschlusspfad und am
//! Lebenszyklus. Offline, mit gescriptetem `FakeLlm` (eine Instanz je Agent,
//! siehe `integration.rs`).

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy};
use agentkit_swarm::tools::MAX_PROPOSALS_PER_AGENT;
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use std::sync::Arc;
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
