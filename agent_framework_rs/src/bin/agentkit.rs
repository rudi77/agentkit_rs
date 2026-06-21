//! agentkit — die installierbare Kommandozeilen-/TUI-Anwendung.
//!
//! Dies ist die Haupt-Executable, die `cargo install` bzw. die Install-Skripte auf
//! den Rechner legen. Sie kennt drei Betriebsarten:
//!
//! ```bash
//! agentkit "Was ist 17 + 25?"     # One-shot: Auftrag ausführen, Antwort streamen
//! agentkit --repl                 # einfacher Zeilen-REPL (Memory bleibt erhalten)
//! agentkit --tui                  # interaktives Terminal-UI (nur mit Feature `tui`)
//! agentkit                        # ohne Argumente: TUI, falls einkompiliert, sonst REPL
//! ```
//!
//! LLM-Auswahl: `AZURE_OPENAI_*` -> Azure, sonst `OPENAI_API_KEY` (+ optional
//! `OPENAI_MODEL`) -> OpenAI, sonst ein eingebauter, netzfreier Demo-LLM. So ist die
//! Anwendung auch ohne API-Key sofort nutzbar (`--demo` erzwingt den Demo-Modus).

use std::io::Write;

use agentkit::demo::{build_llm, demo_tools};
use agentkit::events::EventData;
use agentkit::{new_cancel, Agent, Strategy};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("-h") || has("--help") {
        print_help();
        return Ok(());
    }
    if has("-V") || has("--version") {
        println!("agentkit {VERSION}");
        return Ok(());
    }

    let force_demo = has("--demo");
    let want_tui = has("--tui");
    let want_repl = has("--repl");
    let strategy = if has("--plan") {
        Strategy::Plan
    } else if has("--plain") {
        Strategy::Plain
    } else {
        Strategy::React
    };

    // Alles, was kein Flag ist, ergibt zusammen den Auftrag (so braucht es keine
    // Anführungszeichen für mehrteilige Aufträge).
    let task: String = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");

    if want_tui {
        return launch_tui(strategy, force_demo);
    }
    if !task.is_empty() {
        return run_once(&task, strategy, force_demo);
    }
    if want_repl {
        return repl(strategy, force_demo);
    }

    // Kein Auftrag und kein expliziter Modus: TUI, falls einkompiliert; sonst REPL.
    #[cfg(feature = "tui")]
    {
        launch_tui(strategy, force_demo)
    }
    #[cfg(not(feature = "tui"))]
    {
        repl(strategy, force_demo)
    }
}

/// Startet das TUI — aber nur, wenn das Binary mit Feature `tui` gebaut wurde.
fn launch_tui(strategy: Strategy, force_demo: bool) -> std::io::Result<()> {
    #[cfg(feature = "tui")]
    {
        agentkit::tui::run(strategy, force_demo)
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = (strategy, force_demo);
        eprintln!(
            "Dieses Build enthält kein TUI. Neu bauen mit `--features tui` \
             oder den REPL-/One-shot-Modus nutzen (`agentkit --repl` bzw. \
             `agentkit \"deine Frage\"`)."
        );
        Ok(())
    }
}

/// Baut LLM + Agent für den Demo-Werkzeugkasten und gibt das Modell-Label zurück.
fn build_agent(strategy: Strategy, force_demo: bool) -> (Agent, String) {
    let (llm, label) = build_llm(force_demo);
    let agent = Agent::builder(llm)
        .tools(demo_tools())
        .strategy(strategy)
        .build();
    (agent, label)
}

/// Arbeitet einen Auftrag ab und schreibt die Antwort (gestreamt) auf stdout; kam der
/// Text nicht als Deltas, wird die finale Antwort nachgetragen. Tool-Spur/Fehler gehen
/// nach stderr, damit stdout nur die Antwort trägt.
fn run_and_print(agent: &mut Agent, task: &str) {
    let cancel = new_cancel();
    let mut streamed = false;
    let final_text = agent.run_with_events(task, Some(&cancel), |ev| {
        on_event(ev.data, &mut streamed);
    });
    if !streamed {
        print!("{final_text}");
    }
    println!();
}

/// One-shot: einen einzelnen Auftrag abarbeiten.
fn run_once(task: &str, strategy: Strategy, force_demo: bool) -> std::io::Result<()> {
    let (mut agent, label) = build_agent(strategy, force_demo);
    eprintln!("» Modell: {label}");
    run_and_print(&mut agent, task);
    Ok(())
}

/// Einfacher Zeilen-REPL: Frage -> Antwort, der Agent behält sein Gedächtnis über die
/// Turns hinweg. Leere Zeile oder Ctrl-D beendet.
fn repl(strategy: Strategy, force_demo: bool) -> std::io::Result<()> {
    use std::io::BufRead;

    let (mut agent, label) = build_agent(strategy, force_demo);
    println!("agentkit REPL — Modell: {label}. Leere Zeile oder Ctrl-D beendet.");

    let stdin = std::io::stdin();
    loop {
        print!("› ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // Ctrl-D
        }
        let task = line.trim();
        if task.is_empty() {
            break;
        }
        run_and_print(&mut agent, task);
    }
    Ok(())
}

/// Rendert ein einzelnes Event als Konsolenausgabe. Text-Deltas streamen nach stdout,
/// alles andere wird als Spur nach stderr geschrieben.
fn on_event(data: EventData, streamed: &mut bool) {
    match data {
        EventData::TextDelta(t) => {
            print!("{t}");
            let _ = std::io::stdout().flush();
            *streamed = true;
        }
        EventData::ToolCall { name, args } => eprintln!("🔧 {name}({args})"),
        EventData::ToolResult { name, result } => eprintln!("   ↳ {name}: {result}"),
        EventData::Plan(p) => eprintln!("📋 {p}"),
        EventData::Error { name, error } => {
            let prefix = name.map(|n| format!("{n}: ")).unwrap_or_default();
            eprintln!("⚠ {prefix}{error}");
        }
        _ => {}
    }
}

fn print_help() {
    println!(
        "agentkit {VERSION} — ein ganz einfaches Agent-Framework als CLI/TUI\n\n\
         AUFRUF:\n  \
           agentkit [OPTIONEN] [AUFTRAG …]\n\n\
         BETRIEBSARTEN:\n  \
           agentkit \"Was ist 17 + 25?\"   One-shot: Auftrag ausführen, Antwort streamen\n  \
           agentkit --repl                einfacher Zeilen-REPL (Gedächtnis bleibt erhalten)\n  \
           agentkit --tui                 interaktives Terminal-UI (nur mit Feature `tui`)\n  \
           agentkit                       ohne Argumente: TUI falls vorhanden, sonst REPL\n\n\
         OPTIONEN:\n  \
           --demo        Demo-Modus erzwingen (eingebauter, netzfreier LLM)\n  \
           --plan        Plan-and-Execute statt ReAct\n  \
           --plain       Ohne Strategie-Preamble\n  \
           --repl        REPL-Modus\n  \
           --tui         TUI-Modus\n  \
           -h, --help    Diese Hilfe\n  \
           -V, --version Version anzeigen\n\n\
         LLM-AUSWAHL (ohne --demo, Feature `openai`):\n  \
           AZURE_OPENAI_API_KEY/_ENDPOINT/_DEPLOYMENT  -> Azure\n  \
           OPENAI_API_KEY [+ OPENAI_MODEL]             -> OpenAI\n  \
           sonst                                        -> Demo-Modus"
    );
}
