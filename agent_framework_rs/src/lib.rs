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
pub mod coding;
pub mod events;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod planning;
pub mod skills;
pub mod subagents;
pub mod testing;
pub mod tools;

// Kern
pub use agent::{
    new_cancel, to_assistant_dict, Agent, AgentBuilder, Cancel, Strategy, PLAN_PREAMBLE,
    REACT_PREAMBLE,
};
pub use tools::{ToolFn, ToolRegistry};

// LLM
pub use llm::{Chunk, Delta, Llm, Message, ToolCallDelta};

// Memory
pub use memory::{count_tokens_text, truncate, LongTermMemory, ShortTermMemory};

// Planning
pub use planning::{Plan, Step};

// Coding-Tools
pub use coding::{coding_tools, CodingTools, CODING_SYSTEM};

// Skills
pub use skills::{parse_frontmatter, skills_tools, SkillInfo, Skills, SKILL_SYSTEM};

// Sub-Agents
pub use subagents::{add_subagent, Subagent};

// Events
pub use events::{
    AgentEvent, EventBus, EventData, CANCELLED, DONE, ERROR, FINAL, PLAN, STEP, TEXT_DELTA,
    TOOL_CALL, TOOL_RESULT,
};

// MCP
pub use mcp::{mcp_tools_to_schemas, MCPClient};

#[cfg(feature = "openai")]
pub use llm::{azure_from_env, openai_from_env, OpenAiLlm};
