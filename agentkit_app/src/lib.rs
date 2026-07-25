//! Verdrahtung — der einzige Grund, warum dieses Crate existiert.
//!
//! `agentkit-swarm` hängt von `agentkit` ab; die Executable braucht beide. Hier
//! treffen sie sich: eine Closure, die das `swarm`-Tool in die Registry des
//! Coding-Agenten registriert, plus das dazugehörige Prompt-Fragment. Beide
//! Binaries (`agentkit`, `tui`) benutzen dieselben zwei Funktionen — deshalb
//! liegen sie in einer Bibliothek und nicht doppelt in den Bins.

use agentkit::{ExtraToolCtx, ExtraTools, ToolRegistry};
use agentkit_swarm::{add_swarm_tool, SwarmLimits, SwarmToolConfig, SWARM_SYSTEM};
use std::sync::Arc;

/// Die [`ExtraTools`]-Closure für `CodingAgentConfig`/`TuiConfig`: registriert
/// das `swarm`-Tool mit den Bausteinen des gerade gebauten Coding-Agenten.
///
/// Der [`ExtraToolCtx`] liefert genau das, was der Schwarm braucht — Lauf-Kontext
/// (für Bus und Stop-Knopf), LLM, Workspace, Freigabe-Callback, Skills, Rollen
/// und den geteilten MCP-Hub. Die Schwarm-Mitglieder sind damit im selben
/// Sandbox-Workspace unterwegs wie der Orchestrator und seine Sub-Agenten.
pub fn swarm_extra_tools() -> ExtraTools {
    Arc::new(|registry: &mut ToolRegistry, ctx: &ExtraToolCtx| {
        add_swarm_tool(
            registry,
            SwarmToolConfig {
                run: ctx.run.clone(),
                llm: ctx.llm.clone(),
                workspace: ctx.workspace.to_string(),
                approve: Some(ctx.approve.clone()),
                shell_timeout: ctx.shell_timeout,
                skills: ctx.skills.cloned(),
                roles: ctx.roles.to_vec(),
                mcp: ctx.mcp.clone(),
                limits: SwarmLimits::default(),
            },
        );
    })
}

/// Hängt [`SWARM_SYSTEM`] an den agenten-spezifischen Zusatz-Prompt an, wenn das
/// `swarm`-Tool aktiv ist.
///
/// Warum über `--system` und nicht wie `SUBAGENT_SYSTEM` fest im Coding-Prompt:
/// der Agent-Kern kennt den Schwarm nicht und darf ihn nicht kennen. Der
/// Zusatz-Prompt ist die vorhandene, dafür gedachte Naht.
pub fn system_with_swarm(system: Option<&str>, swarm: bool) -> Option<String> {
    let eigen = system.map(str::trim).filter(|s| !s.is_empty());
    match (eigen, swarm) {
        (None, false) => None,
        (Some(s), false) => Some(s.to_string()),
        (None, true) => Some(SWARM_SYSTEM.to_string()),
        (Some(s), true) => Some(format!("{s}\n\n{SWARM_SYSTEM}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_with_swarm_kombiniert_beide_teile() {
        assert_eq!(system_with_swarm(None, false), None);
        assert_eq!(system_with_swarm(Some("  "), false), None);
        assert_eq!(
            system_with_swarm(Some("Sei knapp."), false).as_deref(),
            Some("Sei knapp.")
        );
        let beides = system_with_swarm(Some("Sei knapp."), true).unwrap();
        assert!(beides.starts_with("Sei knapp."));
        assert!(beides.contains("'swarm'"));
        assert_eq!(system_with_swarm(None, true).as_deref(), Some(SWARM_SYSTEM));
    }

    // Die Closure muss das Tool wirklich registrieren — sonst fällt es erst im
    // TUI auf, wo kein Test hinschaut.
    #[test]
    fn swarm_extra_tools_registriert_das_tool() {
        use agentkit::{ApproveFn, McpHub, RunHandle};

        let run = RunHandle::new();
        let llm: Arc<dyn agentkit::Llm> = Arc::new(agentkit::testing::FakeLlm::new(vec![]));
        let approve: ApproveFn = Arc::new(|_| true);
        let mcp = Arc::new(McpHub::empty());
        let dir = std::env::temp_dir().join(format!("agentkit_app_{}", std::process::id()));

        let mut reg = ToolRegistry::new();
        swarm_extra_tools()(
            &mut reg,
            &ExtraToolCtx {
                run: &run,
                llm: &llm,
                approve: &approve,
                mcp: &mcp,
                workspace: dir.to_str().unwrap(),
                skills: None,
                roles: &[],
                shell_timeout: 120,
            },
        );
        assert!(reg.has("swarm"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
