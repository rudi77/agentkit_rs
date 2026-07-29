//! Tests ohne Netz — Nachrichten-Routing, Topologie, Konsens und
//! Actor-Lifecycle, durchgängig mit gescriptetem `FakeLlm` (eine Instanz pro
//! Schwarm-Agent: der Turn-Cursor des FakeLlm ist global, geteilte Instanzen
//! wären bei nebenläufigen Agenten nichtdeterministisch).
//!
//! Scripting-Konvention: ein Agent-Run konsumiert EINEN FakeLlm-Turn pro
//! Loop-Schritt — ein Turn mit Tool-Aufruf braucht danach also einen weiteren
//! Turn für die finale Antwort. Die Skripte sind die konkatenierte Turn-Folge
//! über alle Nachrichten, die der Agent empfangen wird.

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy, ToolRegistry};
use agentkit_swarm::{
    CompletionPolicy, CompletionReason, DeliveryResult, MessageKind, Recipient, SwarmBuilder,
    SwarmError, SwarmEvent, SwarmMessage,
};
use serde_json::json;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ Helfer

fn test_agent(turns: Vec<Vec<Chunk>>) -> (Arc<FakeLlm>, Agent) {
    let llm = Arc::new(FakeLlm::new(turns));
    let agent = Agent::builder(llm.clone())
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();
    (llm, agent)
}

/// Sammelt Events, bis `until` zutrifft (Panic bei Timeout) — gibt alle
/// gesehenen Events zurück.
fn collect_until(
    rx: &Receiver<SwarmEvent>,
    timeout: Duration,
    mut until: impl FnMut(&SwarmEvent) -> bool,
) -> Vec<SwarmEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
            let done = until(&event);
            seen.push(event);
            if done {
                return seen;
            }
        }
    }
    panic!("erwartetes Event nicht eingetroffen; gesehen: {seen:#?}");
}

fn turn_completed(event: &SwarmEvent, agent: &str) -> bool {
    matches!(event, SwarmEvent::TurnCompleted { agent: a, .. } if a == agent)
}

/// Alle Prompts, die ein FakeLlm gesehen hat, als ein String (für Assertions
/// auf das gerenderte `[SWARM MESSAGE]`-Format).
fn seen_text(llm: &FakeLlm) -> String {
    serde_json::to_string(&*llm.seen_messages.lock().unwrap()).unwrap()
}

// ---------------------------------------------------------- Nachrichtenmodell

