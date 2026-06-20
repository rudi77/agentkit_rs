//! Microbenchmarks für den Framework-Overhead (FakeLlm, kein Netz).
//!
//! Misst dieselben Szenarien wie das Python-Pendant
//! (`benchmarks/bench_python.py`), damit Rust vs. Python direkt vergleichbar
//! ist. Gibt am Ende eine JSON-Zeile `{"lang":"rust","results":{...}}` auf stdout
//! aus (vom Vergleichs-Runner geparst); menschenlesbare Zeilen gehen nach stderr.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use agentkit::llm::Chunk;
use agentkit::skills::parse_frontmatter;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, ShortTermMemory, Strategy, ToolRegistry};
use serde_json::{json, Value};

fn add_schema() -> Value {
    json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]})
}

fn build_add_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.add(
        "add",
        "Addiert zwei Zahlen.",
        add_schema(),
        |args: Value| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        },
    );
    reg
}

/// Misst eine Operation über `iters` Wiederholungen.
fn time<F: FnMut()>(iters: u64, mut f: F) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64
}

fn main() {
    let scale: f64 = std::env::var("AGENTKIT_BENCH_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    let it = |base: u64| ((base as f64 * scale) as u64).max(1);

    let mut results: Vec<(String, u64, f64)> = Vec::new();
    let mut run = |name: &str, iters: u64, f: &mut dyn FnMut()| {
        let ns = time(iters, f);
        eprintln!(
            "{name:<24} iters={iters:>9}  {:>10.1} ns/op  ({:.3} s total)",
            ns / iters as f64,
            ns / 1e9
        );
        results.push((name.to_string(), iters, ns));
    };

    // 1) Voller Agent-Loop: Tool-Call -> Ergebnis -> finale Antwort (frischer Agent).
    {
        let iters = it(50_000);
        run("agent_loop_single_tool", iters, &mut || {
            let reg = build_add_registry();
            let llm = Arc::new(FakeLlm::new(vec![
                vec![Chunk::tool(0, "c1", "add", "{\"a\":2,\"b\":3}")],
                vec![Chunk::text("Das Ergebnis ist 5.")],
            ]));
            let mut agent = Agent::builder(llm)
                .tools(reg)
                .strategy(Strategy::Plain)
                .build();
            black_box(agent.run("Was ist 2+3?"));
        });
    }

    // 2) Parallele Tool-Calls: 8 Tools in EINER Antwort (Thread-Overhead).
    {
        let iters = it(5_000);
        run("parallel_tools_8", iters, &mut || {
            let mut reg = ToolRegistry::new();
            reg.add(
                "noop",
                "Gibt ok zurück.",
                json!({"type":"object","properties":{"x":{"type":"integer"}},"required":["x"]}),
                |args: Value| Ok(format!("ok{}", args["x"].as_i64().unwrap_or(0))),
            );
            let turn1: Vec<Chunk> = (0..8)
                .map(|i| Chunk::tool(i, &format!("t{i}"), "noop", &format!("{{\"x\":{i}}}")))
                .collect();
            let llm = Arc::new(FakeLlm::new(vec![turn1, vec![Chunk::text("fertig")]]));
            let mut agent = Agent::builder(llm)
                .tools(reg)
                .strategy(Strategy::Plain)
                .parallel_tools(true)
                .build();
            black_box(agent.run("rechne"));
        });
    }

    // 3) Reiner Tool-Dispatch über die Registry.
    {
        let iters = it(500_000);
        let reg = build_add_registry();
        run("tool_dispatch", iters, &mut || {
            black_box(reg.call("add", json!({"a":2,"b":3})).unwrap());
        });
    }

    // 4) Token-Zählung über eine Historie (20 Nachrichten ~200 Zeichen).
    {
        let iters = it(200_000);
        let mut mem = ShortTermMemory::new(Some("Du bist ein hilfreicher Agent."));
        let filler = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam quis nostrud.";
        for i in 0..20 {
            mem.add(json!({"role":"user","content":format!("Nachricht {i}: {filler}")}));
        }
        run("token_count_history", iters, &mut || {
            black_box(mem.tokens());
        });
    }

    // 5) Frontmatter-Parsing einer SKILL.md.
    {
        let iters = it(500_000);
        let text = "---\nname: rechnungsrueckfrage\ndescription: Beantwortet Rückfragen zu Rechnungen sauber und vollständig.\n---\n\n# Rechnungsrückfrage\n\nSchritt 1. Lies die Rechnung.\nSchritt 2. Prüfe die Positionen.\n";
        run("frontmatter_parse", iters, &mut || {
            black_box(parse_frontmatter(text));
        });
    }

    // 6) JSON-Roundtrip eines Tool-Argument-Objekts.
    {
        let iters = it(300_000);
        run("json_roundtrip", iters, &mut || {
            let v = json!({"a":2,"b":3,"name":"test","items":[1,2,3],"nested":{"k":"v"}});
            let s = serde_json::to_string(&v).unwrap();
            let back: Value = serde_json::from_str(&s).unwrap();
            black_box(back);
        });
    }

    // JSON-Ergebniszeile für den Vergleichs-Runner.
    let entries: Vec<String> = results
        .iter()
        .map(|(name, iters, ns)| {
            format!(
                "\"{name}\":{{\"iters\":{iters},\"total_ns\":{ns},\"ns_per_op\":{:.4}}}",
                ns / *iters as f64
            )
        })
        .collect();
    println!(
        "{{\"lang\":\"rust\",\"results\":{{{}}}}}",
        entries.join(",")
    );
}
