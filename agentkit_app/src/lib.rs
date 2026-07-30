//! Verdrahtung — der einzige Grund, warum dieses Crate existiert.
//!
//! `agentkit-swarm` und `agentkit-graph` hängen beide von `agentkit` ab; die
//! Executable braucht alle drei. Hier treffen sie sich: Closures, die das
//! `swarm`-Tool und die Graph-Tools in die Registry des Coding-Agenten
//! registrieren, plus die dazugehörigen Prompt-Fragmente. Beide Binaries
//! (`agentkit`, `tui`) benutzen dieselben Funktionen — deshalb liegen sie in
//! einer Bibliothek und nicht doppelt in den Bins.

use agentkit::{ExtraToolCtx, ExtraTools, ToolRegistry};
use agentkit_swarm::{add_swarm_tool, SwarmLimits, SwarmToolConfig, SWARM_SYSTEM};
use std::sync::Arc;

#[cfg(feature = "graph")]
pub use agentkit_graph::{GraphAccess, GraphStore};

/// Die [`ExtraTools`]-Closure für `CodingAgentConfig`/`TuiConfig`: registriert
/// das `swarm`-Tool mit den Bausteinen des gerade gebauten Coding-Agenten.
///
/// Der [`ExtraToolCtx`] liefert genau das, was der Schwarm braucht — Lauf-Kontext
/// (für Bus und Stop-Knopf), LLM, Workspace, Freigabe-Callback, Skills, Rollen
/// und den geteilten MCP-Hub. Die Schwarm-Mitglieder sind damit im selben
/// Sandbox-Workspace unterwegs wie der Orchestrator und seine Sub-Agenten.
pub fn swarm_extra_tools() -> ExtraTools {
    // Ohne Feature `graph` hat FrontendTools nur ein Feld — das Update ist dann leer.
    #[cfg_attr(not(feature = "graph"), allow(clippy::needless_update))]
    FrontendTools {
        swarm: true,
        ..Default::default()
    }
    .build()
    .expect("swarm = true liefert immer Tools")
}

/// Hängt [`SWARM_SYSTEM`] an den agenten-spezifischen Zusatz-Prompt an, wenn das
/// `swarm`-Tool aktiv ist.
///
/// Warum über `--system` und nicht wie `SUBAGENT_SYSTEM` fest im Coding-Prompt:
/// der Agent-Kern kennt den Schwarm nicht und darf ihn nicht kennen. Der
/// Zusatz-Prompt ist die vorhandene, dafür gedachte Naht.
pub fn system_with_swarm(system: Option<&str>, swarm: bool) -> Option<String> {
    system_with_extras(system, swarm, false)
}

/// Der geöffnete Wissensgraph plus der Zugriff, unter dem dieser Prozess ihn
/// benutzt (Feature `graph`, CLI `--graph DIR`).
#[cfg(feature = "graph")]
pub struct GraphSetup {
    pub store: Arc<GraphStore>,
    pub access: GraphAccess,
}

/// Öffnet den Graphen in `dir` und baut den Laufzeit-Zugriff.
///
/// Die Identität des kanonischen Wissens hängt am **Workspace-Pfad** (absolut,
/// soweit auflösbar): derselbe Checkout findet sein Wissen wieder, ein anderer
/// bekommt seinen eigenen Scope. Der vorläufige Arbeits-Scope hängt dagegen an
/// `run_id` — bei `--session FILE` also an der Session, sodass ein
/// wiederaufgenommener Lauf seinen Arbeitsstand behält.
#[cfg(feature = "graph")]
pub fn open_graph(
    dir: &str,
    workspace: &str,
    run_id: &str,
    read_only: bool,
) -> Result<GraphSetup, String> {
    let store = GraphStore::open(dir).map_err(|e| e.to_string())?;
    let workspace_id = std::fs::canonicalize(workspace)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| workspace.to_string());
    let access = if read_only {
        let view = agentkit_graph::GraphView::new(vec![agentkit_graph::GraphTarget::canonical(
            agentkit_graph::GraphScope::workspace(&workspace_id),
        )]);
        GraphAccess::read_only("agentkit", view)
    } else {
        GraphAccess::session("agentkit", &workspace_id, run_id)
    };
    Ok(GraphSetup {
        store: Arc::new(store),
        access,
    })
}