#[test]
fn swarm_message_serde_roundtrip() {
    let msg = SwarmMessage {
        id: "msg-1".into(),
        swarm_id: "test".into(),
        from: "a".into(),
        to: Recipient::Agent("b".into()),
        kind: MessageKind::Observation,
        content: "Der Deadlock tritt nur bei parallelen MCP-Calls auf.".into(),
        reply_to: Some("msg-0".into()),
        correlation_id: Some("race-17".into()),
        created_at: 1234,
        hop_count: 3,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains("\"observation\""),
        "MessageKind snake_case: {json}"
    );
    let back: SwarmMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn render_prompt_contains_sender_kind_and_content() {
    let msg = SwarmMessage {
        id: "msg-42".into(),
        swarm_id: "test".into(),
        from: "tester".into(),
        to: Recipient::Agent("developer".into()),
        kind: MessageKind::Observation,
        content: "Zweite Session wartet mit gehaltenem stdin-Lock.".into(),
        reply_to: Some("msg-31".into()),
        correlation_id: None,
        created_at: 0,
        hop_count: 1,
    };
    let prompt = msg.render_prompt();
    assert!(prompt.starts_with("[SWARM MESSAGE]"));
    assert!(prompt.contains("Nachricht-ID: msg-42"));
    assert!(prompt.contains("Von: tester"));
    assert!(prompt.contains("Art: observation"));
    assert!(prompt.contains("Antwort auf: msg-31"));
    assert!(prompt.contains("stdin-Lock"));
    assert!(prompt.contains("swarm_reply"));
}

/// Der Kickoff der Laufzeit hat keinen Absender, dem man antworten könnte —
/// `runtime` steht in keiner PeerDirectory. Der pauschale Reply-Hinweis hätte
/// das Mitglied ausgerechnet bei seiner ersten Nachricht in den einen Sendepfad
/// geführt, der hier nicht funktionieren kann.
#[test]
fn render_prompt_der_initialaufgabe_verweist_nicht_auf_reply() {
    let msg = SwarmMessage {
        id: "msg-1".into(),
        swarm_id: "test".into(),
        from: "runtime".into(),
        to: Recipient::Agent("architekt".into()),
        kind: MessageKind::Task,
        content: "Analysiere das Repo.".into(),
        reply_to: None,
        correlation_id: None,
        created_at: 0,
        hop_count: 0,
    };
    let prompt = msg.render_prompt();
    assert!(prompt.contains("Analysiere das Repo."));
    assert!(!prompt.contains("swarm_reply"), "{prompt}");
    assert!(prompt.contains("swarm_broadcast"), "{prompt}");
    assert!(prompt.contains("swarm_propose"), "{prompt}");
}

// ------------------------------------------------------------------ Builder

#[test]
fn builder_rejects_empty_duplicate_and_invalid_topology() {
    assert_eq!(
        SwarmBuilder::new("s").build().err(),
        Some(SwarmError::EmptySwarm)
    );

    let (_, a1) = test_agent(vec![]);
    let (_, a2) = test_agent(vec![]);
    assert_eq!(
        SwarmBuilder::new("s")
            .agent("a", a1)
            .agent("a", a2)
            .build()
            .err(),
        Some(SwarmError::DuplicateAgent("a".into()))
    );

    let (_, a) = test_agent(vec![]);
    let err = SwarmBuilder::new("s")
        .agent("a", a)
        .connect_bidirectional("a", "geist")
        .build()
        .err();
    assert!(
        matches!(err, Some(SwarmError::InvalidTopology(_))),
        "{err:?}"
    );
}

// ------------------------------------------------------------ Actor-Lifecycle

#[test]
fn single_agent_turn_then_max_runtime_ends_swarm() {
    let (_, a) = test_agent(vec![vec![Chunk::text("hallo")]]);
    let handle = SwarmBuilder::new("solo")
        .agent("a", a)
        .max_runtime(Duration::from_millis(400))
        .build()
        .unwrap()
        .start()
        .unwrap();

    assert_eq!(
        handle.send_initial("geist", "x").err(),
        Some(SwarmError::UnknownAgent("geist".into()))
    );
    handle.send_initial("a", "Sag hallo.").unwrap();

    let result = handle.join();
    assert_eq!(result.reason, CompletionReason::MaxRuntimeReached);
    assert_eq!(result.turns.get("a"), Some(&1));
    assert_eq!(result.messages_sent, 1); // nur die Initialaufgabe
}

/// `max_runtime` (und der Stop-Knopf) dürfen nicht davon abhängen, dass der
/// Event-Strom kurz stillsteht: `recv_timeout` läuft nur ab, wenn 100 ms lang
/// KEIN Event kommt. Wurden die Schranken nur im Timeout-Zweig geprüft, lief ein
/// Schwarm unter Dauerverkehr unbegrenzt weiter — hier erzeugt `a` genau solchen
/// Verkehr, indem es fortlaufend (budgetfrei) Vorschläge einreicht, denen `b` nie
/// zustimmt.
#[test]
fn max_runtime_greift_auch_unter_dauerhaftem_event_strom() {
    let propose = |i: usize| {
        vec![Chunk::tool(
            0,
            &format!("p{i}"),
            "swarm_propose",
            r#"{"proposal":"jetzt?"}"#,
        )]
    };
    let (_, a) = test_agent((0..500).map(propose).collect());
    // b stimmt nie zu -> kein Konsens, nur die Laufzeit kann beenden.
    let (_, b) = test_agent(vec![vec![Chunk::text("nein")]]);

    let handle = SwarmBuilder::new("flut")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_runtime(Duration::from_millis(500))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Schlag etwas vor.").unwrap();

    let start = Instant::now();
    let result = handle.join();
    assert_eq!(result.reason, CompletionReason::MaxRuntimeReached);
    // Großzügig: es geht um „terminiert überhaupt", nicht um Präzision.
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "Monitor hing am Event-Strom: {:?}",
        start.elapsed()
    );
}

#[test]
fn failed_turn_keeps_actor_alive() {
    // max_steps = 1 und ein Tool-Aufruf pro Turn -> jeder Run endet mit dem
    // Sentinel "(max_steps erreicht)" (success = false), der Actor lebt weiter.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "t1", "swarm_peers", "{}")],
        vec![Chunk::tool(0, "t2", "swarm_peers", "{}")],
    ]));
    let agent = Agent::builder(llm.clone())
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .max_steps(1)
        .build();
    let handle = SwarmBuilder::new("solo")
        .agent("a", agent)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();

    handle.send_initial("a", "eins").unwrap();
    let seen = collect_until(&events, Duration::from_secs(5), |e| turn_completed(e, "a"));
    handle.send_initial("a", "zwei").unwrap();
    let seen2 = collect_until(&events, Duration::from_secs(5), |e| turn_completed(e, "a"));

    for events in [&seen, &seen2] {
        assert!(events
            .iter()
            .any(|e| matches!(e, SwarmEvent::TurnCompleted { success: false, .. })));
    }
    let result = handle.stop();
    assert_eq!(result.reason, CompletionReason::Stopped);
    assert_eq!(
        result.turns.get("a"),
        Some(&2),
        "Actor hat beide Turns verarbeitet"
    );
}

