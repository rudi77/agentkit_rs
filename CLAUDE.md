# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository layout

Monorepo with six loosely coupled components. There is **no Cargo workspace at the root** — the Rust crates have independent manifests, so from the repo root always use `--manifest-path` (or `cd` into the crate).

| Directory | What it is |
|---|---|
| `agent_framework_rs/` | **agentkit** — the agent itself: agentic loop, tools, skills, sub-agents, MCP, and the CLI/REPL/TUI *logic*. Library only (plus the `bench` binary). **Has its own `CLAUDE.md` — read it before working there**; it covers the architecture, feature flags, test conventions and the Python-port design constraint in detail. |
| `ctxman_rs/` | **ctxman** — context management as a standalone, synchronous library (watermark GC, content-addressed blob store, LLM-backed compaction, byte-stable render pipeline). Behavior-faithful port of a C#/.NET service; deliberate deviations are listed in its `README.md`. |
| `agentkit_swarm/` | **agentkit-swarm** — actor-based agent-to-agent system on top of agentkit: one thread + bounded mailbox per agent, peer-to-peer tools (`swarm_send` & co.), consensus-based completion, plus the `swarm` tool that lets an agent build a swarm *at runtime* (`src/dynamic.rs`). No changes to the agent core; not a port (design decisions documented in its `README.md`). |
| `agentkit_graph/` | **agentkit-graph** — graph-based knowledge on top of agentkit: a Working Graph for the running task, a Canonical Graph for durable knowledge, mandatory provenance, `graph_*` tools and a `GraphAgent` recall wrapper. Storage is an in-memory snapshot (`RwLock<Arc<GraphIndex>>`) plus an append-only JSONL journal — **no storage dependency, no C compiler**. Not a port; design decisions in its `README.md`. |
| `agentkit_app/` | The installable **`agentkit` executable** (CLI/REPL/TUI) — a thin wiring crate that depends on agentkit, agentkit-swarm *and* (feature `graph`) agentkit-graph. It exists only because a binary inside `agent_framework_rs` would be a Cargo package cycle. Also builds the `tui` binary. |
| `agent_benchmarks/` | Python/uv harness running agentkit headless against SWE-bench Lite, Terminal-Bench 2.0 and Aider Polyglot (via Harbor). See its `README.md`. |

How they connect:

- agentkit consumes ctxman as a **path dependency** gated behind the Cargo feature `ctxman` (`agentkit --ctx <dir>`). A change to ctxman's public API can break agentkit — after touching `ctxman_rs/src`, also build/test agentkit with `--features ctxman`.
- agentkit_swarm consumes agentkit as a **path dependency** (`default-features = false`; `openai`/`ctxman` are passed through). The dependency direction between the *libraries* is one-way: the agent core does not know the swarm. A change to agentkit's public API (notably `run_on_bus`, `ToolRegistry`, `build_coding_agent`/`ExtraToolCtx`, the failure sentinels `"(abgebrochen)"`/`"(keine Antwort)"`/`"(max_steps erreicht)"`) can break agentkit_swarm — after touching `agent_framework_rs/src`, also run the swarm tests.
- agentkit_graph consumes agentkit as a **path dependency** (`default-features = false`). Same one-way direction as the swarm: the agent core does not know the graph. It has zero dependencies beyond agentkit/serde — deliberately, so the offline default and the static musl build stay intact.
- agentkit_app is the only crate that knows all three. It builds the coding agent through `agentkit::build_coding_agent` and injects `agentkit_swarm::add_swarm_tool` plus `agentkit_graph::register_graph_tools` through the `extra_tools` seam (`FrontendTools` in `agentkit_app/src/lib.rs`) — that seam is the *only* reason agentkit has an extension point at all. Dynamically created swarm members build their own registry, so the graph reaches them through `SwarmToolConfig::extra_member_tools`; a change to that field's shape breaks agentkit_app.
- The benchmark harness builds agentkit_app as a **static x86_64-musl binary** (`make build-agent`) and uploads it into each benchmark's task container; it invokes `agentkit -p … -y` and depends on agentkit's **exit-code contract** (0 ok, 1 runtime/max-steps, 2 API, 3 context) and on non-TTY stdin being read to EOF (containers must pipe `</dev/null`).

## Language convention (repo-wide)

