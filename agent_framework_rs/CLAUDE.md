# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`agentkit` (Rust) — a small agent framework built on one idea: **an agent is an LLM in a loop with tools.**

```text
while the model calls a tool:
    run tool -> append result -> ask model again
else:
    final answer
```

It is a **structural port of the Python `agentkit`** in `../agent_framework` (part of the "AI Agents under the Hood" material). Keeping the two ports comparable is a *design constraint*, not an accident: module names, event type strings, tool names and behaviour are deliberately 1:1. Before restructuring anything, check the Python counterpart — `src/agent.rs` ↔ `agent.py`, `src/tools.rs` ↔ `tools.py`, and so on. Deviations that already exist are listed in `README.md` ("Bewusste Unterschiede zu Python") and each is justified; add to that list rather than diverging silently.

The crate ships a library plus the `agentkit` executable, which is both an interactive coding agent (CLI/REPL/TUI) and a Unix filter usable in pipelines.

## Language convention

**Everything user-visible is German**: doc comments, inline comments, system prompts, tool descriptions, CLI output, README/docs, and commit messages. Identifiers and types are English. Match this — a new tool description in English would be out of place.

## Build, test, run

```bash
cargo test --no-default-features         # tests — no HTTP/TLS deps needed, always run this
cargo test --no-default-features skills  # single test / filter by substring
cargo build                              # default features = `openai` (pulls ureq + rustls)
cargo clippy --all-targets
cargo fmt

cargo run --example react_fake --no-default-features
cargo run --example parallel_subagents --no-default-features
cargo run --bin tui --features tui                     # TUI needs its feature
cargo run --bin bench --release --no-default-features  # framework-overhead microbenchmarks
python3 ../benchmarks/compare.py --scale 0.2           # Rust vs. Python benchmark table
```

Tests live in one file, `tests/integration.rs` (31 tests), and use `FakeLlm` from `src/testing.rs` — **no test touches the network.** Keep it that way: a new feature is tested by scripting `FakeLlm` with the chunk sequence the model would have produced.

### Feature flags

| Feature | Default | Effect |
|---|---|---|
| `openai` | **yes** | real Azure/OpenAI path via `ureq` (sync HTTP, SSE parsed line-by-line). Without it the whole core builds with zero HTTP/TLS deps. |
| `tui` | no | `ratatui` terminal UI; gates `src/tui.rs` and the `tui` binary. crossterm comes re-exported via `ratatui::crossterm` — do not add a second crossterm dependency. |
| `pdf` | no | `read_pdf` tool + `agentkit read-pdf` subcommand via `pdf-extract`. |

Release binaries and `cargo install` use `--features "tui pdf"`.

## Architecture

### The loop and its harness

`Agent::drive` (`src/agent.rs`) is the single core; `run`, `run_cb`, `run_with_events` and `run_on_bus` are thin wrappers over it. Python's `run_iter` generator becomes a `FnMut(AgentEvent)` sink. The harness around the loop: `max_steps`, stream retries (3×), soft tool errors, memory compaction on `token_budget`, and cooperative cancellation via `Cancel = Arc<AtomicBool>`.

**Strategy is only a system-prompt preamble.** `Strategy::{React, Plan, Plain}` selects `REACT_PREAMBLE` / `PLAN_PREAMBLE` / nothing. There is no separate execution path.

**Error contract:** an unknown tool returns `Ok("ERROR: …")` (soft — the model self-corrects, no ERROR event); a tool that fails returns `Err` and *also* emits an ERROR event. Preserve this distinction.

### Events decouple the loop from every frontend