#[test]
fn actor_panic_stops_swarm() {
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"b","content":"lauf"}"#,
        )],
        vec![Chunk::text("gesendet")],
    ]);
    let mut b_tools = ToolRegistry::new();
    b_tools.add(
        "explodiere",
        "Testtool: panict.",
        json!({"type":"object","properties":{}}),
        |_| panic!("kaboom"),
    );
    let b_llm = Arc::new(FakeLlm::new(vec![vec![Chunk::tool(
        0,
        "x1",
        "explodiere",
        "{}",
    )]]));
    let b = Agent::builder(b_llm)
        .tools(b_tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("crash")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Bring b zum Absturz.").unwrap();

    let result = handle.join();
    match result.reason {
        CompletionReason::ActorFailure { agent, error } => {
            assert_eq!(agent, "b");
            assert!(error.contains("kaboom"), "Panic-Text erwartet: {error}");
        }
        other => panic!("ActorFailure erwartet, war {other:?}"),
    }
}

#[test]
fn shutdown_drains_mailbox_to_dead_letters() {
    // b hängt im ersten Turn in einem langsamen Tool; die zweite Nachricht
    // wartet in der Mailbox, bis max_runtime den Schwarm stoppt -> Drain.
    let (_, a) = test_agent(vec![
        vec![
            Chunk::tool(0, "s1", "swarm_send", r#"{"to":"b","content":"eins"}"#),
            Chunk::tool(1, "s2", "swarm_send", r#"{"to":"b","content":"zwei"}"#),
        ],
        vec![Chunk::text("gesendet")],
    ]);
    let mut b_tools = ToolRegistry::new();
    b_tools.add(
        "schlafe",
        "Testtool: blockiert kurz.",
        json!({"type":"object","properties":{}}),
        |_| {
            std::thread::sleep(Duration::from_millis(500));
            Ok("ausgeschlafen".into())
        },
    );
    let b_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "z1", "schlafe", "{}")],
        vec![Chunk::text("fertig")],
    ]));
    let b = Agent::builder(b_llm)
        .tools(b_tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("drain")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .max_runtime(Duration::from_millis(250))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle
        .send_initial("a", "Schick b zwei Nachrichten.")
        .unwrap();

    let result = handle.join();
    assert_eq!(result.reason, CompletionReason::MaxRuntimeReached);
    assert!(
        result
            .dead_letters
            .iter()
            .any(|d| d.reason == DeliveryResult::RecipientUnavailable),
        "gedrainte Mailbox-Nachricht erwartet: {:?}",
        result.dead_letters
    );
}

// ------------------------------------------------------------------- Routing

#[test]
fn ping_pong_message_routing() {
    let (a_llm, a) = test_agent(vec![
        // Run 1 (Initialaufgabe): send an b, dann final.
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"b","content":"ping"}"#,
        )],
        vec![Chunk::text("gesendet")],
        // Run 2 (Reply von b): final.
        vec![Chunk::text("pong erhalten")],
    ]);
    let (b_llm, b) = test_agent(vec![
        vec![Chunk::tool(0, "r1", "swarm_reply", r#"{"content":"pong"}"#)],
        vec![Chunk::text("geantwortet")],
    ]);

    let handle = SwarmBuilder::new("pingpong")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Schick b ein Ping.").unwrap();

    // Drei Turns: a (Initial), b (ping), a (pong-Reply).
    let mut completed = 0;
    let seen = collect_until(&events, Duration::from_secs(5), |e| {
        if matches!(e, SwarmEvent::TurnCompleted { .. }) {
            completed += 1;
        }
        completed == 3
    });

    // b hat das gerenderte Format mit Absender a gesehen; a die Reply von b.
    let b_seen = seen_text(&b_llm);
    assert!(b_seen.contains("[SWARM MESSAGE]"), "{b_seen}");
    assert!(b_seen.contains("Von: a"));
    assert!(b_seen.contains("Art: information"));
    let a_seen = seen_text(&a_llm);
    assert!(a_seen.contains("Von: b"));
    assert!(a_seen.contains("Art: reply"));
    assert!(
        a_seen.contains("Antwort auf: msg-2"),
        "Reply-Verkettung: {a_seen}"
    );

    // Genau drei Zustellungen: Initial (runtime->a), a->b, b->a. Die
    // Event-Reihenfolge ÜBER Actor-Grenzen ist nicht garantiert (publiziert
    // wird nach der Zustellung, der Empfänger kann schneller sein) — daher
    // Mengen- statt Reihenfolge-Assertion.
    let mut queued: Vec<String> = seen
        .iter()
        .filter_map(|e| match e {
            SwarmEvent::MessageQueued { message } => Some(message.from.clone()),
            _ => None,
        })
        .collect();
    queued.sort();
    assert_eq!(queued, ["a", "b", "runtime"]);

    let result = handle.stop();
    assert_eq!(result.turns.get("a"), Some(&2));
    assert_eq!(result.turns.get("b"), Some(&1));
}

#[test]
fn topology_blocks_unconnected_recipient() {
    let (a_llm, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"c","content":"hallo"}"#,
        )],
        vec![Chunk::text("versucht")],
    ]);
    let (b_llm, b) = test_agent(vec![]);
    let (c_llm, c) = test_agent(vec![]);

    let handle = SwarmBuilder::new("topo")
        .agent("a", a)
        .agent("b", b)
        .agent("c", c)
        .connect_bidirectional("a", "b") // c ist NICHT mit a verbunden
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Schreib an c.").unwrap();
    collect_until(&events, Duration::from_secs(5), |e| turn_completed(e, "a"));

    // Das Modell bekam den Status "nicht_erlaubt" samt erlaubter Peers …
    let a_seen = seen_text(&a_llm);
    assert!(a_seen.contains("nicht_erlaubt"), "{a_seen}");
    assert!(a_seen.contains("erreichbar"));
    // … und c (wie b) hat nie eine Nachricht gesehen.
    assert_eq!(c_llm.calls(), 0);
    assert_eq!(b_llm.calls(), 0);

    let result = handle.stop();
    assert_eq!(result.turns.get("c"), None);
}

