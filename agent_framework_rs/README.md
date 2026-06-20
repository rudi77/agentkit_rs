# agentkit (Rust)

Rust-Port des Python-`agentkit` aus [`../agent_framework`](../agent_framework) —
**so strukturgleich wie möglich**, damit sich Rust und Python direkt vergleichen
lassen. Kernidee bleibt: **Ein Agent ist ein LLM in einer Schleife mit Tools.**

```text
solange das Modell ein Tool aufruft:
    Tool ausführen -> Ergebnis anhängen -> Modell erneut fragen
sonst:
    finale Antwort
```

## Was drin ist (1:1 zum Python-Original)

| Baustein | Datei | Python-Pendant |
|---|---|---|
| **Agentic Loop** | `src/agent.rs` | `agentkit/agent.py` — streamend, event-basiert; ReAct/Plan/Plain über `Strategy`; parallele Tool-Calls; Harness (max_steps, Retries, Fehlertoleranz, Compaction, Stop-Knopf) |
| **Tools** | `src/tools.rs` | `tools.py` — `ToolRegistry` (Schema explizit; Rust hat keine Laufzeit-Reflection) |
| **Coding-Tools** | `src/coding.rs` | `coding.py` — `CodingTools` mit Sandbox + Approval |
| **Skills** | `src/skills.rs` | `skills.py` — `Skills` + `list_skills`/`read_skill`, progressive disclosure |
| **Planning** | `src/planning.rs` | `planning.py` — `Plan` + `update_plan` |
| **Sub-Agents** | `src/subagents.rs` | `subagents.py` — `add_subagent` / `Subagent` |
| **Events** | `src/events.rs` | `events.py` — `AgentEvent` + `EventBus` (mpsc-Kanäle) |
| **Memory** | `src/memory.rs` | `memory.py` — `ShortTermMemory` + `LongTermMemory` |
| **MCP** | `src/mcp.rs` | `mcp.py` — `MCPClient` (synchrone stdio-Session, ohne async-Runtime) |
| **LLM** | `src/llm.rs` | `llm.py` — `Llm`-Trait + `OpenAiLlm` (Azure/OpenAI über `ureq`) |
| **FakeLlm** | `src/testing.rs` | der `FakeLLM` aus den Python-Tests |

### Bewusste Unterschiede zu Python

- **Tool-Schemas explizit.** Python leitet das Schema per `@tool()` aus Typ-Hints
  + Docstring ab. Rust hat keine Laufzeit-Reflection — das Schema wird als
  `serde_json::Value` übergeben (`registry.add(...)`). `add_typed` deserialisiert
  die Argumente typsicher.
- **Events typisiert.** Statt `data: Any` eine `EventData`-Enum; die `type`-Strings
  (`"step"`, `"tool_call"`, …) sind identisch.
- **Streaming per Callback statt Generator.** `run_iter` (Python-Generator) wird zu
  `run_with_events(task, cancel, |ev| ...)`. Darauf bauen `run`, `run_cb` und
  `run_on_bus` auf.
- **Parallele Tools** über `std::thread::scope` (Python: `ThreadPoolExecutor`).
- **MCP synchron.** Der stdio-Transport ist zeilengetrenntes JSON-RPC; in Rust
  genügt eine `Mutex`-geschützte Session — keine asyncio-Schleife im Thread nötig.

## In 12 Zeilen (ohne Netz, FakeLlm)

```rust
use std::sync::Arc;
use agentkit::{Agent, ToolRegistry};
use agentkit::testing::FakeLlm;
use agentkit::llm::Chunk;
use serde_json::json;

let mut tools = ToolRegistry::new();
tools.add("add", "Addiert zwei Zahlen.",
    json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
    |args| Ok((args["a"].as_i64().unwrap() + args["b"].as_i64().unwrap()).to_string()));

let llm = Arc::new(FakeLlm::new(vec![
    vec![Chunk::tool(0, "c1", "add", "{\"a\":17,\"b\":25}")],
    vec![Chunk::text("Das Ergebnis ist 42.")],
]));
let mut agent = Agent::new(llm, tools);
println!("{}", agent.run("Was ist 17 + 25?"));
```

Mit echtem Modell (Feature `openai`, Default an):

```rust
let llm = std::sync::Arc::new(agentkit::azure_from_env()?); // oder openai_from_env()
let mut agent = agentkit::Agent::new(llm, tools);
```

## Bauen, Testen, Beispiele

```bash
cargo test --no-default-features          # Tests ohne Netz/TLS-Abhängigkeiten
cargo build                               # mit Feature `openai` (ureq + rustls)
cargo run --example react_fake --no-default-features
cargo run --example parallel_subagents --no-default-features
```

## Performance: Rust vs. Python

Die Benchmarks messen **reinen Framework-Overhead** mit einem FakeLlm (kein Netz —
bei echten Calls dominiert die LLM-Latenz und ist für beide identisch). Beide Seiten
fahren **dieselben Szenarien mit denselben Iterationszahlen**; die Token-Zählung
nutzt beidseitig den `len//4`-Fallback (kein tiktoken).

```bash
python3 ../benchmarks/compare.py          # baut Rust-Release + führt beide aus
python3 ../benchmarks/compare.py --scale 0.2   # schneller
```

Beispiel-Lauf (Linux, Python 3.11; vollständige Tabelle in
[`../benchmarks/RESULTS.md`](../benchmarks/RESULTS.md)):

| Szenario | Python | Rust | Speedup |
|---|---:|---:|---:|
| Agent-Loop (1 Tool + Antwort) | 17.6 µs | 6.4 µs | 2.8× |
| 8 parallele Tool-Calls | 876 µs | 261 µs | 3.4× |
| Tool-Dispatch (Registry.call) | 271 ns | 105 ns | 2.6× |
| Token-Zählung (20 Msgs) | 2.03 µs | 430 ns | 4.7× |
| Skill-Frontmatter parsen | 1.15 µs | 220 ns | 5.3× |
| JSON dump+parse | 4.72 µs | 1.18 µs | 4.0× |

**Geometrisches Mittel ≈ 3.6× schneller.** Einordnung:

- Rechenlastige, allokationsarme Pfade (Token-Zählung, Frontmatter-Parsing) profitieren
  am stärksten (~5×).
- Der volle Agent-Loop liegt niedriger (~2.8×): Ein großer Teil ist `serde_json`-Value-
  Allokation/-Klonen und Thread-Aufbau — beide Sprachen allozieren hier. Dafür ist
  die Speichernutzung in Rust deutlich kompakter und ohne GC-Pausen.
- Bei **echten** LLM-Calls verschwindet dieser Overhead im Netzwerk-Rauschen — der
  Rust-Vorteil zählt v. a. bei hohem Tool-/Event-Durchsatz, vielen parallelen
  Sub-Agents und vorhersagbarer Latenz (kein GC).
