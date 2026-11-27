//! agentkit TUI — dünner Wrapper um [`agentkit::tui::run`].
//!
//! Die eigentliche UI-Logik lebt im Library-Modul `agentkit::tui` (Feature `tui`),
//! damit sie sowohl von diesem Binary als auch von der Haupt-Executable `agentkit`
//! (`agentkit --tui`) genutzt werden kann.
//!
//! ```bash
//! cargo run --bin tui --features tui                       # mit Azure/OpenAI (Default)
//! cargo run --bin tui --no-default-features --features tui  # nur Demo-Modus (kein Netz)
//! cargo run --bin tui --features tui -- --demo             # Demo-Modus erzwingen
//! ```

use agentkit::tui::TuiConfig;
use agentkit::{load_dotenv, load_user_config, PlanExecuteParams, RunStrategy, Strategy};
use agentkit_app::packages;

/// Die Optionen dieses Binaries (siehe [`print_help`]) — mit und ohne Wert.
const OPTIONEN_MIT_WERT: &[&str] = &[
    "-w",
    "--mode",
    "--workspace",
    "--skills",
    "--agents",
    "--sub-rules",
    "--protect-paths",
    "--memory",
    "--max-steps",
    "--mcp-config",
    "--mcp",
    "--system",
    "--system-file",
    "--ctx",
    "--ctx-budget",
    "--ctx-policy",
    "--ctx-compaction-model",
    "--graph",
];
const OPTIONEN_OHNE_WERT: &[&str] = &[
    "--demo",
    "--agents-only",
    "--no-subagents",
    "--no-swarm",
    "--no-project-instructions",
    "-y",
    "--yes",
    "--plan",
    "--plain",
    "--plan-execute",
    "--no-mcp",
    "--graph-readonly",
    "--no-graph",
    "--no-ctx",
    "-h",
    "--help",
];

