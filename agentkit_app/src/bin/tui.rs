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
use agentkit::{load_dotenv, load_user_config, Strategy};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("-h") || has("--help") {
        print_help();
        return Ok(());
    }
    // Wert eines Flags lesen (z. B. `--workspace <DIR>`).
    let val = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };

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
    #[cfg(feature = "graph")]
    if let Some(dir) = val("--graph") {
        let run_id = format!("pid-{}", std::process::id());
        match agentkit_app::open_graph(&dir, &workspace, &run_id, has("--graph-readonly")) {
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
    let graph_aktiv = cfg!(feature = "graph") && val("--graph").is_some();

    let cfg = TuiConfig {
        strategy,
        force_demo: has("--demo"),
        workspace,
        skills: val("--skills"),
        agents: val("--agents"),
        memory: val("--memory"),
        subagents: !has("--no-subagents"),
        max_steps: val("--max-steps")
            .and_then(|s| s.parse().ok())
            .unwrap_or(160),
        ask_approval: !(has("-y") || has("--yes")),
        mcp_config: val("--mcp-config"),
        mcp_enable,
        no_mcp: has("--no-mcp"),
        system: agentkit_app::system_with_extras(
            val("--system-file")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .or_else(|| val("--system"))
                .as_deref(),
            !has("--no-swarm"),
            graph_aktiv,
        ),
        ctx: val("--ctx"),
        ctx_budget: val("--ctx-budget")
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000),
        ctx_policy: val("--ctx-policy"),
        ctx_compaction_model: val("--ctx-compaction-model"),
        extra_tools: extras.build(),
        session: None,
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
           --memory FILE     Langzeitgedächtnis (JSONL)\n  \
           --no-subagents    das 'task'-Tool deaktivieren\n  \
           --no-swarm        das 'swarm'-Tool (dynamische Agenten-Schwärme) deaktivieren\n  \
           --max-steps N     Max. Loop-Schritte (Default: 160)\n  \
           -y, --yes         Shell-Freigabe initial auf AUTO\n  \
           --plan / --plain  Strategie statt ReAct\n  \
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
           Ctrl-Tab   Shell-Freigabe umschalten (nachfragen / auto)\n  \
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
