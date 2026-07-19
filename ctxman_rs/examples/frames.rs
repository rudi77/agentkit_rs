//! Frames-Beispiel (Spec §2.5): Subagent-Aufrufe als Stack — push, isolierte Frame-Sicht,
//! LIFO-Disziplin und pop mit Return-Segment im Parent.
//!
//! Ausführen: `cargo run --example frames`

use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::{
    AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions, StaticSegmentSpec,
};

fn render(session: &mut ContextSession, scope: RenderScope) -> Result<String, CtxmanError> {
    Ok(session
        .render(RenderOptions {
            provider: "anthropic".to_string(),
            scope,
            turn_advance: false, // nur Anschauung — kein echter Model-Call
        })?
        .canonical_json)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut session =
        ContextSession::new(PolicyConfig::default_policy(), CtxmanServices::default());

    session.bump_static_epoch(vec![StaticSegmentSpec {
        kind: "system_prompt".to_string(),
        role: Some(Role::System),
        content: "Du bist ein Recherche-Agent.".to_string(),
        source: Some("core".to_string()),
    }])?;

    // Haupt-Konversation (Root-Frame) — eine gepinnte Task überlebt jede GC und ist auch
    // in der isolierten Subagent-Sicht präsent.
    session.append_segment(AppendRequest {
        pinned: true,
        ..AppendRequest::inline("task", Some(Role::User), "Task: Bibliothek X evaluieren.")
    })?;
    session.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "Prüfe zuerst die Lizenz.",
    ))?;

    // Subagent starten: neuer Frame — alle folgenden Appends landen automatisch darin.
    let research = session.push_frame("lizenz_recherche");
    session.append_segment(AppendRequest::inline(
        "assistant_msg",
        Some(Role::Assistant),
        "Ich lese die LICENSE-Datei …",
    ))?;
    session.append_segment(AppendRequest::inline(
        "user_msg",
        Some(Role::User),
        "Zwischenstand: MIT, kompatibel.",
    ))?;

    // Geschachtelter Subagent (LIFO: muss vor `research` gepoppt werden).
    let detail = session.push_frame("detail_check");
    session.append_segment(AppendRequest::inline(
        "assistant_msg",
        Some(Role::Assistant),
        "Detail: keine Copyleft-Klauseln.",
    ))?;

    // Isolierte Subagent-Sicht (scope=frame): Static + gepinnte Root-Segmente + Tip-Frame.
    println!("— Frame-Sicht des Detail-Subagents —");
    println!("{}\n", render(&mut session, RenderScope::Frame)?);

    // LIFO-Disziplin: der äußere Frame kann nicht vor seinem Kind gepoppt werden.
    match session.pop_frame(research, "zu früh", None) {
        Err(CtxmanError::FrameDiscipline(msg)) => println!("LIFO-Schutz greift: {msg}\n"),
        other => panic!("erwartet FrameDiscipline, bekam {other:?}"),
    }

    // Kinder zuerst: Detail-Frame poppen, dann den Recherche-Frame.
    session.pop_frame(detail, "Detail-Ergebnis: keine Copyleft-Klauseln.", None)?;
    let pop = session.pop_frame(
        research,
        "Lizenz-Ergebnis: MIT, kommerzielle Nutzung unproblematisch.",
        None,
    )?;
    println!(
        "Recherche-Frame gepoppt — Return-Segment {} liegt im Root-Frame.\n",
        pop.return_segment_id
    );

    // Parent-Sicht: die Zwischenschritte der Subagents sind verschwunden (evicted),
    // nur die subagent_return-Segmente bleiben in der Chronologie.
    println!("— Parent-Sicht nach den Pops (scope=path) —");
    println!("{}\n", render(&mut session, RenderScope::Path)?);

    println!("— Events —");
    for event in session.drain_events() {
        println!("seq={} {}", event.seq, event.event_type);
    }
    Ok(())
}