`src/events.rs`: the loop publishes typed `AgentEvent`s (`step`, `text_delta`, `tool_call`, `tool_result`, `plan`, `final`, `error`, `cancelled`, `done`). CLI, TUI and sub-agent forwarding are *just consumers* of the same stream — that is why adding a frontend never touches `agent.rs`. `EventData` is an enum (Python's `data: Any`), but the `type` strings stay identical to Python.

`EventData::Plan` carries the structured `Vec<Step>`, not a pre-rendered string; each frontend renders it itself via `render_steps` (CLI multi-line, TUI single-line).

### RunHandle — how tools reach the live run

`ToolRegistry` is `Clone` and gets **copied** when the agent is built. So a tool that needs the *currently active* `EventBus`/`Cancel` (notably the `task` tool) cannot capture them at registration time. It holds a `RunHandle` instead — an `Arc`-shared cell that `drive()` overwrites at the start of every run. Consequence: a tool needing the run context must be registered **before** `build()`, and the same `RunHandle` must be passed to `AgentBuilder::run_handle`. `app.rs::build_coding_agent` shows the canonical wiring.

### Tools

`src/tools.rs`: a tool is a JSON schema plus a `Fn(Value) -> Result<String, String>`. Rust has no runtime reflection, so **schemas are written out explicitly** in `registry.add(...)` (`add_typed` deserializes args into a struct but still takes the schema). `ToolFn` is an `Arc`, which makes the registry cheap to clone and `Send + Sync` — that is what allows parallel tool execution (`std::thread::scope`) and sub-agents owning their own registry copy.

`src/coding.rs` — sandboxed coding tools (`list_files`, `glob_files`, `grep`, `read_file`, `read_pdf`, `write_file`, `edit_file`, `run_shell`). Two safety nets: every path is confined to the workspace (`safe()`), and `run_shell` goes through an `ApproveFn` callback. `READ_ONLY_TOOLS` is the subset handed to read-only sub-agent roles.

`--dry-run` works by rebuilding the registry with `dry_run_blocking(is_likely_destructive)`: destructive tools become no-ops that report themselves, **but the schemas stay identical** so the model sees the same toolbox and the loop is unchanged.

### Sub-agents and roles

`src/roles.rs` gives the agent one `task` tool (Claude-Code style) with a `subagent_type` parameter: `general`, `explorer`, `reviewer`, `tester`, plus custom roles loaded from `*.md` files (`--agents DIR`, frontmatter = metadata, body = system prompt — same format as skills). A role is pure data: system prompt + tool subset + strategy.

Hard limits, by design: sub-agents **never** get the `task` tool (exactly one level deep, no recursion), and they share the one workspace. Multiple `task` calls in a single model response run in parallel and forward all their events into the same bus, tagged with `source`.

### MCP

`src/mcp.rs` — stdio JSON-RPC, **synchronous** (a `Mutex`-guarded session; no async runtime anywhere in this crate). Servers are declared in `.mcp.json` (Claude Code format, auto-discovered in workspace then CWD). Tools appear namespaced as `mcp__<server>__<tool>`.

Live enable/disable (REPL `/mcp on|off`, TUI F2) works by keeping a **MCP-free base registry**: `McpHub::apply(&mut agent)` returns it, and `McpHub::rewire(&mut agent, &base)` rebuilds `agent.tools` from `base.clone()` + the currently enabled servers. Only an atomic `enabled` flag flips; sessions are never torn down.

### Frontends

`src/app.rs` holds everything CLI and TUI share (`build_coding_agent`, `.env` loading, plan rendering, the platform-specific `run_shell` hint). The *only* real difference between the frontends is the approval callback — CLI asks on stdin, TUI opens a dialog — so it is passed in.

`src/cli.rs` holds the decoupled, testable pipe primitives (exit codes, `OutputFormat`, `read_stdin_context`, `extract_json`, `classify_outcome`); argument parsing itself lives in `src/bin/agentkit.rs`.

### The executable as a Unix filter

Stream contract (hexagonal — the agent core is untouched): **stdin** = context only (piped input is appended to the query); **stdout** = only the final, cleaned result when piped / `-p` / `--format json`; **stderr** = everything else (status, tool trace, ReAct thoughts). Exit codes: `0` ok, `1` runtime error, `2` API/network, `3` context too large or prompt invalid, `4` `--format` not satisfiable after retries. Keep these stable — pipelines in `examples/accounts_payable` depend on them.

Two behaviours that surprise people scripting the CLI:

- **`-p` silences the whole tool trace.** `Renderer::handle` returns early on `quiet`, and `quiet = print_mode` — so with `-p` you get no `tool_call`/`tool_result` lines on stderr *even with `--steps`*. To keep stdout clean **and** see the trace, drop `-p` and use `--format json --steps`: `clean_stdout` is already true in JSON mode, so the result still goes to stdout alone. `examples/win_triage` relies on this to show what `--dry-run` blocked.
- **`--dry-run` blocks by tool *name*.** `is_likely_destructive` matches substrings, so `run_shell`, `write_file` and `edit_file` are no-ops — but so is `update_plan` (it contains "update"). Harmless, but it shows up in the trace.

Piped stdin is not optional in a script: when stdin is not a TTY, `read_stdin_context` reads it to EOF. A background/non-interactive invocation with an inherited-but-never-closed stdin **hangs**. Always pipe something (even an empty string).

`--profile FILE` bundles per-stage config (system prompt, strategy, tools, skills, MCP allowlist, …) as JSON; explicit CLI flags override profile values.

`examples/accounts_payable/` is the worked example of the composition principle: a PowerShell pipeline of one agent (or one deterministic tool) per step — deterministic `read-pdf`/xcheck for facts, LLM agents for judgement.

## Adding things

- **A tool:** `registry.add(name, german_description, json!(schema), closure)`. If it needs the live bus/cancel, take a `RunHandle` clone and register before `build()`.
- **An event type:** add the `&'static str` const *and* an `EventData` variant, then handle it in the CLI `Renderer` and the TUI — the compiler will point at both.
- **A sub-agent role:** one `AgentRole` entry in `builtin_roles()`, or just drop a `.md` file in the `--agents` directory (no code).
- **A frontend:** subscribe to the `EventBus`; do not add anything to `agent.rs`.
