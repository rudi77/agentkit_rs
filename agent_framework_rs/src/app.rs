//! Gemeinsame Anwendungs-Bausteine für die Frontends (`bin/agentkit`, `bin/tui`).
//!
//! Beide Frontends bauen denselben vollen Coding-Agenten und wollen dieselben
//! Hilfen — `.env` laden, den Plan rendern, PLAN-Events auf den aktiven Bus schicken.
//! Damit das nicht doppelt gepflegt werden muss, liegt es hier. Der einzige echte
//! Unterschied zwischen den Frontends ist der **Freigabe-Callback** (`approve`): das
//! CLI fragt über stdin, das TUI über einen Dialog — er wird daher hereingereicht.

use std::sync::Arc;

use crate::coding::{ApproveFn, CodingTools};
use crate::events::{AgentEvent, EventData};
use crate::llm::Llm;
use crate::planning::Step;
use crate::roles::{add_task_tool, builtin_roles, load_roles_from_dir, merge_roles, AgentRole};
use crate::{
    Agent, LongTermMemory, Plan, RunHandle, Skills, Strategy, ToolRegistry, CODING_SYSTEM, PLAN,
    SKILL_SYSTEM, SUBAGENT_SYSTEM,
};

/// Plattform-Hinweis für `run_shell`, an den Coding-System-Prompt angehängt — so
/// fummelt das Modell nicht erst mit der falschen Shell-Syntax (Bash-Heredocs auf
/// Windows etc.) herum.
#[cfg(windows)]
const SHELL_HINT: &str = "\n\nrun_shell nutzt PowerShell (Windows): verwende \
PowerShell-Syntax, KEINE Bash-Heredocs (`<<'EOF'`). Mehrzeilige Skripte am besten \
mit write_file in eine Datei schreiben und dann ausführen (z. B. `python script.py`).";

#[cfg(not(windows))]
const SHELL_HINT: &str = "\n\nrun_shell nutzt bash.";

/// Lädt eine `.env` aus dem aktuellen Verzeichnis — nur Variablen, die noch nicht
/// gesetzt sind (wie die Python-CLI, ohne zusätzliche Abhängigkeit).
pub fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if std::env::var(k).is_err() {
                std::env::set_var(k, v);
            }
        }
    }
}

/// Plan-Schritte rendern: `[ ]/[~]/[x] N. Beschreibung`, mit `sep` verbunden
/// (CLI nutzt `"\n"`, das TUI `"  "` für eine Zeile).
pub fn render_steps(steps: &[Step], sep: &str) -> String {
    if steps.is_empty() {
        return "(noch kein Plan)".to_string();
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mark = match s.status.as_str() {
                "in_progress" => "[~]",
                "done" => "[x]",
                _ => "[ ]",
            };
            format!("{mark} {}. {}", i + 1, s.step)
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// Ein [`Plan`], der nach jeder Aktualisierung ein PLAN-Event auf den jeweils
/// aktiven Bus des [`RunHandle`] schickt — die kanonische Verdrahtung beider Frontends.
pub fn plan_with_bus_updates(run: &RunHandle, sep: &str) -> Plan {
    let run = run.clone();
    let sep = sep.to_string();
    Plan::with_on_update(move |steps| {
        if let Some(bus) = run.bus() {
            bus.publish(AgentEvent::new(PLAN, EventData::Plan(render_steps(steps, &sep))));
        }
    })
}

/// Konfiguration des vollen Coding-Agenten (gemeinsam von CLI und TUI befüllt).
pub struct CodingAgentConfig<'a> {
    pub workspace: &'a str,
    pub strategy: Strategy,
    pub max_steps: usize,
    pub skills: Option<&'a str>,
    pub agents: Option<&'a str>,
    pub memory: Option<&'a str>,
    pub subagents: bool,
    /// Trennzeichen fürs PLAN-Event-Rendering (`"\n"` CLI, `"  "` TUI).
    pub plan_sep: &'a str,
}

/// Baut den vollen Coding-Agenten: Sandbox-Tools (inkl. glob/grep), optional Skills
/// und Langzeitgedächtnis, Plan (mit PLAN-Events) sowie Rollen + `task`-Tool.
///
/// `approve` ist der frontend-spezifische Freigabe-Callback (stdin bzw. TUI-Dialog);
/// die Coding-Tools fragen ihn IMMER (`approval = true`) — die Policy (nachfragen,
/// auto, `--yes`) steckt im Callback selbst. Gibt neben dem Agenten Plan, Skills und
/// die aktiven Rollen zurück (für Slash-Befehle wie `/plan`, `/skills`, `/agents`).
pub fn build_coding_agent(
    llm: Arc<dyn Llm>,
    cfg: &CodingAgentConfig,
    approve: ApproveFn,
) -> (Agent, Plan, Option<Skills>, Vec<AgentRole>) {
    let run = RunHandle::new();

    let mut tools = ToolRegistry::new();
    CodingTools::with_approve(cfg.workspace, true, approve.clone(), 120).register(&mut tools, None);

    let skills = cfg.skills.map(Skills::new);
    let long_term = cfg.memory.map(LongTermMemory::new);

    let mut system = String::from(CODING_SYSTEM);
    system.push_str(SHELL_HINT);
    if skills.is_some() {
        system.push_str("\n\n");
        system.push_str(SKILL_SYSTEM);
    }
    if cfg.subagents {
        system.push_str("\n\n");
        system.push_str(SUBAGENT_SYSTEM);
    }

    let mut roles = builtin_roles();
    if let Some(dir) = cfg.agents {
        roles = merge_roles(roles, load_roles_from_dir(dir));
    }

    let plan = plan_with_bus_updates(&run, cfg.plan_sep);

    // `task`-Tool VOR dem Build registrieren (die Registry wird beim Build kopiert);
    // es teilt sich über `run` den Lauf-Kontext mit dem fertigen Agenten.
    if cfg.subagents {
        add_task_tool(
            &mut tools,
            run.clone(),
            llm.clone(),
            cfg.workspace,
            true,
            Some(approve),
            roles.clone(),
        );
    }

    let mut builder = Agent::builder(llm)
        .tools(tools)
        .system(&system)
        .strategy(cfg.strategy)
        .plan(plan.clone())
        .max_steps(cfg.max_steps)
        // Großzügiges Kontext-Budget: moderne Azure/OpenAI-Modelle haben großen Kontext,
        // und die frühe (verlustbehaftete) Kompaktierung bei 8000 würde mitten in einer
        // Coding-Sitzung wertvollen Verlauf zusammenfalten.
        .token_budget(100_000)
        .run_handle(run);
    if let Some(s) = skills.clone() {
        builder = builder.skills(s);
    }
    if let Some(lt) = long_term {
        builder = builder.long_term(lt);
    }
    (builder.build(), plan, skills, roles)
}