fn main() -> std::io::Result<()> {
    let roh: Vec<String> = std::env::args().skip(1).collect();
    if roh.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }
    // Ein Tippfehler wie `--no-swam` liefe sonst still MIT Schwarm; und erst
    // die Normalisierung lässt `--workspace=DIR` beim Nachschlagen finden.
    let args = match agentkit_app::optionen::pruefe_optionen(
        &roh,
        OPTIONEN_MIT_WERT,
        OPTIONEN_OHNE_WERT,
    ) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("[ERROR] {e}\n        Alle Optionen: tui --help");
            std::process::exit(agentkit::ExitCode::ContextError.code());
        }
    };
    let has = |flag: &str| args.iter().any(|a| a == flag);
    // Wert eines Flags lesen (z. B. `--workspace <DIR>`).
    let val = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };

    // Ein Tippfehler im Modus liefe sonst still mit Rückfrage statt im Plan.
    if let Some(m) = val("--mode") {
        if agentkit::SessionMode::parse(&m).is_none() {
            eprintln!("[ERROR] --mode '{m}' unbekannt (nachfragen | auto | plan)");
            std::process::exit(agentkit::ExitCode::ContextError.code());
        }
    }
    let mode = val("--mode").and_then(|m| agentkit::SessionMode::parse(&m));

    // Rangfolge wie im CLI: echte Umgebung > `.env` im CWD > `~/.agentkit/config.json`.
    load_dotenv();
    load_user_config();

    let strategy = if has("--plan") {
        Strategy::Plan
    } else if has("--plain") {
        Strategy::Plain
    } else {
        Strategy::React
    };

    // `--mcp <name>` kann mehrfach auftreten -> alle Werte einsammeln.
    let mcp_enable: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--mcp")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();

    let workspace = val("-w")
        .or_else(|| val("--workspace"))
        .unwrap_or_else(|| ".".into());

    // Frontend-eigene Tools: Schwarm immer (außer --no-swarm), Graph nur mit
    // `--graph DIR` und nur, wenn das Binary das Feature mitbringt.
    #[allow(unused_mut)]
    #[cfg_attr(not(feature = "graph"), allow(clippy::needless_update))]
    let mut extras = agentkit_app::FrontendTools {
        swarm: !has("--no-swarm"),
        ..Default::default()
    };
    // Dasselbe Gedächtnis wie im CLI: ohne Flag am Default-Ort, `--graph DIR`
    // für einen anderen, `--no-graph` für keines. Die Vertrauensfrage zu einem
    // fremden Bundle kommt hier, BEVOR das TUI den Bildschirm übernimmt.
    #[cfg(feature = "graph")]
    let graph_wahl = (!has("--no-graph"))
        .then(|| {
            use std::io::IsTerminal;
            agentkit_app::choose_graph(
                val("--graph").as_deref(),
                &workspace,
                std::io::stdin().is_terminal(),
            )
        })
        .flatten();
    #[cfg(feature = "graph")]
    if let Some(wahl) = &graph_wahl {
        let run_id = format!("pid-{}", std::process::id());
        match agentkit_app::open_graph(
            &wahl.dir,
            &run_id,
            has("--graph-readonly"),
            wahl.owner_key.as_deref(),
        ) {
            Ok(setup) => extras.graph = Some(setup),
            Err(e) => {
                eprintln!("[FEHLER] --graph: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "graph"))]
    if val("--graph").is_some() {
        eprintln!("[WARN] --graph ignoriert — Binary ohne Feature `graph` gebaut.");
    }
    #[cfg(feature = "graph")]
    let (graph_aktiv, turn_context, extra_commands) = (
        extras.graph.is_some(),
        extras.graph.as_ref().map(agentkit_app::graph_turn_context),
        graph_wahl
            .iter()
            .map(|w| agentkit_app::graph_tui_command(&w.dir))
            .collect::<Vec<_>>(),
    );
    #[cfg(not(feature = "graph"))]
    let (graph_aktiv, turn_context, extra_commands) = (false, None, Vec::new());

    // Fakten des Kontext-Managements gehen als Episoden in den Graphen; am
    // Sitzungsende werden alte Arbeits-Scopes weggeräumt. Die Konsolidierung
    // (`--consolidate`) gibt es nur im `agentkit`-Binary.
    #[cfg(feature = "graph")]
    let (fact_listener, on_exit) = match extras.graph.clone() {
        Some(setup) => {
            let ende = setup.clone();
            let on_exit: agentkit::SessionEndFn = std::sync::Arc::new(move |agent| {
                for note in agentkit_app::graph_session_end(&ende, &agent.memory.messages, None) {
                    eprintln!("» {note}");
                }
            });
            (agentkit_app::graph_fact_listener(&setup), Some(on_exit))
        }
        None => (None, None),
    };
    #[cfg(not(feature = "graph"))]
    let (fact_listener, on_exit) = (None, None);

    let ctx_gewollt = cfg!(feature = "ctxman")
        && !has("--no-ctx")
        && agentkit_app::config_bool("ctx") != Some(false);
    let ctx_dir = val("--ctx").or_else(|| {
        let sitzung = val("--session").filter(|_| ctx_gewollt)?;
        let dir = agentkit::context_dir_for(std::path::Path::new(&sitzung))?;
        Some(dir.to_string_lossy().to_string())
    });
    let ctx_ephemeral = ctx_gewollt && ctx_dir.is_none();
    let config_dir = agentkit::config_dir();

    let cfg = TuiConfig {
        strategy,
        turn_context,
        extra_commands,
        fact_listener,
        on_exit,
        run_strategy: if mode == Some(agentkit::SessionMode::Plan) {
            agentkit::run_strategy_from_str("planning")
        } else if has("--plan-execute") {
            RunStrategy::PlanExecute(PlanExecuteParams::default())
        } else {
            RunStrategy::Direct(strategy)
        },
        force_demo: has("--demo"),
        workspace,
        // Installierte Skills und Plugins wie im `agentkit`-Binary.
        skills: packages::skills_arg(val("--skills").as_deref(), config_dir.as_deref()),
        agents: if has("--agents-only") {
            val("--agents")
        } else {
            packages::agents_arg(val("--agents").as_deref(), config_dir.as_deref())
        },
        agents_only: has("--agents-only"),
        sub_rules: val("--sub-rules"),
        // Wie im CLI kommagetrennt (siehe `--protect-paths` in bin/agentkit.rs).
        protect_paths: val("--protect-paths")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        // Für dieses Binary (noch) keine eigene CLI-Option — Feld existiert nur,
        // damit `TuiConfig` (Pflichtfeld, kein `Default` hier verwendet) kompiliert.
        allow_read: Vec::new(),
        add_dirs: Vec::new(),
        workspace_name: None,
        memory: val("--memory"),
        subagents: !has("--no-subagents"),
        max_steps: val("--max-steps")
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
        ask_approval: !(has("-y") || has("--yes") || mode == Some(agentkit::SessionMode::Auto)),
        mcp_config: val("--mcp-config"),
        mcp_enable,
        no_mcp: has("--no-mcp"),
        system: val("--system-file")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .or_else(|| val("--system")),
        tool_system: agentkit_app::tool_system(!has("--no-swarm"), graph_aktiv),
        // Ohne `--ctx` ist das Kontext-Management trotzdem an, außer mit
        // `--no-ctx` oder `"ctx": false` in der Config: mit Sitzung liegt sein
        // Zustand neben ihr, ohne Sitzung lebt es nur für diesen Lauf.
        ctx: ctx_dir,
        ctx_ephemeral,
        ctx_budget: val("--ctx-budget")
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000),
        ctx_policy: val("--ctx-policy"),
        ctx_compaction_model: val("--ctx-compaction-model"),
        extra_tools: extras.build(),
        session: None,
        project_instructions: !has("--no-project-instructions"),
        // Mitschnitt für `agentkit viz` gibt es über `agentkit --tui --trace DIR`.
        trace: None,
    };

    agentkit::tui::run(cfg)
}

fn print_help() {
    println!(
        "agentkit TUI — interaktives Terminal-UI für den Rust-Agenten\n\n\
         AUFRUF:\n  \
           cargo run --bin tui --features tui [-- OPTIONEN]\n  \
           agentkit --tui [OPTIONEN]   (Haupt-Executable, mit Feature `tui` gebaut)\n\n\
         OPTIONEN:\n  \
           --demo            Demo-Modus erzwingen (eingebauter, netzfreier LLM)\n  \
           -w, --workspace D Sandbox-/Arbeitsverzeichnis (Default: .)\n  \
           --skills DIR      Skills-Verzeichnis aktivieren\n  \
           --agents DIR      Custom-Sub-Agenten aus *.md laden\n  \
           --agents-only     geladene Rollen ERSETZEN die eingebauten\n  \
           --sub-rules TEXT  Laufregeln für jeden Sub-Agenten\n  \
           --protect-paths M schreibgeschützte Pfadmuster (kommagetrennt)\n  \
           --memory FILE     Langzeitgedächtnis (JSONL)\n  \
           --no-subagents    das 'task'-Tool deaktivieren\n  \
           --no-swarm        das 'swarm'-Tool (dynamische Agenten-Schwärme) deaktivieren\n  \
           --no-graph        kein Gedächtnis in dieser Sitzung (sonst: <workspace>/.agentkit/graph)\n  \
           --no-ctx          kein Kontext-Management (sonst an; mit --session fortsetzbar)\n  \
           --no-project-instructions  AGENTS.md nicht laden (weder Prompt noch Leitplanken)\n  \
           --max-steps N     Max. Loop-Schritte (Default: 600)\n  \
           -y, --yes         Shell-Freigabe initial auf AUTO\n  \
           --plan / --plain  Strategie statt ReAct\n  \
           --mode M          Startmodus: nachfragen | auto | plan (plan = erst planen, dann freigeben)\n  \
           --plan-execute    erst planen, dann je Schritt ein ReAct-Durchlauf\n  \
           --mcp-config FILE MCP-Server aus .mcp.json (sonst Auto-Discovery)\n  \
           --mcp NAME        nur diesen MCP-Server initial aktiv (mehrfach möglich)\n  \
           --no-mcp          MCP komplett deaktivieren\n  \
           --system TEXT     agenten-spezifischer Zusatz-System-Prompt\n  \
           --system-file F   System-Prompt aus Datei (überschreibt --system)\n  \
           --ctx DIR         ctxman-Kontext-Management aktivieren (Feature `ctxman`):\n  \
                             Snapshot-Resume, GC, Externalisierung; /context zeigt\n  \
                             dann die Segment-Statistik\n  \
           --ctx-budget N    Kontext-Budget in Tokens für --ctx (Default: 100000)\n  \
           --ctx-policy F    partielles Policy-Overlay (JSON): Watermarks, kinds-TTLs,\n  \
                             tokenizer (heuristic|o200k|cl100k), max_share, …\n  \
           --ctx-compaction-model NAME  separates LLM nur für Compaction\n  \
           --graph DIR       Wissensgraph in DIR aktivieren (Feature `graph`)\n  \
           --graph-readonly  Graph nur lesen (kein graph_remember/graph_promote)\n  \
           -h, --help        Diese Hilfe\n\n\
         TASTEN (im UI):\n  \
           Enter      Auftrag senden\n  \
           Esc        laufenden Auftrag abbrechen / sonst beenden\n  \
           Shift-Tab  Modus wechseln (nachfragen / auto / plan)\n  \
           F2         MCP-Panel öffnen (Server für den Agenten ein-/ausschalten)\n  \
           Ctrl-C     sofort beenden\n  \
           ↑/↓        Transcript scrollen   PgUp/PgDn seitenweise   End=ans Ende\n\n\
         LLM-AUSWAHL (ohne --demo, Feature `openai`):\n  \
           AZURE_OPENAI_API_KEY/_ENDPOINT/_DEPLOYMENT  -> Azure\n  \
           OPENAI_API_KEY [+ OPENAI_MODEL]             -> OpenAI\n  \
           OPENAI_BASE_URL [+ OPENAI_MODEL]            -> lokaler OpenAI-kompatibler\n  \
                                                           Server (Ollama, LM Studio, vLLM)\n  \
           sonst                                        -> Demo-Modus"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_app::optionen::pruefe_optionen;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Ein Tippfehler wird abgelehnt statt still ignoriert — `--no-swam`
    /// liefe sonst MIT Schwarm.
    #[test]
    fn unbekannte_option_wird_abgelehnt() {
        let e =
            pruefe_optionen(&v(&["--no-swam"]), OPTIONEN_MIT_WERT, OPTIONEN_OHNE_WERT).unwrap_err();
        assert_eq!(e, "Unbekannte Option --no-swam. Meintest du --no-swarm?");
    }

    /// `--flag=wert` wird gefunden, und alles bisher Gültige bleibt gültig.
    #[test]
    fn bekannte_optionen_bleiben_gueltig() {
        let args = pruefe_optionen(
            &v(&["--workspace=/tmp", "--mcp", "a", "--mcp=b", "-y", "--demo"]),
            OPTIONEN_MIT_WERT,
            OPTIONEN_OHNE_WERT,
        )
        .unwrap();
        assert_eq!(
            args,
            v(&[
                "--workspace",
                "/tmp",
                "--mcp",
                "a",
                "--mcp",
                "b",
                "-y",
                "--demo"
            ])
        );
        for flag in OPTIONEN_OHNE_WERT {
            assert!(pruefe_optionen(&v(&[flag]), OPTIONEN_MIT_WERT, OPTIONEN_OHNE_WERT).is_ok());
        }
    }
}
