//! agentkit (Rust) — ein ganz einfaches Agent-Framework, portiert aus dem
//! Python-`agentkit` ("AI Agents under the Hood").
//!
//! Ein Agent ist ein LLM in einer Schleife mit Tools. Dieses Crate bündelt die
//! Bausteine strukturgleich zum Python-Original — ohne unnötige Abstraktion:
//!
//! ```no_run
//! use std::sync::Arc;
//! use agentkit::{Agent, ToolRegistry};
//! use agentkit::testing::FakeLlm;
//! use agentkit::llm::Chunk;
//! use serde_json::json;
//!
//! let mut tools = ToolRegistry::new();
//! tools.add(
//!     "add",
//!     "Addiert zwei Zahlen.",
//!     json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
//!     |args| {
//!         let a = args["a"].as_i64().unwrap_or(0);
//!         let b = args["b"].as_i64().unwrap_or(0);
//!         Ok((a + b).to_string())
//!     },
//! );
//!
//! // Ohne Netz: ein FakeLlm spielt zwei Turns ab.
//! let llm = Arc::new(FakeLlm::new(vec![
//!     vec![Chunk::tool(0, "c1", "add", "{\"a\":17,\"b\":25}")],
//!     vec![Chunk::text("Das Ergebnis ist 42.")],
//! ]));
//! let mut agent = Agent::new(llm, tools);
//! println!("{}", agent.run("Was ist 17 + 25?"));
//! ```

pub mod agent;
pub mod app;
pub mod cli;
pub mod coding;
pub mod demo;
pub mod events;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod planning;
pub mod roles;
pub mod skills;
pub mod subagents;
pub mod testing;
pub mod tools;

// Interaktives Terminal-UI — nur mit Feature `tui` (zieht `ratatui`).
#[cfg(feature = "tui")]
pub mod tui;

// Kern
pub use agent::{
    new_cancel, to_assistant_dict, Agent, AgentBuilder, Cancel, RunHandle, Strategy, PLAN_PREAMBLE,
    REACT_PREAMBLE,
};
pub use tools::{is_likely_destructive, ToolFn, ToolRegistry};

// CLI-Adapter: Unix-Pipe-Bausteine (Exit-Codes, Format, Stream-/JSON-Helfer).
pub use cli::{
    build_task, classify_outcome, extract_json, read_stdin_context, ExitCode, OutputFormat,
    JSON_SYSTEM,
};

// LLM
pub use llm::{Chunk, Delta, Llm, Message, ToolCallDelta};

// Memory
pub use memory::{count_tokens_text, truncate, LongTermMemory, ShortTermMemory};

// Planning
pub use planning::{Plan, Step};

// Coding-Tools
pub use coding::{coding_tools, ApproveFn, CodingTools, CODING_SYSTEM, READ_ONLY_TOOLS};
#[cfg(feature = "pdf")]
pub use coding::extract_pdf_text;

// Skills
pub use skills::{
    body_after_frontmatter, parse_frontmatter, skills_tools, SkillInfo, Skills, SKILL_SYSTEM,
};

// Sub-Agents
pub use subagents::{add_subagent, Subagent};

// Sub-Agent-Rollen + task-Tool (Claude-Code-Stil)
pub use roles::{
    add_task_tool, builtin_roles, load_roles_from_dir, merge_roles, strategy_from_str, AgentRole,
    GENERAL_SUBAGENT_SYSTEM, SUBAGENT_SYSTEM,
};

// Gemeinsame Frontend-Bausteine (CLI + TUI)
pub use app::{
    build_coding_agent, load_dotenv, plan_with_bus_updates, render_steps, CodingAgentConfig,
};

// Events
pub use events::{
    AgentEvent, EventBus, EventData, CANCELLED, DONE, ERROR, FINAL, PLAN, STEP, TEXT_DELTA,
    TOOL_CALL, TOOL_RESULT,
};

// MCP
pub use mcp::{
    discover_mcp_config, load_mcp_config, mcp_prefix, mcp_tools_to_schemas, McpHub, McpServer,
    McpServerSpec, MCPClient,
};

#[cfg(feature = "openai")]
pub use llm::{azure_from_env, openai_from_env, OpenAiLlm};
