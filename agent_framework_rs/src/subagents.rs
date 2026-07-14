//! Sub-Agents — ein Agent als Tool eines anderen.
//!
//! Ein **Orchestrator** bekommt ein `delegate`-Tool (Name frei wählbar). Jeder
//! Aufruf baut über den normalen [`AgentBuilder`](crate::AgentBuilder) einen
//! FRISCHEN [`Agent`] — eigener Kontext, eigene Tools — führt ihn für den
//! Teilauftrag aus und gibt nur dessen **Ergebnis** zurück: Kontext-Isolation +
//! Spezialisierung. Zusammen mit `parallel_tools` laufen mehrere `delegate`-Aufrufe
//! aus EINER Antwort nebenläufig.
//!
//! Es gibt hier bewusst **keinen eigenen „Sub-Agent"-Typ**: ein Sub-Agent IST ein
//! [`Agent`], nur als Tool registriert. Diese Datei ist deshalb nur die dünne
//! Registrierung; Bau (über [`AgentBuilder`](crate::AgentBuilder)) und Lauf (über
//! [`Agent::run_as_subagent`]) teilt sie sich mit jedem anderen Agenten. Genau wie
//! Pythons `add_subagent` ist das eine Funktion, keine Klasse.

use crate::agent::{Agent, Strategy};
use crate::events::EventBus;
use crate::llm::Llm;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

/// Registriert einen Sub-Agenten als Tool `name` im `registry` des Orchestrators.
///
/// Jeder Aufruf erzeugt einen FRISCHEN [`Agent`] (eigenes Kurzzeitgedächtnis) und
/// gibt dessen finale Antwort als Text zurück. Thread-safe, weil jeder Sub-Agent
/// eigenen State hat und der LLM-Client `Send + Sync` ist.
///
/// Wird ein `bus` übergeben, laufen ALLE Events des Sub-Agenten dorthin — getaggt
/// mit `source="<name>:<auftrag>"` (siehe [`Agent::run_as_subagent`]), damit
/// Consumer die (auch parallel laufenden) Sub-Agenten auseinanderhalten können.
///
/// Entspricht Pythons `add_subagent`: eine Funktion, kein eigener Builder-Typ.
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
    let sub_name = name.to_string();
    let system = system.map(|s| s.to_string());
    let tools = tools.unwrap_or_default();
    let param_name = "auftrag";

    let params = json!({
        "type": "object",
        "properties": {param_name: {"type": "string", "description": "Der Teilauftrag in Worten."}},
        "required": [param_name],
    });

    registry.add(name, description, params, move |args: Value| {
        let task = args
            .get(param_name)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Ein Sub-Agent ist ein ganz normaler Agent — gebaut über denselben Builder.
        let mut builder = Agent::builder(llm.clone())
            .tools(tools.clone())
            .strategy(strategy);
        if let Some(s) = &system {
            builder = builder.system(s);
        }
        let mut agent = builder.build();

        Ok(agent.run_as_subagent(&task, &sub_name, bus.as_ref(), None))
    });
}
