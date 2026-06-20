//! Beispiel — ReAct-Agent mit lokalen Tools, ohne Netz (FakeLlm).
//!
//!     cargo run --example react_fake --no-default-features
//!
//! Zeigt den Loop mit Live-Events. Für einen echten LLM siehe
//! `agentkit::azure_from_env` / `openai_from_env` (Feature `openai`).

use std::sync::Arc;

use agentkit::events::{EventData, STEP, TEXT_DELTA, TOOL_CALL};
use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy, ToolRegistry};
use serde_json::{json, Value};

fn main() {
    let mut tools = ToolRegistry::new();
    tools.add(
        "wetter",
        "Aktuelles Wetter einer Stadt.",
        json!({"type":"object","properties":{"stadt":{"type":"string"}},"required":["stadt"]}),
        |args: Value| {
            let stadt = args["stadt"].as_str().unwrap_or("");
            Ok(format!("In {stadt}: 18°C, leicht bewölkt."))
        },
    );

    // FakeLlm: erst Tool-Call, dann finale Antwort (Token für Token).
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "c1", "wetter", "{\"stadt\":\"Wien\"}")],
        vec![
            Chunk::text("In Wien ist es "),
            Chunk::text("18°C und leicht bewölkt."),
        ],
    ]));

    let mut agent = Agent::builder(llm)
        .tools(tools)
        .strategy(Strategy::React)
        .build();

    let answer = agent.run_cb("Wie ist das Wetter in Wien?", None, |ev| match &ev.data {
        EventData::Step { step } if ev.etype == STEP => print!("\n[Schritt {step}] "),
        EventData::ToolCall { name, args } if ev.etype == TOOL_CALL => {
            print!("\n  🔧 {name}({args})\n")
        }
        EventData::TextDelta(t) if ev.etype == TEXT_DELTA => print!("{t}"),
        _ => {}
    });

    println!("\n\n=== Antwort ===\n{answer}");
}
