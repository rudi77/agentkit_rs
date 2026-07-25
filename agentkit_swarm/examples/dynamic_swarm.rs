//! Beispiel — ein Agent baut sich seinen Schwarm zur Laufzeit selbst
//! (FakeLlm, kein Netz).
//!
//!     cargo run --example dynamic_swarm
//!
//! Der Orchestrator bekommt EIN zusätzliches Tool (`swarm`) und beschreibt darin
//! seinen Schwarm: Mitglieder, System-Prompts, Tool-Zugriff, Topologie, Quorum.
//! Das Tool baut den Schwarm, lässt ihn laufen und gibt das Ergebnis zurück —
//! der Orchestrator formuliert daraus die Antwort für den Nutzer.
//!
//! Zwei Dinge sind hier absichtlich sichtbar gemacht:
//!
//! 1. **Alle Mitglieder teilen sich EIN Modell.** Sie unterscheiden sich nur
//!    durch ihren System-Prompt — deshalb erkennt das `RollenLlm` unten den
//!    Fragesteller an der Zeile „Deine Agent-ID ist '…'", die `dynamic.rs` jedem
//!    Mitglied mitgibt. Mit einem echten LLM entfällt das ersatzlos.
//! 2. **Der Schwarm-Verkehr läuft über den Event-Bus des Orchestrators.** Der
//!    Consumer-Thread unten druckt genau das, was im TUI im Transcript landet.

use agentkit::coding::CodingTools;
use agentkit::llm::{chunk_stream, Chunk, ChunkStream, Llm, Message};
use agentkit::testing::FakeLlm;
use agentkit::{
    Agent, EventBus, EventData, McpHub, RunHandle, Strategy, ToolRegistry, FINAL, TOOL_CALL,
    TOOL_RESULT,
};
use agentkit_swarm::{add_swarm_tool, SwarmLimits, SwarmToolConfig};
use serde_json::{json, Value};
use std::sync::Arc;

/// Ein netzfreies Modell, das je nach Agent-ID im System-Prompt antwortet.
struct RollenLlm;

impl RollenLlm {
    fn antwort(system: &str, schritt: usize) -> Vec<Chunk> {
        let ist = |id: &str| system.contains(&format!("Deine Agent-ID ist '{id}'"));
        match (ist("architekt"), ist("kritiker"), schritt) {
            // Der Architekt fragt zuerst den Kritiker …
            (true, _, 0) => vec![Chunk::tool(
                0,
                "s1",
                "swarm_send",
                r#"{"to":"kritiker","content":"Vorschlag: Backoff mit Jitter, 3 Versuche.","kind":"request"}"#,
            )],
            // … und schlägt nach dessen Kritik den Abschluss vor.
            (true, _, 2) => vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                r#"{"proposal":"Backoff mit Jitter, 3 Versuche, harte Obergrenze 30 s."}"#,
            )],
            // Der Kritiker antwortet und stimmt dem Vorschlag später zu.
            (_, true, 0) => vec![Chunk::tool(
                0,
                "k1",
                "swarm_reply",
                r#"{"content":"Ergänze eine harte Obergrenze fürs Backoff.","kind":"critique"}"#,
            )],
            (_, true, 2) => vec![Chunk::tool(
                0,
                "v1",
                "swarm_vote",
                r#"{"proposal_id":"msg-4","approve":true,"comment":"Obergrenze drin"}"#,
            )],
            // Jeder Tool-Aufruf braucht einen zweiten Zug für die finale Antwort.
            _ => vec![Chunk::text("ok")],
        }
    }
}

impl Llm for RollenLlm {
    fn complete(&self, _m: &[Value], _t: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _t: Option<&[Value]>) -> Result<ChunkStream, String> {
        let system = messages
            .iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("");
        // Schritt = wie viele Assistant-Züge dieser Agent schon hatte.
        let schritt = messages.iter().filter(|m| m["role"] == "assistant").count();
        Ok(chunk_stream(RollenLlm::antwort(system, schritt)))
    }
}

/// Die Spezifikation, die ein echtes Modell als Tool-Argumente liefern würde.
fn spezifikation() -> Value {
    json!({
        "auftrag": "Legt das Retry-Verhalten des MCP-Clients fest und einigt euch auf eine Empfehlung.",
        "topologie": "kette",
        "erforderliche_zustimmungen": 1,
        "max_laufzeit_s": 20,
        "agenten": [
            {"id": "architekt", "system": "Du entwirfst die Lösung und verantwortest den Abschluss."},
            {"id": "kritiker",  "system": "Du suchst Schwachstellen und forderst konkrete Ergänzungen."}
        ]
    })
}

fn main() {
    let workspace = std::env::temp_dir().join("agentkit_dynamic_swarm_demo");
    std::fs::create_dir_all(&workspace).unwrap();

    // Der Orchestrator: EIN `swarm`-Aufruf, danach die Antwort an den Nutzer.
    // Der RunHandle muss VOR dem Build in Tool und Builder — nur so sieht das
    // Tool zur Laufzeit den aktiven Bus und den Stop-Knopf.
    let run = RunHandle::new();
    let mut tools = ToolRegistry::new();
    add_swarm_tool(
        &mut tools,
        SwarmToolConfig {
            run: run.clone(),
            llm: Arc::new(RollenLlm),
            coding: CodingTools::new(workspace.to_str().unwrap(), false),
            skills: None,
            roles: Vec::new(),
            mcp: Arc::new(McpHub::empty()),
            dry_run: false,
            limits: SwarmLimits::default(),
            extra_member_tools: None,
        },
    );

    let mut orchestrator = Agent::builder(Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "s1", "swarm", &spezifikation().to_string())],
        vec![Chunk::text(
            "Der Schwarm empfiehlt: Backoff mit Jitter, 3 Versuche, Obergrenze 30 s.",
        )],
    ])))
    .tools(tools)
    .strategy(Strategy::Plain)
    .retry_backoff_ms(0)
    .run_handle(run)
    .build();

    // Genau das, was ein Frontend tut: den Bus abonnieren und rendern.
    let bus = EventBus::new();
    let events = bus.subscribe();
    let drucker = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            let tag = if event.source.is_empty() {
                String::new()
            } else {
                format!("[{}] ", event.source)
            };
            match (&event.etype, &event.data) {
                (&TOOL_CALL, EventData::ToolCall { name, .. }) => println!("{tag}🔧 {name}"),
                (&TOOL_RESULT, EventData::ToolResult { name, result }) => {
                    println!("{tag}  ↳ {name}: {result}")
                }
                (&FINAL, EventData::Final(text)) if event.source.is_empty() => {
                    println!("\n=== Antwort ===\n{text}");
                    break;
                }
                _ => {}
            }
        }
    });

    orchestrator.run_on_bus(
        "Wie soll sich der MCP-Client bei Fehlern verhalten?",
        &bus,
        0,
        None,
        "",
    );
    drucker.join().unwrap();
    std::fs::remove_dir_all(&workspace).ok();
}