**Everything user-visible is German**: doc comments, inline comments, system prompts, tool descriptions, CLI output, READMEs, and commit messages. Identifiers and types are English. The only English prose is in `agent_benchmarks/prompts/` (benchmark system prompts, intentionally English).

## Build and test

```bash
# agentkit — run this first, it needs no HTTP/TLS deps and no network
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features --features ctxman
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features skills   # single test by substring

# agentkit_app — the executable, full build as released
cargo test  --manifest-path agentkit_app/Cargo.toml --no-default-features
cargo build --manifest-path agentkit_app/Cargo.toml --features "tui pdf ctxman tiktoken"

# ctxman
cargo test --manifest-path ctxman_rs/Cargo.toml                  # core, offline, incl. golden byte-comparison
cargo test --manifest-path ctxman_rs/Cargo.toml --all-features   # adds http/tiktoken compiles

# agentkit_swarm — offline by default (default = [], FakeLlm only)
cargo test --manifest-path agentkit_swarm/Cargo.toml
cargo build --manifest-path agentkit_swarm/Cargo.toml --features "openai ctxman"   # feature pass-through compile check

# agentkit_graph — offline, no storage dependency
cargo test --manifest-path agentkit_graph/Cargo.toml
cargo test --manifest-path agentkit_app/Cargo.toml --no-default-features --features graph   # the wiring

# lint (per crate)
cargo clippy --manifest-path agent_framework_rs/Cargo.toml --all-targets
cargo clippy --manifest-path ctxman_rs/Cargo.toml --all-targets --all-features
cargo clippy --manifest-path agentkit_swarm/Cargo.toml --all-targets
cargo clippy --manifest-path agentkit_graph/Cargo.toml --all-targets
cargo clippy --manifest-path agentkit_app/Cargo.toml --all-targets
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

## Conventions shared by all crates

- **Issue tracking runs through the GitHub project.** All issues, feature requests and bugs are tracked in ["Projekt agentkit"](https://github.com/users/rudi77/projects/6). Create new issues with `gh issue create --repo rudi77/agentkit_rs --project "Projekt agentkit"` so they land on the board; issue texts are German (language convention). A `refactoring` label exists besides the GitHub defaults.
- **Coding guidelines are binding.** `CODING_GUIDELINES.md` at the repo root defines the design principles for all Rust code: simplicity first, abstraction only with ≥2 real users (rule of three), single responsibility per module/function, no over-engineering (YAGNI), why-comments only, measured performance claims, offline testability. Read it before writing or reviewing code; its review checklist applies to every change.
- **Synchronous, no async runtime.** No tokio anywhere; HTTP goes through `ureq` behind optional features (`openai` in agentkit, `http` in ctxman). ctxman's sync traits (`CompactionModel`, `PromotionSink`, `BlobStore`) exist because of this agentkit convention — keep new I/O interfaces synchronous.
- **Offline by default.** All crates' default test/build paths compile with zero HTTP/TLS dependencies.
- **Ports with a reference implementation.** agentkit is a structural 1:1 port of a Python original, ctxman of a C# service. Deviations must be deliberate and documented in the respective README's differences section ("Bewusste Unterschiede …") — do not diverge silently. agentkit_swarm and agentkit_graph are the exceptions: they are new designs, not ports; their explanatory decisions live in their READMEs' "Bewusste Design-Entscheidungen" sections (for agentkit_graph that section also records every deliberate deviation from its PRD).

## Releases

`.github/workflows/release.yml` runs on every `v*` tag (or manual dispatch with a `tag` input). It builds four agentkit binaries from `agentkit_app` (Windows/Linux × `voll`/`cli` — `voll` = `tui pdf ctxman tiktoken graph`, `cli` = `pdf ctxman tiktoken graph`), smoke-tests each (`--demo`, ctxman presence, `swarm` tool presence, graph tool presence, cli variant must reject `--tui`), runs the ctxman tests, packages ctxman as a `.crate` archive, runs the agentkit_swarm tests (no package — path dependency), and attaches `agentkit-examples.zip`. The release version comes from `agentkit_app/Cargo.toml` — bump it there when releasing (keep it in sync with `agent_framework_rs/Cargo.toml`). `scripts/agentkit_setup.ps1` and `INSTALL.md` download assets via `releases/latest/download/<asset>`, so asset names must stay stable and unversioned.
