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

use agentkit::Strategy;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("-h") || has("--help") {
        print_help();
        return Ok(());
    }
    let force_demo = has("--demo");
    let strategy = if has("--plan") {
        Strategy::Plan
    } else if has("--plain") {
        Strategy::Plain
    } else {
        Strategy::React
    };

    agentkit::tui::run(strategy, force_demo)
}

fn print_help() {
    println!(
        "agentkit TUI — interaktives Terminal-UI für den Rust-Agenten\n\n\
         AUFRUF:\n  \
           cargo run --bin tui --features tui [-- OPTIONEN]\n  \
           agentkit --tui [OPTIONEN]   (Haupt-Executable, mit Feature `tui` gebaut)\n\n\
         OPTIONEN:\n  \
           --demo     Demo-Modus erzwingen (eingebauter, netzfreier LLM)\n  \
           --plan     Plan-and-Execute statt ReAct\n  \
           --plain    Ohne Strategie-Preamble\n  \
           -h, --help Diese Hilfe\n\n\
         TASTEN (im UI):\n  \
           Enter      Auftrag senden\n  \
           Esc        laufenden Auftrag abbrechen / sonst beenden\n  \
           Ctrl-C     sofort beenden\n  \
           ↑/↓        Transcript scrollen   PgUp/PgDn seitenweise   End=ans Ende\n\n\
         LLM-AUSWAHL (ohne --demo, Feature `openai`):\n  \
           AZURE_OPENAI_API_KEY/_ENDPOINT/_DEPLOYMENT  -> Azure\n  \
           OPENAI_API_KEY [+ OPENAI_MODEL]             -> OpenAI\n  \
           sonst                                        -> Demo-Modus"
    );
}