/// Was das Frontend an eigenen Fähigkeiten mitbringt. Ein Bündel statt mehrerer
/// `Option`-Parameter, damit die Aufrufstellen in beiden Binaries gleich aussehen
/// und ein deaktiviertes Feature nur ein fehlendes Feld ist.
#[derive(Default)]
pub struct FrontendTools {
    /// Das `swarm`-Tool aus agentkit-swarm.
    pub swarm: bool,
    /// Die Graph-Tools aus agentkit-graph (`--graph DIR`).
    #[cfg(feature = "graph")]
    pub graph: Option<GraphSetup>,
}

impl FrontendTools {
    /// Die kombinierte [`ExtraTools`]-Closure — `None`, wenn nichts anliegt
    /// (dann baut `build_coding_agent` exakt wie vorher).
    pub fn build(self) -> Option<ExtraTools> {
        let swarm = self.swarm;
        #[cfg(feature = "graph")]
        let graph = self.graph;
        #[cfg(feature = "graph")]
        let etwas = swarm || graph.is_some();
        #[cfg(not(feature = "graph"))]
        let etwas = swarm;
        if !etwas {
            return None;
        }
        Some(Arc::new(
            move |registry: &mut ToolRegistry, ctx: &ExtraToolCtx| {
                // Schwarm-Mitglieder bauen ihre Registry selbst und erben deshalb
                // nichts vom Orchestrator — der Graph muss ihnen ausdrücklich
                // mitgegeben werden, sonst hätte der Erzeuger eines Schwarms ein
                // Gedächtnis, das seine Mitglieder nicht haben.
                #[cfg(feature = "graph")]
                let extra_member_tools: Option<agentkit_swarm::ExtraMemberTools> =
                    graph.as_ref().map(|setup| {
                        let store = setup.store.clone();
                        let access = setup.access.clone();
                        Arc::new(move |reg: &mut ToolRegistry, id: &str| {
                            // Gleicher Scope, eigener Autor: geteiltes Gedächtnis
                            // mit ehrlicher Provenance.
                            agentkit_graph::register_graph_tools(
                                reg,
                                store.clone(),
                                access.as_principal(id),
                            );
                        }) as agentkit_swarm::ExtraMemberTools
                    });
                #[cfg(not(feature = "graph"))]
                let extra_member_tools = None;

                if swarm {
                    add_swarm_tool(
                        registry,
                        SwarmToolConfig {
                            run: ctx.run.clone(),
                            llm: ctx.llm.clone(),
                            coding: ctx.coding.clone(),
                            skills: ctx.skills.cloned(),
                            roles: ctx.roles.to_vec(),
                            mcp: ctx.mcp.clone(),
                            dry_run: ctx.dry_run,
                            limits: SwarmLimits::default(),
                            helper_ctx_budget: ctx.helper_ctx_budget,
                            extra_member_tools,
                        },
                    );
                }
                #[cfg(feature = "graph")]
                if let Some(setup) = &graph {
                    // Dieselbe Naht wie beim Schwarm: Tools in die vorhandene
                    // Registry, kein Eingriff in den Agent-Loop.
                    agentkit_graph::register_graph_tools(
                        registry,
                        setup.store.clone(),
                        setup.access.clone(),
                    );
                }
            },
        ))
    }
}

