//! agentkit — die installierbare Kommandozeilen-/TUI-Anwendung als nativer
//! Unix-Filter.
//!
//! Die Standard-Streams sind die primären I/O-Adapter (hexagonale Architektur):
//!
//! - **`stdin`**  trägt nur Kontext/Datenströme (per Pipe) und wird automatisch an
//!   die Query angehängt.
//! - **`stdout`** trägt nur das finale, bereinigte Resultat — pipe-tauglich für
//!   `jq`, `awk` oder einen zweiten Agenten.
//! - **`stderr`** trägt Status, Tool-Spur, ReAct-Gedanken und Fehler.
//!
//! ```bash
//! agentkit "Was ist 17 + 25?"                       # One-shot
//! cat daten.json | agentkit --format json "Fasse"   # stdin = Kontext, stdout = JSON
//! agentkit --dry-run "Räum das Verzeichnis auf"      # Schreibvorgänge blockiert
//! agentkit --repl                                    # interaktiver REPL
//! agentkit --tui                                     # Terminal-UI (Feature `tui`)
//! ```
//!
//! Exit-Codes: `0` Erfolg · `1` Laufzeitfehler · `2` API/Netz · `3` Kontext/Prompt
//! · `4` Format. SSOT der CLI-Parameter ist [`agentkit::Config`].

use std::io::{self, IsTerminal, Write};
use std::process;

use agentkit::demo::{build_llm_with, demo_tools};
use agentkit::events::EventData;
use agentkit::{
    build_task, classify_outcome, count_tokens_text, extract_json, is_likely_destructive,
    new_cancel, read_stdin_context, Agent, Config, ExitCode, Strategy, JSON_SYSTEM,
};
use clap::Parser;

fn main() -> io::Result<()> {
    let config = Config::parse();

    // Interaktive Modi: keine stdin-Kontext-Aufnahme, kein Exit-Code-Vertrag.
    if config.tui {
        return launch_tui(config.strategy(), config.demo);
    }
    let stdin_piped = !io::stdin().is_terminal();
    if config.repl && !stdin_piped {
        return repl(config.strategy(), config.demo);
    }

    // One-shot-/Pipe-Pfad: stdin als Kontext aufnehmen (falls gepipt) und an die
    // Query anhängen. Alle Statusmeldungen ab hier zwingend auf stderr.
    let context = if stdin_piped {
        read_stdin_context()?
    } else {
        None
    };
    if let Some(ctx) = &context {
        eprintln!("[INFO] Kontext aus Pipe gelesen ({} Bytes).", ctx.len());
    }

    let task = build_task(&config.prompt_text(), context.as_deref());

    // Kein Auftrag und kein expliziter Modus -> interaktiver Default (TUI/REPL).
    if task.is_empty() {
        if !stdin_piped {
            return default_interactive(config.strategy(), config.demo);
        }
        eprintln!("[ERROR] Kein Prompt übergeben und stdin lieferte keine Daten.");
        process::exit(ExitCode::ContextError.code());
    }

    // Validierung: passt der (geschätzte) Kontext ins Fenster? -> sonst Exit 3.
    let tokens = count_tokens_text(&task);
    if tokens > config.max_context {
        eprintln!(
            "[ERROR] Kontext zu groß: ~{tokens} Tokens > Limit {}. \
             (Anpassbar via --max-context / AGENTKIT_MAX_CONTEXT.)",
            config.max_context
        );
        process::exit(ExitCode::ContextError.code());
    }

    let code = run_once(&task, &config);
    process::exit(code.code());
}

/// One-shot mit Exit-Code-Vertrag. Im JSON-Modus wird die Antwort validiert und bei
/// Bedarf mehrfach neu erzeugt; gelingt das nicht, ist der Exit-Code 4.
fn run_once(task: &str, config: &Config) -> ExitCode {
    let json_mode = config.json_mode();
    let (llm, label) = build_llm_with(config.demo, json_mode);
    eprintln!("[INFO] Modell: {label}");
    if config.dry_run {
        eprintln!("[INFO] Dry-Run aktiv — zerstörerische Schreibvorgänge werden blockiert.");
    }

    // Im Text-Modus an einem Terminal darf die Antwort live nach stdout streamen;
    // sobald stdout in eine Pipe geht (oder JSON erzwungen ist), sammeln wir die
    // Antwort und schreiben EINMAL ein sauberes Resultat (Format-Treue).
    let stream_to_stdout = !json_mode && io::stdout().is_terminal();

    let attempts = if json_mode {
        config.json_retries.max(1)
    } else {
        1
    };
    let mut last_final = String::new();

    for attempt in 1..=attempts {
        if json_mode && attempt > 1 {
            eprintln!("[INFO] JSON ungültig — neuer Versuch {attempt}/{attempts} …");
        }

        let mut agent = build_agent(llm.clone(), config, json_mode);
        let cancel = new_cancel();
        let mut hard_error = false;
        let final_text = agent.run_with_events(task, Some(&cancel), |ev| {
            on_event(ev.data, stream_to_stdout, &mut hard_error);
        });
        if stream_to_stdout {
            // Der live gestreamte Text liegt schon auf stdout — nur abschließen.
            println!();
        } else {
            // Der Denkprozess lief auf stderr; sauberen Abschluss dort setzen.
            eprintln!();
        }

        // Harte Fehler (Modell unerreichbar) / Sentinels -> direkter Exit-Code.
        if let Some(code) = classify_outcome(&final_text, hard_error) {
            return code;
        }

        if json_mode {
            match extract_json(&final_text) {
                Some(clean) => return print_result(&clean),
                None => {
                    last_final = final_text;
                    continue; // erneut versuchen
                }
            }
        }

        // Text-Modus: bei Pipe das Resultat sauber ausgeben (bei TTY schon gestreamt).
        if stream_to_stdout {
            return ExitCode::Success;
        }
        return print_result(&final_text);
    }

    eprintln!(
        "[ERROR] Konnte trotz {attempts} Versuchen kein gültiges JSON erzeugen. \
         Letzte Antwort (gekürzt): {}",
        last_final.chars().take(200).collect::<String>()
    );
    ExitCode::FormatError
}

