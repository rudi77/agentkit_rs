# agentkit-app

Die installierbare Executable **`agentkit`** (CLI, REPL, TUI, Unix-Filter) — plus das
Binary `tui`. Dieses Crate enthält **keine Fachlogik**: es parst Argumente, rendert den
Event-Strom und verdrahtet die Bibliotheken.

## Warum es dieses Crate gibt

`agentkit-swarm` hängt von `agentkit` ab (ein Schwarm-Agent *ist* ein
`agentkit::Agent`). Die Executable braucht beide — sie klinkt das `swarm`-Tool in den
Coding-Agenten ein. Läge das Binary weiter in `agent_framework_rs`, wäre das ein
**Cargo-Paketzyklus**. Also:

```text
agentkit  ←  agentkit-swarm        (Bibliotheken: strikt einbahnig)
   ↖             ↗
    agentkit-app                   (nur dieses Crate kennt beide)
```

Der Agent-Kern kennt den Schwarm weiterhin nicht. Die einzige Naht ist
`agentkit::CodingAgentConfig::extra_tools` — eine Closure, die
`agentkit_swarm::add_swarm_tool` in die Registry des Haupt-Agenten registriert, bevor
der Agent gebaut wird. Sie steht samt Prompt-Fragment in `src/lib.rs`
(`swarm_extra_tools`, `system_with_swarm`); beide Binaries benutzen dieselben zwei
Funktionen.

Ergebnis: `agentkit`, `agentkit --repl` und `agentkit --tui` können zur Laufzeit
Agenten-Schwärme erzeugen. `--no-swarm` schaltet es ab. Im Demo-Modus (ohne
LLM-Zugang) gibt es keinen Coding-Agenten und damit auch kein `swarm`-Tool — dieselbe
Grenze gilt dort schon fürs `task`-Tool.

## Bauen, testen, installieren

```bash
cargo test  --manifest-path agentkit_app/Cargo.toml --no-default-features
cargo build --manifest-path agentkit_app/Cargo.toml --features "tui pdf ctxman tiktoken"
cargo install --path agentkit_app --bin agentkit --features "tui pdf ctxman tiktoken"
```

| Feature | Default | Wirkung |
|---|---|---|
| `openai` | **ja** | echter Azure/OpenAI-Pfad (an beide Bibliotheken durchgereicht) |
| `tui` | nein | `agentkit --tui` und das Binary `tui` (zieht `ratatui`) |
| `pdf` | nein | `read_pdf`-Tool + `agentkit read-pdf` |
| `ctxman` | nein | Context-Management (`--ctx DIR`), an beide Bibliotheken durchgereicht |
| `tiktoken` | nein | provider-genaue Token-Zählung; impliziert `ctxman` |

Release-Binaries: `voll` = `tui pdf ctxman tiktoken`, `cli` = `pdf ctxman tiktoken`.

## Bewusste Abweichung vom agentkit-Release-Profil

`[profile.release]` setzt hier **kein** `panic = "abort"`. agentkit tut das (spart
~1 MB Binary), der Schwarm-Supervisor braucht aber Thread-Unwinding: er erkennt einen
Actor-Panic daran, dass `JoinHandle::join` ein `Err` liefert, und beendet dann *den
Schwarm* kontrolliert. Mit `abort` würde stattdessen der ganze Prozess sterben. Da das
Release-Profil vom gebauten Top-Level-Paket kommt, entscheidet dieses Crate — und
entscheidet sich für Unwinding.

## Wo die Logik liegt

| Thema | Ort |
|---|---|
| Agent-Loop, Tools, Skills, Sub-Agenten, MCP | `../agent_framework_rs/src/` |
| Pipe-Bausteine (Exit-Codes, `--format json`, stdin) | `agentkit::cli` |
| Coding-Agent-Aufbau (gemeinsam für CLI und TUI) | `agentkit::app::build_coding_agent` |
| Terminal-UI | `agentkit::tui` |
| Schwarm-Laufzeit und das `swarm`-Tool | `../agentkit_swarm/src/` |
| Argument-Parsing, CLI-Renderer, Verdrahtung | `src/bin/agentkit.rs`, `src/bin/tui.rs` |
