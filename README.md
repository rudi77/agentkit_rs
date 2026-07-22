# agentkit_rs

**agentkit** in Rust — ein Agent ist ein LLM in einer Schleife mit Tools. Dieses Repo
bündelt den Agent-Kern, das Context-Management und die Benchmark-Harness:

| Komponente | Verzeichnis | Was es ist |
|---|---|---|
| **agentkit** | [`agent_framework_rs/`](agent_framework_rs/) | Der Agent: agentic Loop, Tools, Skills, Sub-Agenten, MCP, TUI/REPL/One-shot, `read-pdf` — eine kleine native Executable |
| **ctxman** | [`ctxman_rs/`](ctxman_rs/) | Context-Management als eigenständige Bibliothek: Watermark-GC, content-addressed Blob Store, LLM-gestützte Compaction, byte-stabile Render-Pipeline |
| **agent_benchmarks** | [`agent_benchmarks/`](agent_benchmarks/) | Harness für SWE-bench Lite, Terminal-Bench 2.0 und Aider Polyglot — agentkit headless in den Task-Containern |

agentkit bindet ctxman über das Cargo-Feature `ctxman` ein (`agentkit --ctx <dir>`);
seit v0.13.1 ist es in allen Release-Binaries enthalten.

## Installation

Fertige Binaries (Windows/Linux, mit und ohne TUI) hängen an jedem
[GitHub-Release](https://github.com/rudi77/agentkit_rs/releases) — Details und alle
Wege (Setup-Skript, `cargo install`, Install-Skripte) in [INSTALL.md](INSTALL.md).

Schnellster Weg unter Windows:

```powershell
irm https://raw.githubusercontent.com/rudi77/agentkit_rs/main/scripts/agentkit_setup.ps1 | iex
```

Ohne API-Key läuft ein netzfreier Demo-Modus:

```bash
agentkit --demo "Was ist 17 + 25?"
```

Für ein echtes Modell (Azure OpenAI oder OpenAI-kompatibel) die Vorlage
[`agent_framework_rs/.env.example`](agent_framework_rs/.env.example) nach `.env`
kopieren und ausfüllen — oder global `~/.agentkit/config.json` nutzen (siehe
[INSTALL.md](INSTALL.md#konfiguration-agentkitconfigjson)); die Benchmarks haben
eine eigene Vorlage ([`agent_benchmarks/.env.example`](agent_benchmarks/.env.example)).

## Entwicklung

```bash
# Bauen & testen (Agent-Kern)
cargo build --manifest-path agent_framework_rs/Cargo.toml --features "tui pdf ctxman"
cargo test  --manifest-path agent_framework_rs/Cargo.toml

# ctxman (Bibliothek)
cargo test --manifest-path ctxman_rs/Cargo.toml
cargo test --manifest-path ctxman_rs/Cargo.toml --features http
```

Releases baut [`.github/workflows/release.yml`](.github/workflows/release.yml) bei jedem
`v*`-Tag: vier Binaries (Windows/Linux × voll/cli), das `ctxman`-Crate-Archiv und
`agentkit-examples.zip` mit den kompletten Beispielen.

## Mitwirken

Planung und Tracking laufen zentral über das GitHub-Projekt
[**„Projekt agentkit"**](https://github.com/users/rudi77/projects/6): Alle Issues,
Feature-Requests und Bugs werden dort erfasst — neue Einträge bitte immer dem Projekt
zuordnen (`gh issue create --project "Projekt agentkit"`). Für Code-Beiträge gelten
die verbindlichen [Coding-Guidelines](CODING_GUIDELINES.md): Einfachheit zuerst,
Abstraktion nur bei nachgewiesenem Bedarf, Single Responsibility — inklusive
Review-Checkliste.

## Herkunft

Die Historie wurde per `git filter-repo` aus dem [fsod-Repo](https://github.com/rudi77/fsod)
extrahiert (Stand v0.13.1); dort liegt weiterhin das Python-Original (`agent_framework/`),
zu dem der Rust-Port strukturgleich gehalten ist.