/// Schreibt das finale Resultat auf stdout (genau eine Zeile, getrimmt) und meldet
/// Erfolg — oder einen Laufzeitfehler, falls stdout nicht beschreibbar ist.
fn print_result(text: &str) -> ExitCode {
    match writeln!(io::stdout(), "{}", text.trim_end()) {
        Ok(()) => ExitCode::Success,
        Err(e) => {
            eprintln!("[ERROR] Schreiben auf stdout fehlgeschlagen: {e}");
            ExitCode::GeneralError
        }
    }
}

/// Baut LLM-Agent für den Demo-Werkzeugkasten — inklusive `--dry-run`-Sicherung und
/// (im JSON-Modus) der JSON-System-Anweisung.
fn build_agent(llm: std::sync::Arc<dyn agentkit::Llm>, config: &Config, json_mode: bool) -> Agent {
    let mut tools = demo_tools();
    if config.dry_run {
        tools = tools.dry_run_blocking(is_likely_destructive);
    }
    let mut builder = Agent::builder(llm).tools(tools).strategy(config.strategy());
    if json_mode {
        builder = builder.system(JSON_SYSTEM);
    }
    builder.build()
}

/// Rendert ein einzelnes Event. Text-Deltas gehen nur dann nach stdout, wenn dort
/// live gestreamt werden darf; sonst (Pipe/JSON) auf stderr als sichtbarer
/// Denkprozess. Tool-Spur, Plan und Fehler immer auf stderr.
fn on_event(data: EventData, stream_to_stdout: bool, hard_error: &mut bool) {
    match data {
        EventData::TextDelta(t) => {
            if stream_to_stdout {
                print!("{t}");
                let _ = io::stdout().flush();
            } else {
                eprint!("{t}");
                let _ = io::stderr().flush();
            }
        }
        EventData::ToolCall { name, args } => eprintln!("🔧 {name}({args})"),
        EventData::ToolResult { name, result } => eprintln!("   ↳ {name}: {result}"),
        EventData::Plan(p) => eprintln!("📋 {p}"),
        EventData::Error { name, error } => {
            // Fehler ohne Tool-Namen = harter Stream-/Modellfehler (API/Netz).
            if name.is_none() {
                *hard_error = true;
            }
            let prefix = name.map(|n| format!("{n}: ")).unwrap_or_default();
            eprintln!("⚠ {prefix}{error}");
        }
        _ => {}
    }
}

/// Einfacher Zeilen-REPL: Frage -> Antwort, der Agent behält sein Gedächtnis. Leere
/// Zeile oder Ctrl-D beendet. Antworten streamen nach stdout, Spur nach stderr.
fn repl(strategy: Strategy, force_demo: bool) -> io::Result<()> {
    use std::io::BufRead;

    let (llm, label) = build_llm_with(force_demo, false);
    let mut agent = Agent::builder(llm)
        .tools(demo_tools())
        .strategy(strategy)
        .build();
    eprintln!("agentkit REPL — Modell: {label}. Leere Zeile oder Ctrl-D beendet.");

    let stdin = io::stdin();
    loop {
        eprint!("› ");
        io::stderr().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // Ctrl-D
        }
        let task = line.trim();
        if task.is_empty() {
            break;
        }
        let cancel = new_cancel();
        let mut hard_error = false;
        agent.run_with_events(task, Some(&cancel), |ev| {
            on_event(ev.data, true, &mut hard_error);
        });
        println!();
    }
    Ok(())
}

/// Default ohne Auftrag/Modus an einem Terminal: TUI, falls einkompiliert, sonst REPL.
fn default_interactive(strategy: Strategy, force_demo: bool) -> io::Result<()> {
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
fn launch_tui(strategy: Strategy, force_demo: bool) -> io::Result<()> {
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
