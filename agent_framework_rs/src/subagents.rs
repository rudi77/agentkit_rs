//! Sub-Agents — ein Agent als Tool eines anderen.
//!
//! Ein **Orchestrator** bekommt ein `delegate`-Tool. Jeder Aufruf startet einen
//! eigenständigen Agent-Loop (eigener Kontext, eigene Tools) und gibt nur das
//! **Ergebnis** zurück — Kontext-Isolation + Spezialisierung. Zusammen mit
//! `parallel_tools` laufen mehrere `delegate`-Aufrufe aus EINER Antwort nebenläufig.

use crate::agent::{Agent, Strategy};
use crate::events::EventBus;
use crate::llm::Llm;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

/// Konfiguration eines Sub-Agenten. Sinnvolle Defaults wie im Python-Port;
/// per Buildermethoden anpassbar.
pub struct Subagent {
    llm: Arc<dyn Llm>,
    tools: ToolRegistry,
    system: Option<String>,
    strategy: Strategy,
    max_steps: usize,
    param_name: String,
    param_desc: String,
    parallel_tools: bool,
    bus: Option<EventBus>,
}

impl Subagent {
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        Subagent {
            llm,
            tools: ToolRegistry::new(),
            system: None,
            strategy: Strategy::React,
            max_steps: 12,
            param_name: "auftrag".to_string(),
            param_desc: "Der Teilauftrag in Worten.".to_string(),
            parallel_tools: true,
            bus: None,
        }
    }

    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub fn system(mut self, system: &str) -> Self {
        self.system = Some(system.to_string());
        self
    }
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = n;
        self
    }
    pub fn param(mut self, name: &str, description: &str) -> Self {
        self.param_name = name.to_string();
        self.param_desc = description.to_string();
        self
    }
    pub fn parallel_tools(mut self, on: bool) -> Self {
        self.parallel_tools = on;
        self
    }
    /// Event-Forwarding: ALLE Sub-Agent-Events laufen in diesen geteilten Bus,
    /// getaggt mit `source="<name>:<auftrag>"`.
    pub fn bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Registriert den Sub-Agenten als Tool `name` im `registry` des Orchestrators.
    /// Jeder Aufruf erzeugt einen FRISCHEN Agent (eigenes Kurzzeitgedächtnis).
    pub fn register(self, registry: &mut ToolRegistry, name: &str, description: &str) {
        let Subagent {
            llm,
            tools,
            system,
            strategy,
            max_steps,
            param_name,
            param_desc,
            parallel_tools,
            bus,
        } = self;

        let tool_name = name.to_string();
        let pname = param_name.clone();
        let key = param_name.clone();

        let params = json!({
            "type": "object",
            "properties": {key: {"type": "string", "description": param_desc}},
            "required": [param_name],
        });

        registry.add(name, description, params, move |args: Value| {
            let task = args
                .get(&pname)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let mut builder = Agent::builder(llm.clone())
                .tools(tools.clone())
                .strategy(strategy)
                .max_steps(max_steps)
                .parallel_tools(parallel_tools);
            if let Some(s) = &system {
                builder = builder.system(s);
            }
            let mut agent = builder.build();

            match &bus {
                None => Ok(agent.run(&task)),
                Some(bus) => {
                    let label: String = task
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(24)
                        .collect();
                    let source = format!("{tool_name}:{label}");
                    Ok(agent.run_on_bus(&task, bus, -1, None, &source))
                }
            }
        });
    }
}

/// Bequemer Helfer mit Defaults (entspricht Pythons `add_subagent`).
#[allow(clippy::too_many_arguments)]
pub fn add_subagent(
    registry: &mut ToolRegistry,
    name: &str,
    description: &str,
    llm: Arc<dyn Llm>,
    tools: Option<ToolRegistry>,
    system: Option<&str>,
    strategy: Strategy,
    bus: Option<EventBus>,
) {
    let mut sub = Subagent::new(llm).strategy(strategy);
    if let Some(t) = tools {
        sub = sub.tools(t);
    }
    if let Some(s) = system {
        sub = sub.system(s);
    }
    if let Some(b) = bus {
        sub = sub.bus(b);
    }
    sub.register(registry, name, description);
}