#[test]
fn broadcast_reaches_all_peers() {
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "b1",
            "swarm_broadcast",
            r#"{"content":"an alle"}"#,
        )],
        vec![Chunk::text("verteilt")],
    ]);
    let (b_llm, b) = test_agent(vec![vec![Chunk::text("ok b")]]);
    let (c_llm, c) = test_agent(vec![vec![Chunk::text("ok c")]]);

    let handle = SwarmBuilder::new("stern")
        .agent("a", a)
        .agent("b", b)
        .agent("c", c)
        .connect_bidirectional("a", "b")
        .connect_bidirectional("a", "c")
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Broadcast an alle.").unwrap();

    let mut b_done = false;
    let mut c_done = false;
    collect_until(&events, Duration::from_secs(5), |e| {
        b_done |= turn_completed(e, "b");
        c_done |= turn_completed(e, "c");
        b_done && c_done
    });
    assert!(seen_text(&b_llm).contains("an alle"));
    assert!(seen_text(&c_llm).contains("an alle"));

    let result = handle.stop();
    assert_eq!(result.turns.get("b"), Some(&1));
    assert_eq!(result.turns.get("c"), Some(&1));
    // Initial + 2 Broadcast-Zustellungen (jede zählt gegen das Budget).
    assert_eq!(result.messages_sent, 3);
}

// ------------------------------------------------------------------- Konsens

#[test]
fn consensus_completes_swarm() {
    // a schlägt vor (Proposal-ID deterministisch "msg-2": msg-1 ist die
    // Initialaufgabe), b und c stimmen zu; Quorum = 2.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"Aufgabe erledigt: 42"}"#,
        )],
        vec![Chunk::text("vorgeschlagen")],
    ]);
    let vote = r#"{"proposal_id":"msg-2","approve":true,"comment":"passt"}"#;
    let (_, b) = test_agent(vec![
        vec![Chunk::tool(0, "v1", "swarm_vote", vote)],
        vec![Chunk::text("abgestimmt")],
    ]);
    let (_, c) = test_agent(vec![
        vec![Chunk::tool(0, "v2", "swarm_vote", vote)],
        vec![Chunk::text("abgestimmt")],
    ]);

    let result = {
        let handle = SwarmBuilder::new("konsens")
            .agent("a", a)
            .agent("b", b)
            .agent("c", c)
            .connect_bidirectional("a", "b")
            .connect_bidirectional("a", "c")
            .completion(CompletionPolicy::Consensus {
                required_approvals: 2,
            })
            .build()
            .unwrap()
            .start()
            .unwrap();
        handle
            .send_initial("a", "Erledige die Aufgabe und schließe ab.")
            .unwrap();
        handle.join()
    };

    match result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            assert_eq!(approvals, 2);
            assert_eq!(proposal.content, "Aufgabe erledigt: 42");
            assert_eq!(proposal.kind, MessageKind::Proposal);
        }
        other => panic!("Konsens erwartet, war {other:?}"),
    }
}

#[test]
fn zero_quorum_completes_on_proposal_alone() {
    // required_approvals = 0: das Proposal erfüllt den Konsens ohne Votes.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"sofort fertig"}"#,
        )],
        vec![Chunk::text("vorgeschlagen")],
    ]);
    let result = {
        let handle = SwarmBuilder::new("solo")
            .agent("a", a)
            .completion(CompletionPolicy::Consensus {
                required_approvals: 0,
            })
            .build()
            .unwrap()
            .start()
            .unwrap();
        handle.send_initial("a", "Schließe ab.").unwrap();
        handle.join()
    };
    match result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            assert_eq!(approvals, 0);
            assert_eq!(proposal.content, "sofort fertig");
        }
        other => panic!("Konsens erwartet, war {other:?}"),
    }
}

