//! Beispiel — paralleler Orchestrator mit Sub-Agents (FakeLlm, kein Netz).
//!
//!     cargo run --example parallel_subagents --no-default-features
//!
//! Der Orchestrator ruft `delegate` für mehrere Städte in EINER Antwort auf;
//! die Sub-Agents laufen nebenläufig. Alle Events laufen in denselben EventBus,
//! getaggt mit ihrer `source` — der sichtbare Beweis der Nebenläufigkeit.

use std::sync::Arc;

use agentkit::events::DONE;
use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{add_subagent, Agent, EventBus, Strategy, ToolRegistry};
use serde_json::{json, Value};

fn city_tools() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.add(
        "wetter",
        "Aktuelles Wetter einer Stadt.",
        json!({"type":"object","properties":{"stadt":{"type":"string"}},"required":["stadt"]}),
        |args: Value| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(format!(
                "Wetter in {}: 18°C",
                args["stadt"].as_str().unwrap_or("")
            ))
        },
    );
    reg
}

fn main() {
    let bus = EventBus::new();
    let q = bus.subscribe();

    // Jeder Sub-Agent: ruft 'wetter', dann finale Antwort.
    let sub_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "w0", "wetter", "{\"stadt\":\"Stadt\"}")],
        vec![Chunk::text("Steckbrief fertig")],
    ]));

    let mut orch_tools = ToolRegistry::new();
    add_subagent(
        &mut orch_tools,
        "delegate",
        "Delegiert einen Rechercheauftrag.",
        sub_llm,
        Some(city_tools()),
        Some("Recherchiere genau eine Stadt."),
        Strategy::Plain,
        Some(bus.clone()),
    );

    // Orchestrator: zwei delegate-Calls in EINER Antwort -> parallel.
    let orch_llm = Arc::new(FakeLlm::new(vec![
        vec![
            Chunk::tool(0, "d0", "delegate", "{\"auftrag\":\"Wien\"}"),
            Chunk::tool(1, "d1", "delegate", "{\"auftrag\":\"Berlin\"}"),
        ],
        vec![Chunk::text("Vergleich fertig.")],
    ]));

    // Consumer-Thread: zeigt die verschränkten Trace-Zeilen.
    let consumer = std::thread::spawn(move || {
        while let Ok(ev) = q.recv() {
            let src = if ev.source.is_empty() {
                "ORCH"
            } else {
                &ev.source
            };
            println!("[{src}] {}", ev.etype);
            if ev.etype == DONE && ev.source.is_empty() {
                break;
            }
        }
    });

    let mut orchestrator = Agent::builder(orch_llm)
        .tools(orch_tools)
        .strategy(Strategy::Plain)
        .build();
    let answer = orchestrator.run_on_bus("Vergleiche Wien und Berlin.", &bus, -1, None, "");

    consumer.join().unwrap();
    println!("\n=== Antwort ===\n{answer}");
}
