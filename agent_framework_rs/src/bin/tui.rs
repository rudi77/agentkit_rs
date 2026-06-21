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
use agentkit::{load_dotenv, Strategy};

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

    load_dotenv(); // .env aus dem aktuellen Verzeichnis (für AZURE_OPENAI_* / OPENAI_API_KEY)

    let strategy = if has("--plan") {
        Strategy::Plan
    } else if has("--plain") {
        Strategy::Plain
    } else {
        Strategy::React
    };

    let cfg = TuiConfig {
        strategy,
        force_demo: has("--demo"),
        workspace: val("-w").or_else(|| val("--workspace")).unwrap_or_else(|| ".".into()),
        skills: val("--skills"),
        agents: val("--agents"),
        memory: val("--memory"),
        subagents: !has("--no-subagents"),
        max_steps: val("--max-steps").and_then(|s| s.parse().ok()).unwrap_or(160),
        ask_approval: !(has("-y") || has("--yes")),
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
           --max-steps N     Max. Loop-Schritte (Default: 160)\n  \
           -y, --yes         Shell-Freigabe initial auf AUTO\n  \
           --plan / --plain  Strategie statt ReAct\n  \
           -h, --help        Diese Hilfe\n\n\
         TASTEN (im UI):\n  \
           Enter      Auftrag senden\n  \
           Esc        laufenden Auftrag abbrechen / sonst beenden\n  \
           Ctrl-Tab   Shell-Freigabe umschalten (nachfragen / auto)\n  \
           Ctrl-C     sofort beenden\n  \
           ↑/↓        Transcript scrollen   PgUp/PgDn seitenweise   End=ans Ende\n\n\
         LLM-AUSWAHL (ohne --demo, Feature `openai`):\n  \
           AZURE_OPENAI_API_KEY/_ENDPOINT/_DEPLOYMENT  -> Azure\n  \
           OPENAI_API_KEY [+ OPENAI_MODEL]             -> OpenAI\n  \
           sonst                                        -> Demo-Modus"
    );
}