#[test]
fn stop_wins_over_pending_completion_event() {
    // Der Konsens ist bereits publiziert (liegt ungelesen in der internen
    // Queue des Handles), erst DANACH ruft der Aufrufer stop(): das
    // versprochene `Stopped` muss trotzdem zurückkommen.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::text("vorgeschlagen")],
    ]);
    let handle = SwarmBuilder::new("solo")
        .agent("a", a)
        .completion(CompletionPolicy::Consensus {
            required_approvals: 0,
        })
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Schließe ab.").unwrap();
    collect_until(&events, Duration::from_secs(5), |e| {
        matches!(
            e,
            SwarmEvent::SwarmCompleted {
                reason: CompletionReason::Consensus { .. }
            }
        )
    });

    let result = handle.stop();
    assert_eq!(result.reason, CompletionReason::Stopped);
}

#[test]
fn vote_for_unknown_proposal_becomes_dead_letter() {
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "v1",
            "swarm_vote",
            r#"{"proposal_id":"msg-99","approve":true}"#,
        )],
        vec![Chunk::text("abgestimmt")],
    ]);
    let handle = SwarmBuilder::new("solo")
        .agent("a", a)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Stimm über msg-99 ab.").unwrap();
    collect_until(&events, Duration::from_secs(5), |e| {
        matches!(e, SwarmEvent::MessageRejected { .. })
    });

    let result = handle.stop();
    assert!(
        result.dead_letters.iter().any(|d| {
            d.reason == DeliveryResult::NotAllowed && d.message.kind == MessageKind::Vote
        }),
        "{:?}",
        result.dead_letters
    );
}

// -------------------------------------------------------------------- Limits

#[test]
fn message_limit_stops_swarm() {
    // Budget 2: Initial (1) + a->b (2); b's Reply läuft ins Limit.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"b","content":"ping"}"#,
        )],
        vec![Chunk::text("gesendet")],
    ]);
    let (b_llm, b) = test_agent(vec![
        vec![Chunk::tool(0, "r1", "swarm_reply", r#"{"content":"pong"}"#)],
        vec![Chunk::text("geantwortet")],
    ]);

    let result = {
        let handle = SwarmBuilder::new("limit")
            .agent("a", a)
            .agent("b", b)
            .connect_bidirectional("a", "b")
            .max_messages(2)
            .build()
            .unwrap()
            .start()
            .unwrap();
        handle.send_initial("a", "Schick b ein Ping.").unwrap();
        handle.join()
    };

    assert_eq!(result.reason, CompletionReason::MessageLimitReached);
    assert_eq!(result.messages_sent, 2);
    assert!(
        result
            .dead_letters
            .iter()
            .any(|d| d.reason == DeliveryResult::LimitReached),
        "{:?}",
        result.dead_letters
    );
    // b hat das Ping verarbeitet (sein Reply lief ins Limit); dass b den
    // Limit-Status noch als Tool-Ergebnis "sieht", ist nicht garantiert —
    // der Shutdown bricht seinen Turn unmittelbar nach dem Limit ab.
    assert!(seen_text(&b_llm).contains("[SWARM MESSAGE]"));
}

#[test]
fn full_mailbox_yields_dead_letter_and_refunds_budget() {
    // Kapazität 1, drei parallele Sends, während b im langsamen Tool hängt:
    // mindestens eine Zustellung wird abgewiesen — und abgewiesene
    // Zustellungen verbrauchen das Nachrichtenbudget NICHT (max_messages = 4
    // würde sonst durch Initial + 3 Versuche erschöpft und den Schwarm mit
    // MessageLimitReached beenden).
    let (_, a) = test_agent(vec![
        vec![
            Chunk::tool(0, "s1", "swarm_send", r#"{"to":"b","content":"eins"}"#),
            Chunk::tool(1, "s2", "swarm_send", r#"{"to":"b","content":"zwei"}"#),
            Chunk::tool(2, "s3", "swarm_send", r#"{"to":"b","content":"drei"}"#),
        ],
        vec![Chunk::text("gesendet")],
    ]);
    let mut b_tools = ToolRegistry::new();
    b_tools.add(
        "schlafe",
        "Testtool: blockiert kurz.",
        json!({"type":"object","properties":{}}),
        |_| {
            std::thread::sleep(Duration::from_millis(300));
            Ok("ausgeschlafen".into())
        },
    );
    let b_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "z1", "schlafe", "{}")],
        vec![Chunk::text("fertig 1")],
        vec![Chunk::text("fertig 2")],
        vec![Chunk::text("fertig 3")],
    ]));
    let b = Agent::builder(b_llm)
        .tools(b_tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("voll")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .mailbox_capacity(1)
        .max_messages(4)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let events = handle.events();
    handle.send_initial("a", "Flute b.").unwrap();

    collect_until(&events, Duration::from_secs(5), |e| {
        matches!(
            e,
            SwarmEvent::MessageRejected {
                result: DeliveryResult::MailboxFull,
                ..
            }
        )
    });
    let result = handle.stop();
    assert!(result
        .dead_letters
        .iter()
        .any(|d| d.reason == DeliveryResult::MailboxFull));
    // Budget-Erstattung: höchstens Initial + 2 erfolgreiche Zustellungen —
    // das Limit von 4 wurde durch die abgewiesenen Versuche NICHT erreicht.
    assert_eq!(result.reason, CompletionReason::Stopped);
    assert!(result.messages_sent <= 3, "{}", result.messages_sent);
}

