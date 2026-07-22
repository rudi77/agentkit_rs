# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository layout

Monorepo with three loosely coupled components. There is **no Cargo workspace at the root** — the two Rust crates have independent manifests, so from the repo root always use `--manifest-path` (or `cd` into the crate).

| Directory | What it is |
|---|---|
| `agent_framework_rs/` | **agentkit** — the agent itself: agentic loop, tools, skills, sub-agents, MCP, CLI/REPL/TUI. Library + `agentkit` executable. **Has its own `CLAUDE.md` — read it before working there**; it covers the architecture, feature flags, test conventions and the Python-port design constraint in detail. |
| `ctxman_rs/` | **ctxman** — context management as a standalone, synchronous library (watermark GC, content-addressed blob store, LLM-backed compaction, byte-stable render pipeline). Behavior-faithful port of a C#/.NET service; deliberate deviations are listed in its `README.md`. |
| `agent_benchmarks/` | Python/uv harness running agentkit headless against SWE-bench Lite, Terminal-Bench 2.0 and Aider Polyglot (via Harbor). See its `README.md`. |

How they connect:

- agentkit consumes ctxman as a **path dependency** gated behind the Cargo feature `ctxman` (`agentkit --ctx <dir>`). A change to ctxman's public API can break agentkit — after touching `ctxman_rs/src`, also build/test agentkit with `--features ctxman`.
- The benchmark harness builds agentkit as a **static x86_64-musl binary** (`make build-agent`) and uploads it into each benchmark's task container; it invokes `agentkit -p … -y` and depends on agentkit's **exit-code contract** (0 ok, 1 runtime/max-steps, 2 API, 3 context) and on non-TTY stdin being read to EOF (containers must pipe `</dev/null`).

## Language convention (repo-wide)

**Everything user-visible is German**: doc comments, inline comments, system prompts, tool descriptions, CLI output, READMEs, and commit messages. Identifiers and types are English. The only English prose is in `agent_benchmarks/prompts/` (benchmark system prompts, intentionally English).

## Build and test

```bash
# agentkit — run this first, it needs no HTTP/TLS deps and no network
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features --features ctxman
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features skills   # single test by substring

# agentkit — full build as released
cargo build --manifest-path agent_framework_rs/Cargo.toml --features "tui pdf ctxman tiktoken"

# ctxman
cargo test --manifest-path ctxman_rs/Cargo.toml                  # core, offline, incl. golden byte-comparison
cargo test --manifest-path ctxman_rs/Cargo.toml --all-features   # adds http/tiktoken compiles

# lint (per crate)
cargo clippy --manifest-path agent_framework_rs/Cargo.toml --all-targets
cargo clippy --manifest-path ctxman_rs/Cargo.toml --all-targets --all-features
cargo fmt --manifest-path <crate>/Cargo.toml
```

No test in either crate touches the network. agentkit tests script `FakeLlm` (`src/testing.rs`); ctxman's `tests/orchestration.rs` reads as the behavioral spec and `tests/golden/` holds byte-exact conformance fixtures from the C# original — **never regenerate the golden fixtures to make a test pass**; a byte diff there means the render pipeline changed behavior.

Benchmarks (need Docker, uv, API keys — see `agent_benchmarks/README.md`):

```bash
cd agent_benchmarks
make setup build-agent
make swebench-demo    # plumbing test, no API cost
make smoke            # smoke runs of all three benchmarks
```

## Conventions shared by both crates

- **Issue tracking runs through the GitHub project.** All issues, feature requests and bugs are tracked in ["Projekt agentkit"](https://github.com/users/rudi77/projects/6). Create new issues with `gh issue create --repo rudi77/agentkit_rs --project "Projekt agentkit"` so they land on the board; issue texts are German (language convention). A `refactoring` label exists besides the GitHub defaults.
- **Coding guidelines are binding.** `CODING_GUIDELINES.md` at the repo root defines the design principles for all Rust code: simplicity first, abstraction only with ≥2 real users (rule of three), single responsibility per module/function, no over-engineering (YAGNI), why-comments only, measured performance claims, offline testability. Read it before writing or reviewing code; its review checklist applies to every change.
- **Synchronous, no async runtime.** No tokio anywhere; HTTP goes through `ureq` behind optional features (`openai` in agentkit, `http` in ctxman). ctxman's sync traits (`CompactionModel`, `PromotionSink`, `BlobStore`) exist because of this agentkit convention — keep new I/O interfaces synchronous.
- **Offline by default.** Both crates' default test/build paths compile with zero HTTP/TLS dependencies.
- **Ports with a reference implementation.** agentkit is a structural 1:1 port of a Python original, ctxman of a C# service. Deviations must be deliberate and documented in the respective README's differences section ("Bewusste Unterschiede …") — do not diverge silently.

## Releases

`.github/workflows/release.yml` runs on every `v*` tag (or manual dispatch with a `tag` input). It builds four agentkit binaries (Windows/Linux × `voll`/`cli` — `voll` = `tui pdf ctxman tiktoken`, `cli` = `pdf ctxman tiktoken`), smoke-tests each (`--demo`, ctxman presence, cli variant must reject `--tui`), runs the ctxman tests, packages ctxman as a `.crate` archive, and attaches `agentkit-examples.zip`. The release version comes from `agent_framework_rs/Cargo.toml` — bump it there when releasing. `scripts/agentkit_setup.ps1` and `INSTALL.md` download assets via `releases/latest/download/<asset>`, so asset names must stay stable and unversioned.