/// Hängt die Prompt-Fragmente der aktiven Frontend-Tools an den
/// agenten-spezifischen Zusatz-Prompt an.
///
/// Warum über `--system` und nicht fest im Coding-Prompt: der Agent-Kern kennt
/// weder Schwarm noch Graph und darf sie nicht kennen. Der Zusatz-Prompt ist die
/// vorhandene, dafür gedachte Naht.
pub fn system_with_extras(system: Option<&str>, swarm: bool, graph: bool) -> Option<String> {
    let mut teile: Vec<String> = Vec::new();
    if let Some(eigen) = system.map(str::trim).filter(|s| !s.is_empty()) {
        teile.push(eigen.to_string());
    }
    if swarm {
        teile.push(SWARM_SYSTEM.to_string());
    }
    #[cfg(feature = "graph")]
    if graph {
        teile.push(agentkit_graph::GRAPH_SYSTEM.to_string());
    }
    #[cfg(not(feature = "graph"))]
    let _ = graph;
    if teile.is_empty() {
        None
    } else {
        Some(teile.join("\n\n"))
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

    /// Baut eine Registry über die [`ExtraTools`]-Closure — derselbe Weg, den
    /// `build_coding_agent` geht.
    fn registry_via(extra: ExtraTools, name: &str) -> (ToolRegistry, std::path::PathBuf) {
        use agentkit::coding::CodingTools;
        use agentkit::{McpHub, RunHandle};

        let run = RunHandle::new();
        let llm: Arc<dyn agentkit::Llm> = Arc::new(agentkit::testing::FakeLlm::new(vec![]));
        let mcp = Arc::new(McpHub::empty());
        let dir = std::env::temp_dir().join(format!("agentkit_app_{name}_{}", std::process::id()));
        let coding = CodingTools::new(dir.to_str().unwrap(), false);

        let mut reg = ToolRegistry::new();
        extra(
            &mut reg,
            &ExtraToolCtx {
                run: &run,
                llm: &llm,
                coding: &coding,
                mcp: &mcp,
                skills: None,
                roles: &[],
                dry_run: false,
                helper_ctx_budget: None,
            },
        );
        (reg, dir)
    }

    // Die Closure muss das Tool wirklich registrieren — sonst fällt es erst im
    // TUI auf, wo kein Test hinschaut.
    #[test]
    fn swarm_extra_tools_registriert_das_tool() {
        let (reg, dir) = registry_via(swarm_extra_tools(), "swarm");
        assert!(reg.has("swarm"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ohne_faehigkeiten_gibt_es_keine_closure() {
        assert!(FrontendTools::default().build().is_none());
    }

    #[cfg(feature = "graph")]
    #[test]
    fn graph_tools_landen_neben_dem_swarm_tool() {
        let store = Arc::new(GraphStore::in_memory());
        let access = GraphAccess::session("agentkit", "ws", "run-1");
        let extra = FrontendTools {
            swarm: true,
            graph: Some(GraphSetup { store, access }),
        }
        .build()
        .expect("Tools angefordert");

        let (reg, dir) = registry_via(extra, "graph");
        assert!(reg.has("swarm"));
        assert!(reg.has("graph_search"));
        assert!(reg.has("graph_remember"));
        assert!(reg.has("graph_promote"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "graph")]
    #[test]
    fn graph_readonly_registriert_keine_schreib_tools() {
        let dir = std::env::temp_dir().join(format!("agentkit_app_ro_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let setup = open_graph(dir.to_str().unwrap(), ".", "run-1", true).expect("Graph offen");
        // Ohne Schreibziel gibt es die Schreib-Tools gar nicht.
        assert!(!setup.access.can_write());
        assert!(!setup.access.can_promote());

        let extra = FrontendTools {
            swarm: false,
            graph: Some(setup),
        }
        .build()
        .expect("Tools angefordert");
        let (reg, ws) = registry_via(extra, "ro_ws");
        assert!(reg.has("graph_search"));
        assert!(!reg.has("graph_remember"));
        assert!(!reg.has("graph_promote"));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Der Prompt-Zusatz muss beide Fragmente tragen — sonst weiß das Modell
    /// nicht, dass es einen Graphen hat.
    #[test]
    fn system_with_extras_haengt_beide_fragmente_an() {
        assert_eq!(system_with_extras(None, false, false), None);
        let nur_swarm = system_with_extras(Some("Sei knapp."), true, false).unwrap();
        assert!(nur_swarm.starts_with("Sei knapp."));
        assert!(nur_swarm.contains("'swarm'"));
        #[cfg(feature = "graph")]
        {
            let beides = system_with_extras(Some("Sei knapp."), true, true).unwrap();
            assert!(beides.contains("'swarm'"));
            assert!(beides.contains("graph_search"));
            let nur_graph = system_with_extras(None, false, true).unwrap();
            assert!(!nur_graph.contains("'swarm'"));
            assert!(nur_graph.contains("Wissensgraph"));
        }
    }
}