// --------------------------------------------------- Abbruch von außen

// `join_with_cancel` ist der Pfad für Aufrufer, die den Schwarm INNERHALB eines
// laufenden Auftrags starten (das `swarm`-Tool): der Stop-Knopf des Nutzers muss
// den blockierenden `join` erreichen, obwohl der Schwarm selbst noch arbeitet.
#[test]
fn join_with_cancel_stoppt_den_schwarm() {
    let mut tools = ToolRegistry::new();
    tools.add(
        "schlafe",
        "Testtool: blockiert kurz.",
        json!({"type":"object","properties":{}}),
        |_| {
            std::thread::sleep(Duration::from_millis(400));
            Ok("ausgeschlafen".into())
        },
    );
    // Ohne Abbruch liefe der Agent ewig im Kreis (Tool, Tool, Tool …).
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "s1", "schlafe", "{}")],
        vec![Chunk::tool(0, "s2", "schlafe", "{}")],
        vec![Chunk::tool(0, "s3", "schlafe", "{}")],
    ]));
    let agent = Agent::builder(llm)
        .tools(tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("abbruch")
        .agent("a", agent)
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Arbeite lange.").unwrap();

    let cancel = agentkit::new_cancel();
    let flipper = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let started = Instant::now();
    let result = handle.join_with_cancel(&cancel);
    flipper.join().unwrap();

    assert_eq!(result.reason, CompletionReason::Stopped);
    // Deutlich unter der Zeit, die die drei Schlaf-Tools bräuchten.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "Abbruch hat zu lange gedauert: {:?}",
        started.elapsed()
    );
}

// Gegenprobe: eine nicht gesetzte Flagge ändert nichts am bisherigen Verhalten.
#[test]
fn join_with_cancel_ohne_abbruch_wie_join() {
    let (_, a) = test_agent(vec![vec![Chunk::text("hallo")]]);
    let handle = SwarmBuilder::new("solo-cancel")
        .agent("a", a)
        .max_runtime(Duration::from_millis(400))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Sag hallo.").unwrap();

    let result = handle.join_with_cancel(&agentkit::new_cancel());
    assert_eq!(result.reason, CompletionReason::MaxRuntimeReached);
    assert_eq!(result.turns.get("a"), Some(&1));
}

/// Ein planmäßiger Abschluss darf laufende Turns nicht mitten im Satz abschneiden.
///
/// Vorher riss der Konsens jeden gerade laufenden Turn ab; im Trace stand ein
/// "⛔ abgebrochen" direkt neben dem Erfolg und sah aus wie ein Fehler. Jetzt
/// klingt der Schwarm aus: `b` bringt seinen Turn zu Ende und taucht in der
/// Turn-Statistik auf, obwohl `a` den Konsens längst hergestellt hat.
#[test]
fn planmaessiger_abschluss_laesst_laufende_turns_ausklingen() {
    // a: verteilt an b und schlägt sofort den Abschluss vor (Quorum 0 -> fertig).
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"b","content":"leg los"}"#,
        )],
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::text("eingereicht")],
    ]);
    // b: braucht spürbar Zeit — genau der Turn, der früher abgeschnitten wurde.
    let mut b_tools = ToolRegistry::new();
    b_tools.add(
        "schlafe",
        "Testtool: blockiert kurz.",
        json!({"type":"object","properties":{}}),
        |_| {
            std::thread::sleep(Duration::from_millis(400));
            Ok("ausgeschlafen".into())
        },
    );
    let b_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "z1", "schlafe", "{}")],
        vec![Chunk::text("b ist fertig")],
    ]));
    let b = Agent::builder(b_llm)
        .tools(b_tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("ausklang")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 0,
        })
        .build()
        .unwrap()
        .start()
        .unwrap();
    // Vor dem Kickoff abonnieren — `TurnCompleted { success }` ist das
    // entscheidende Signal. Die reine Turn-ZAHL taugt nicht: ein abgebrochener
    // Turn meldet ebenfalls `TurnCompleted`, nur mit `success: false`.
    let rx = handle.events();
    handle.send_initial("a", "Los.").unwrap();
    let result = handle.join();

    assert!(matches!(result.reason, CompletionReason::Consensus { .. }));
    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SwarmEvent::TurnCompleted { agent, success: true, .. } if agent == "b"
        )),
        "b hat keinen Turn erfolgreich beendet — abgeschnitten statt ausgeklungen: {events:#?}"
    );
}

/// Ohne Laufzeitgrenze braucht es einen anderen Hänge-Schutz: verstummen die
/// Mitglieder, ohne vorzuschlagen, greift weder Konsens noch Nachrichtenbudget —
/// `join()` wartete sonst ewig. Der Leerlauf beendet den Schwarm stattdessen.
///
/// Bewusst LEERLAUF statt Gesamtzeit: ein Schwarm, der stundenlang produktiv
/// arbeitet, soll weiterlaufen dürfen.
#[test]
fn leerlauf_beendet_den_schwarm_ohne_laufzeitgrenze() {
    // `a` antwortet einmal und schweigt danach — kein Vorschlag, keine Nachricht.
    let (_, a) = test_agent(vec![vec![Chunk::text("bin fertig, sage aber nichts")]]);
    let handle = SwarmBuilder::new("leerlauf")
        .agent("a", a)
        // KEIN max_runtime — genau das ist der neue Default.
        .max_idle(Duration::from_millis(300))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Los.").unwrap();

    let start = Instant::now();
    let result = handle.join();
    assert_eq!(result.reason, CompletionReason::Idle);
    // Der Turn selbst wurde abgewartet, nicht abgeschnitten.
    assert_eq!(result.turns.get("a"), Some(&1));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "Leerlauf griff nicht: {:?}",
        start.elapsed()
    );
}

/// Der Leerlauf darf NICHT anschlagen, solange Arbeit aussteht. Ein Mitglied im
/// LLM-Stream erzeugt keine Schwarm-Events — ein reiner Stille-Detektor hielte es
/// fälschlich für untätig und schnitte es ab.
#[test]
fn leerlauf_greift_nicht_waehrend_gearbeitet_wird() {
    let mut tools = ToolRegistry::new();
    tools.add(
        "schlafe",
        "Testtool: blockiert länger als die Leerlauf-Grenze.",
        json!({"type":"object","properties":{}}),
        |_| {
            std::thread::sleep(Duration::from_millis(600));
            Ok("ausgeschlafen".into())
        },
    );
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "z1", "schlafe", "{}")],
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"fertig"}"#,
        )],
        vec![Chunk::text("eingereicht")],
    ]));
    let a = Agent::builder(llm)
        .tools(tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();

    let handle = SwarmBuilder::new("arbeitet")
        .agent("a", a)
        .completion(CompletionPolicy::Consensus {
            required_approvals: 0,
        })
        // Kürzer als der Tool-Aufruf dauert.
        .max_idle(Duration::from_millis(200))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Los.").unwrap();

    let result = handle.join();
    assert!(
        matches!(result.reason, CompletionReason::Consensus { .. }),
        "während laufender Arbeit als Leerlauf abgebrochen: {:?}",
        result.reason
    );
}

/// Konkurrierende Vorschläge müssen im Ergebnis auftauchen — nicht nur der Text
/// des Gewinners.
///
/// Beobachtet in einem echten Lauf: `features` und `architektur` reichten je
/// einen eigenen Abschluss-Vorschlag ein, einer stimmte dem fremden zu, und der
/// Schwarm endete auf DESSEN Text. Ein drittes Mitglied arbeitete am meisten und
/// stimmte nie ab. Aus `SwarmResult` war davon nichts zu sehen — weder dass es
/// einen Gegenvorschlag gab, noch von wem.
///
/// Der Ablauf ist bewusst SEQUENZIELL: `msg_seq` ist schwarmweit, die IDs hängen
/// also an der Reihenfolge nebenläufiger Turns. Solange `a` wartet, während `b`
/// vorschlägt, ist `msg-3` deterministisch b's Vorschlag.
#[test]
fn ergebnis_zeigt_konkurrierende_vorschlaege_und_stimmen() {
    // a: erst nur anstoßen (msg-2) und Turn beenden — danach ist a untätig.
    // Auf b's Vorschlag (msg-3) hin: eigener Gegenvorschlag (msg-4) UND Zustimmung zu b.
    let (_, a) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"b","content":"leg los"}"#,
        )],
        vec![Chunk::text("gesendet")],
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"Vorschlag A"}"#,
        )],
        vec![Chunk::tool(
            0,
            "v1",
            "swarm_vote",
            r#"{"proposal_id":"msg-3","approve":true}"#,
        )],
        vec![Chunk::text("fertig")],
    ]);
    // b: schlägt als Einziger vor, solange a wartet -> sein Vorschlag ist msg-3.
    let (_, b) = test_agent(vec![
        vec![Chunk::tool(
            0,
            "p2",
            "swarm_propose",
            r#"{"proposal":"Vorschlag B"}"#,
        )],
        vec![Chunk::text("eingereicht")],
    ]);

    let handle = SwarmBuilder::new("rivalen")
        .agent("a", a)
        .agent("b", b)
        .connect_bidirectional("a", "b")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_idle(Duration::from_secs(3))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Los.").unwrap();
    let result = handle.join();

    assert_eq!(result.required_approvals, 1);
    // BEIDE Vorschläge stehen im Ergebnis, auch der unterlegene.
    assert_eq!(
        result.proposals.len(),
        2,
        "erwartet: zwei Vorschläge, bekam {:?}",
        result.proposals
    );
    let angenommen: Vec<_> = result.proposals.iter().filter(|p| p.accepted).collect();
    assert_eq!(angenommen.len(), 1, "{:?}", result.proposals);
    assert_eq!(angenommen[0].from, "b");
    assert_eq!(angenommen[0].approvals, vec!["a".to_string()]);
    // Und der unterlegene ist als solcher erkennbar, samt Urheber.
    let unterlegen: Vec<_> = result.proposals.iter().filter(|p| !p.accepted).collect();
    assert_eq!(unterlegen.len(), 1);
    assert_eq!(unterlegen[0].from, "a");
    assert!(unterlegen[0].approvals.is_empty());
}

/// Die erste Stimme darf den Schwarm nicht sofort beenden — es gilt die
/// Abstimmungsfrist.
///
/// Gemessen an einem echten Lauf mit drei Mitgliedern: Quorum war 1, das erste
/// Votum entschied, und danach trafen noch zwei Vorschläge und drei Stimmen ein
/// — darunter sämtliche Stimmen des Mitglieds, das am meisten gearbeitet hatte.
///
/// Zur ID-Determiniertheit: mehrere Tool-Aufrufe EINER Antwort laufen parallel
/// (`parallel_tools`), ihre Reihenfolge steht also nicht fest. Der Vorschlag muss
/// deshalb in einem EIGENEN Modell-Schritt kommen — dann haben die beiden Sends
/// msg-2 und msg-3 verbraucht (in welcher Reihenfolge auch immer) und der
/// Vorschlag ist zwingend msg-4. Eine erste Fassung packte alles in einen Schritt
/// und war flaky: der Vorschlag bekam mal msg-2, `b` stimmte über eine nicht
/// existierende ID ab, und der Schwarm lief in den Leerlauf.
/// `b` stimmt zu, `c` schweigt: die Frist muss also voll ablaufen.
#[test]
fn abstimmungsfrist_wartet_auf_weitere_stimmen() {
    let (_, a) = test_agent(vec![
        vec![
            Chunk::tool(0, "s1", "swarm_send", r#"{"to":"b","content":"los"}"#),
            Chunk::tool(1, "s2", "swarm_send", r#"{"to":"c","content":"los"}"#),
        ],
        // Eigener Schritt: erst jetzt ist msg-4 sicher die nächste ID.
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"Gemeinsames Ergebnis"}"#,
        )],
        vec![Chunk::text("eingereicht")],
    ]);
    // b: erst Arbeitsnachricht (kein Vote), dann auf den Vorschlag hin zustimmen.
    let (_, b) = test_agent(vec![
        vec![Chunk::text("verstanden")],
        vec![Chunk::tool(
            0,
            "v1",
            "swarm_vote",
            r#"{"proposal_id":"msg-4","approve":true}"#,
        )],
        vec![Chunk::text("zugestimmt")],
    ]);
    // c stimmt NIE ab — der Grund, warum die Frist ablaufen muss.
    let (_, c) = test_agent(vec![
        vec![Chunk::text("schaue zu")],
        vec![Chunk::text("denke noch nach")],
    ]);

    let handle = SwarmBuilder::new("frist")
        .agent("a", a)
        .agent("b", b)
        .agent("c", c)
        .connect_bidirectional("a", "b")
        .connect_bidirectional("a", "c")
        .connect_bidirectional("b", "c")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .vote_window(Duration::from_millis(600))
        .max_idle(Duration::from_secs(5))
        .build()
        .unwrap()
        .start()
        .unwrap();
    handle.send_initial("a", "Los.").unwrap();

    let start = Instant::now();
    let result = handle.join();
    assert!(
        matches!(result.reason, CompletionReason::Consensus { .. }),
        "{:?}",
        result.reason
    );
    assert!(
        start.elapsed() >= Duration::from_millis(500),
        "die erste Stimme hat sofort entschieden: {:?}",
        start.elapsed()
    );
    assert_eq!(result.proposals.len(), 1, "{:?}", result.proposals);
    assert_eq!(result.proposals[0].approvals, vec!["b".to_string()]);
}
