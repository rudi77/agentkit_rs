//! agentkit — die installierbare Kommandozeilen-/TUI-Anwendung (Claude-Code-Stil),
//! zugleich ein pipe-tauglicher Unix-Filter.
//!
//! Derselbe Agent-Loop wie sonst, mit einer Konsolen-Oberfläche drumherum:
//!
//! ```bash
//! agentkit "Was ist 17 + 25?"        # One-shot: Auftrag ausführen, Antwort streamen
//! cat daten.json | agentkit -p "Fasse zusammen" | jq .   # stdin = Kontext, stdout = Resultat
//! agentkit --format json "…"          # strukturierter Output (Validierung + Retries)
//! agentkit --dry-run "…"              # zerstörerische Schreibvorgänge blockieren
//! agentkit                            # interaktive Session (REPL)
//! agentkit --tui                      # interaktives Terminal-UI (nur mit Feature `tui`)
//! ```
//!
//! Unix-I/O-Adapter (hexagonale Architektur): **stdin** trägt gepipten Kontext (wird
//! an die Query angehängt); **stdout** trägt — sobald die Ausgabe gepipt wird, im
//! JSON- oder `--print`-Modus — *nur* das finale, bereinigte Resultat; **stderr**
//! trägt Status, Tool-Spur, ReAct-Gedanken und Fehler. Exit-Codes: `0` Erfolg ·
//! `1` Laufzeitfehler · `2` API/Netz · `3` Kontext/Prompt · `4` Format.
//!
//! Mit echtem LLM (Azure/OpenAI) ist es der volle Coding-Agent — Sandbox-Tools
//! (inkl. glob/grep), Skills, Plan und das `task`-Tool für Sub-Agenten. Ohne API-Key
//! läuft ein netzfreier Demo-Modus mit kleinem Werkzeugkasten.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::coding::ApproveFn;
use agentkit::demo::demo_tools;
use agentkit::{
    build_coding_agent, build_task, classify_outcome, count_tokens_text, extract_json,
    is_likely_destructive, load_dotenv, new_cancel, read_stdin_context, strategy_from_str, Agent,
    AgentEvent, AgentRole, CodingAgentConfig, EventBus, EventData, ExitCode, Llm, OutputFormat,
    Plan, ShortTermMemory, Skills, Strategy, DONE, JSON_SYSTEM,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// --- Globaler Ctrl-C-Zustand: der Handler setzt den Stop-Knopf des laufenden Tasks.
static INT_COUNT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CANCEL: Mutex<Option<agentkit::Cancel>> = Mutex::new(None);

fn main() -> std::io::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| argv.iter().any(|a| a == flag);

    if has("-h") || has("--help") {
        print_help();
        return Ok(());
    }
    if has("-V") || has("--version") {
        println!("agentkit {VERSION}");
        return Ok(());
    }

    let args = Args::parse(&argv);
    load_dotenv();

    // Farben: nur, wenn ein Terminal vorliegt und nicht --no-color (auf Windows VT aktivieren).
    let color = !args.no_color && std::io::stdout().is_terminal() && enable_vt();
    let pal = if color { Pal::color() } else { Pal::plain() };

    // Stop-Knopf: Ctrl-C bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).
    let _ = ctrlc::set_handler(|| {
        let n = INT_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(c) = CURRENT_CANCEL.lock().unwrap().clone() {
            c.store(true, Ordering::Relaxed);
        }
        if n >= 2 {
            std::process::exit(130);
        }
        eprintln!("\n⏸  unterbreche … (nochmal Ctrl-C zum Beenden)");
    });

    if args.tui {
        return launch_tui(&args);
    }

    // One-shot-/Pipe-Pfad: gepipter stdin wird als Kontext an die Query gehängt.
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdin_ctx = if stdin_is_tty {
        None
    } else {
        read_stdin_context()?
    };
    let have_task = !args.prompt.trim().is_empty() || stdin_ctx.is_some();
    if have_task || args.print_mode {
        let code = run_oneshot(&args, pal, stdin_ctx);
        std::process::exit(code.code());
    }

    // Ohne Auftrag und ohne Terminal (leere Pipe) gibt es nichts zu tun -> Exit 3
    // (der REPL braucht ein interaktives stdin).
    if !stdin_is_tty {
        eprintln!("[ERROR] Kein Prompt übergeben und stdin lieferte keine Daten.");
        std::process::exit(ExitCode::ContextError.code());
    }

    // Interaktive Session (stdin ist ein Terminal, kein Auftrag).
    let (mut agent, plan, skills, roles) = build_agent(&args, pal);
    let mut renderer = Renderer {
        show_steps: args.steps,
        quiet: false,
        streaming: false,
        pal,
        to_stderr: false,
    };
    println!("{}", banner(&args, pal));
    repl(
        &mut agent,
        &plan,
        skills.as_ref(),
        &roles,
        &mut renderer,
        pal,
    );
    Ok(())
}

// ------------------------------------------------------------------- Argumente

struct Args {
    prompt: String,
    workspace: String,
    strategy: Strategy,
    skills: Option<String>,
    agents: Option<String>,
    memory: Option<String>,
    provider: String,
    demo: bool,
    max_steps: usize,
    no_subagents: bool,
    yes: bool,
    steps: bool,
    no_color: bool,
    print_mode: bool,
    tui: bool,
    // Unix-Pipe-Optionen.
    format: OutputFormat,
    dry_run: bool,
    max_context: usize,
    json_retries: u32,
}

impl Args {
    fn parse(argv: &[String]) -> Args {
        let mut a = Args {
            prompt: String::new(),
            workspace: ".".to_string(),
            strategy: Strategy::React,
            skills: None,
            agents: None,
            memory: None,
            provider: "auto".to_string(),
            demo: false,
            max_steps: 160,
            no_subagents: false,
            yes: false,
            steps: false,
            no_color: false,
            print_mode: false,
            tui: false,
            format: OutputFormat::Text,
            dry_run: false,
            max_context: 128_000,
            json_retries: 3,
        };
        let mut prompt: Vec<String> = Vec::new();
        let mut it = argv.iter().peekable();
        while let Some(arg) = it.next() {
            let mut take = || it.next().cloned().unwrap_or_default();
            match arg.as_str() {
                "-w" | "--workspace" => a.workspace = take(),
                "-s" | "--strategy" => a.strategy = strategy_from_str(&take()),
                "--skills" => a.skills = Some(take()),
                "--agents" => a.agents = Some(take()),
                "--memory" => a.memory = Some(take()),
                "--provider" => a.provider = take(),
                "--max-steps" => a.max_steps = take().parse().unwrap_or(160),
                "--plan" => a.strategy = Strategy::Plan,
                "--plain" => a.strategy = Strategy::Plain,
                "--react" => a.strategy = Strategy::React,
                "--demo" => a.demo = true,
                "--no-subagents" => a.no_subagents = true,
                "-y" | "--yes" => a.yes = true,
                "--steps" => a.steps = true,
                "--no-color" => a.no_color = true,
                "-p" | "--print" => a.print_mode = true,
                "--tui" => a.tui = true,
                "--repl" => {} // expliziter REPL = Default ohne Auftrag
                "--format" => a.format = parse_format(&take()),
                "--dry-run" => a.dry_run = true,
                "--max-context" => a.max_context = take().parse().unwrap_or(128_000),
                "--json-retries" => a.json_retries = take().parse().unwrap_or(3),
                other if other.starts_with('-') => {} // unbekannte Flags ignorieren
                other => prompt.push(other.to_string()),
            }
        }
        a.prompt = prompt.join(" ");
        a
    }
}

/// `--format`-Wert -> [`OutputFormat`] (unbekannt => Text).
fn parse_format(s: &str) -> OutputFormat {
    match s.trim().to_lowercase().as_str() {
        "json" => OutputFormat::Json,
        _ => OutputFormat::Text,
    }
}

// --------------------------------------------------------------------- Farben

#[derive(Clone, Copy)]
struct Pal {
    reset: &'static str,
    bold: &'static str,
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    magenta: &'static str,
    cyan: &'static str,
    gray: &'static str,
}

impl Pal {
    fn color() -> Self {
        Pal {
            reset: "\x1b[0m",
            bold: "\x1b[1m",
            red: "\x1b[31m",
            green: "\x1b[32m",
            yellow: "\x1b[33m",
            magenta: "\x1b[35m",
            cyan: "\x1b[36m",
            gray: "\x1b[90m",
        }
    }
    fn plain() -> Self {
        Pal {
            reset: "",
            bold: "",
            red: "",
            green: "",
            yellow: "",
            magenta: "",
            cyan: "",
            gray: "",
        }
    }
}

/// Aktiviert ANSI-Verarbeitung auf der Windows-Konsole (Virtual Terminal). Auf
/// anderen Plattformen (und in Windows Terminal) immer `true`.
#[cfg(windows)]
fn enable_vt() -> bool {
    extern "system" {
        fn GetStdHandle(n: u32) -> isize;
        fn GetConsoleMode(h: isize, m: *mut u32) -> i32;
        fn SetConsoleMode(h: isize, m: u32) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // -11
    const ENABLE_VT: u32 = 0x0004;
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        SetConsoleMode(h, mode | ENABLE_VT) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

// ----------------------------------------------------------------- Rendering

fn abbrev(value: &str, limit: usize) -> String {
    let s: String = value
        .chars()
        .map(|c| if c == '\n' { '↵' } else { c })
        .collect();
    if s.chars().count() > limit {
        let head: String = s.chars().take(limit).collect();
        format!("{head}… ({} Z.)", s.chars().count())
    } else {
        s
    }
}

/// Tool-Argumente als `k=v, …` (Objekt) oder kompaktes JSON.
fn fmt_args(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                let val = match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                };
                format!("{k}={}", abbrev(&val, 60))
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => abbrev(&args.to_string(), 60),
    }
}

/// Übersetzt `AgentEvent`s in farbige Terminal-Ausgabe.
///
/// `to_stderr` lenkt die gesamte Spur (inkl. gestreamter Token) auf stderr — so
/// bleibt stdout für das reine Resultat frei, wenn die Ausgabe gepipt wird, im
/// JSON- oder `--print`-Modus läuft.
struct Renderer {
    show_steps: bool,
    quiet: bool,
    streaming: bool,
    pal: Pal,
    to_stderr: bool,
}

impl Renderer {
    /// Eine Zeile auf den gewählten Strom.
    fn put(&self, s: &str) {
        if self.to_stderr {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    }

    /// Rohtext ohne Zeilenumbruch (Streaming) auf den gewählten Strom, sofort geflusht.
    fn put_raw(&self, s: &str) {
        if self.to_stderr {
            eprint!("{s}");
            let _ = std::io::stderr().flush();
        } else {
            print!("{s}");
            let _ = std::io::stdout().flush();
        }
    }

    fn end_stream(&mut self) {
        if self.streaming {
            self.put("");
            self.streaming = false;
        }
    }

    fn handle(&mut self, ev: &AgentEvent) {
        if self.quiet {
            return;
        }
        let p = self.pal;
        let src = ev.source.as_str();

        // TEXT_DELTA zuerst (höchste Frequenz): nur der Haupt-Agent streamt Token.
        if let EventData::TextDelta(t) = &ev.data {
            if !src.is_empty() {
                return;
            }
            self.streaming = true;
            self.put_raw(t);
            return;
        }

        // Tag für (auch parallele) Sub-Agenten.
        let tag = if src.is_empty() {
            String::new()
        } else {
            let label = src.split(':').next().unwrap_or(src);
            format!("{}[{label}]{} ", p.gray, p.reset)
        };

        match &ev.data {
            EventData::Step { step } => {
                if self.show_steps {
                    self.end_stream();
                    self.put(&format!("{tag}{}— Schritt {step} —{}", p.gray, p.reset));
                }
            }
            EventData::ToolCall { name, args } => {
                self.end_stream();
                self.put(&format!(
                    "{tag}{}⏺ {}{name}{}{}({}){}",
                    p.cyan,
                    p.bold,
                    p.reset,
                    p.gray,
                    fmt_args(args),
                    p.reset
                ));
            }
            EventData::ToolResult { name: _, result } => {
                self.end_stream();
                self.print_result(result, &tag);
            }
            EventData::Plan(text) => {
                self.end_stream();
                self.put(&format!("{}📋 Plan{}", p.magenta, p.reset));
                for line in text.lines() {
                    self.put(&format!("{}   {line}{}", p.magenta, p.reset));
                }
            }
            EventData::Error { name, error } => {
                self.end_stream();
                let n = name.as_deref().unwrap_or("?");
                self.put(&format!(
                    "{tag}{}✖ Fehler in {n}: {error}{}",
                    p.red, p.reset
                ));
            }
            EventData::Cancelled { where_ } => {
                self.end_stream();
                self.put(&format!("{}⛔ abgebrochen ({where_}){}", p.yellow, p.reset));
            }
            EventData::Final(_) => self.end_stream(),
            // TextDelta wurde oben bereits behandelt (früher Return).
            EventData::TextDelta(_) | EventData::Done | EventData::None => {}
        }
    }

    fn print_result(&self, result: &str, tag: &str) {
        let p = self.pal;
        let lines: Vec<&str> = if result.is_empty() {
            vec!["(leer)"]
        } else {
            result.lines().collect()
        };
        let max_lines = 6;
        for line in lines.iter().take(max_lines) {
            self.put(&format!(
                "{tag}{}  ⎿ {}{}",
                p.gray,
                abbrev(line, 100),
                p.reset
            ));
        }
        if lines.len() > max_lines {
            self.put(&format!(
                "{tag}{}  ⎿ …(+{} Zeilen){}",
                p.gray,
                lines.len() - max_lines,
                p.reset
            ));
        }
    }
}

// ------------------------------------------------------------------ Approval

/// approve-Callback für `run_shell`: fragt mit eingefärbtem Prompt nach.
fn confirm_shell(command: &str, pal: Pal) -> bool {
    eprintln!(
        "\n{}⚠  Shell-Befehl ausführen?{}\n  {}{command}{}",
        pal.yellow, pal.reset, pal.bold, pal.reset
    );
    eprint!("{}  [j]a / [N]ein › {}", pal.yellow, pal.reset);
    let _ = std::io::stderr().flush();
    let mut ans = String::new();
    if std::io::stdin().read_line(&mut ans).is_err() {
        return false;
    }
    matches!(ans.trim().to_lowercase().as_str(), "j" | "ja" | "y" | "yes")
}

// --------------------------------------------------------------------- Setup

/// Wählt den LLM und gibt `(llm, label)` zurück.
fn build_llm(provider: &str, force_demo: bool) -> (Arc<dyn Llm>, String) {
    if force_demo || provider == "demo" {
        return agentkit::demo::build_llm(true);
    }
    #[cfg(feature = "openai")]
    {
        if provider == "azure" {
            match agentkit::azure_from_env() {
                Ok(llm) => {
                    let dep =
                        std::env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "?".into());
                    return (Arc::new(llm), format!("azure:{dep}"));
                }
                Err(e) => eprintln!("azure_from_env: {e} — Demo-Fallback"),
            }
        }
        if provider == "openai" {
            match agentkit::openai_from_env() {
                Ok(llm) => {
                    let model =
                        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
                    return (Arc::new(llm), format!("openai:{model}"));
                }
                Err(e) => eprintln!("openai_from_env: {e} — Demo-Fallback"),
            }
        }
    }
    // auto (oder Feature `openai` aus): Azure -> OpenAI -> Demo.
    agentkit::demo::build_llm(false)
}

/// Stellt den Agenten zusammen: voller Coding-Agent (echter LLM) oder schlanker
/// Demo-Agent. Gibt zusätzlich Plan, Skills und Rollen für die Slash-Befehle zurück.
fn build_agent(args: &Args, pal: Pal) -> (Agent, Plan, Option<Skills>, Vec<AgentRole>) {
    let (llm, label) = build_llm(&args.provider, args.demo);
    eprintln!("{}» Modell: {label}{}", pal.gray, pal.reset);

    // Demo-Modus: schlanker, netzfreier Agent.
    if label.starts_with("demo") {
        let agent = Agent::builder(llm)
            .tools(demo_tools())
            .strategy(args.strategy)
            .max_steps(args.max_steps)
            .build();
        return (agent, Plan::new(), None, Vec::new());
    }

    // Freigabe-Policy steckt im Callback: bei `--yes` immer erlauben, sonst nachfragen.
    let yes = args.yes;
    let approve: ApproveFn = Arc::new(move |cmd: &str| yes || confirm_shell(cmd, pal));

    let cfg = CodingAgentConfig {
        workspace: &args.workspace,
        strategy: args.strategy,
        max_steps: args.max_steps,
        skills: args.skills.as_deref(),
        agents: args.agents.as_deref(),
        memory: args.memory.as_deref(),
        subagents: !args.no_subagents,
        plan_sep: "\n",
    };
    build_coding_agent(llm, &cfg, approve)
}

// ------------------------------------------------------------ One-shot / Pipe

/// One-shot mit Exit-Code-Vertrag und strikter Stream-Trennung. Im JSON-Modus wird
/// die Antwort validiert und bei Bedarf mehrfach neu erzeugt; gelingt das nicht, ist
/// der Exit-Code 4.
fn run_oneshot(args: &Args, pal: Pal, stdin_ctx: Option<String>) -> ExitCode {
    let task = build_task(args.prompt.trim(), stdin_ctx.as_deref());
    if task.is_empty() {
        eprintln!("Keine Aufgabe übergeben.");
        return ExitCode::ContextError;
    }

    // Validierung: passt der (geschätzte) Kontext ins Fenster? -> sonst Exit 3.
    let tokens = count_tokens_text(&task);
    if tokens > args.max_context {
        eprintln!(
            "[ERROR] Kontext zu groß: ~{tokens} Tokens > Limit {}. \
             (Anpassbar via --max-context.)",
            args.max_context
        );
        return ExitCode::ContextError;
    }

    let json_mode = args.format == OutputFormat::Json;
    // Sobald die Ausgabe gepipt wird, im JSON- oder --print-Modus läuft: stdout
    // bleibt dem reinen Resultat vorbehalten, die Spur geht auf stderr.
    let clean_stdout = json_mode || args.print_mode || !std::io::stdout().is_terminal();

    let attempts = if json_mode {
        args.json_retries.max(1)
    } else {
        1
    };
    let mut last_final = String::new();

    for attempt in 1..=attempts {
        if attempt > 1 {
            eprintln!("[INFO] JSON ungültig — neuer Versuch {attempt}/{attempts} …");
        }

        // Frischer Agent pro Versuch (sauberes Gedächtnis bei JSON-Retry).
        let (mut agent, _plan, _skills, _roles) = build_agent(args, pal);
        if args.dry_run {
            eprintln!("[INFO] Dry-Run aktiv — zerstörerische Schreibvorgänge werden blockiert.");
            agent.tools = agent.tools.dry_run_blocking(is_likely_destructive);
        }
        if json_mode {
            inject_json_system(&mut agent);
        }

        let mut renderer = Renderer {
            show_steps: args.steps,
            quiet: args.print_mode,
            streaming: false,
            pal,
            to_stderr: clean_stdout,
        };
        let (_agent, final_, hard_error) = run_task(agent, &task, &mut renderer);

        // Harte Fehler (Modell unerreichbar) / Sentinels -> direkter Exit-Code.
        if let Some(code) = classify_outcome(&final_, hard_error) {
            return code;
        }

        if json_mode {
            // Gültiges JSON -> sauber ausgeben; sonst nächster Versuch.
            let Some(clean) = extract_json(&final_) else {
                last_final = final_;
                continue;
            };
            return print_result_stdout(&clean);
        }

        // Text-Modus: bei sauberem stdout das Resultat einmal ausgeben (bei TTY hat
        // der Renderer es bereits live gestreamt).
        return if clean_stdout {
            print_result_stdout(&final_)
        } else {
            ExitCode::Success
        };
    }

    eprintln!(
        "[ERROR] Konnte trotz {attempts} Versuchen kein gültiges JSON erzeugen. \
         Letzte Antwort (gekürzt): {}",
        last_final.chars().take(200).collect::<String>()
    );
    ExitCode::FormatError
}

/// Schreibt das finale Resultat (getrimmt, eine abschließende Zeile) auf stdout.
fn print_result_stdout(text: &str) -> ExitCode {
    match writeln!(std::io::stdout(), "{}", text.trim_end()) {
        Ok(()) => ExitCode::Success,
        Err(e) => {
            eprintln!("[ERROR] Schreiben auf stdout fehlgeschlagen: {e}");
            ExitCode::GeneralError
        }
    }
}

/// Hängt die JSON-System-Anweisung an die System-Nachricht des Agenten an (bzw. legt
/// eine an), damit auch Modelle ohne nativen JSON-Mode strukturiert antworten.
fn inject_json_system(agent: &mut Agent) {
    let msgs = &mut agent.memory.messages;
    if let Some(sys) = msgs.iter_mut().find(|m| m["role"] == "system") {
        if let Some(c) = sys["content"].as_str() {
            sys["content"] = serde_json::Value::String(format!("{c}\n\n{JSON_SYSTEM}"));
            return;
        }
    }
    msgs.insert(
        0,
        serde_json::json!({"role": "system", "content": JSON_SYSTEM}),
    );
}

// ------------------------------------------------------------------ Ausführen

/// Treibt EINE Aufgabe auf einem Worker-Thread an und rendert die Events live. Gibt
/// `(Agent, finale Antwort, harter_Fehler)` zurück; `harter_Fehler` markiert einen
/// Modell-/Stream-Ausfall (ERROR-Event ohne Tool-Namen) für die Exit-Code-Abbildung.
fn run_task(agent: Agent, task: &str, renderer: &mut Renderer) -> (Agent, String, bool) {
    let bus = EventBus::new();
    let q = bus.subscribe();
    let cancel = new_cancel();
    INT_COUNT.store(0, Ordering::SeqCst);
    *CURRENT_CANCEL.lock().unwrap() = Some(cancel.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    let task_owned = task.to_string();
    let bus_worker = bus.clone();
    let cancel_worker = cancel.clone();
    let mut agent = agent;
    std::thread::spawn(move || {
        let final_ = agent.run_on_bus(&task_owned, &bus_worker, -1, Some(&cancel_worker), "");
        let _ = tx.send((agent, final_));
    });

    // Nur das Root-DONE (leere `source`) beendet die Anzeige; Sub-Agent-DONEs nicht.
    let mut hard_error = false;
    while let Ok(ev) = q.recv() {
        if ev.etype == DONE && ev.source.is_empty() {
            break;
        }
        if let EventData::Error { name: None, .. } = &ev.data {
            hard_error = true;
        }
        renderer.handle(&ev);
    }
    let (agent, final_) = rx
        .recv()
        .unwrap_or((build_dummy(), "(keine Antwort)".into()));
    *CURRENT_CANCEL.lock().unwrap() = None;
    (agent, final_, hard_error)
}

/// Notnagel, falls der Worker-Kanal abreißt (sollte nie passieren).
fn build_dummy() -> Agent {
    Agent::builder(agentkit::demo::build_llm(true).0).build()
}

// -------------------------------------------------------------- Slash-Befehle

fn repl(
    agent: &mut Agent,
    plan: &Plan,
    skills: Option<&Skills>,
    roles: &[AgentRole],
    renderer: &mut Renderer,
    pal: Pal,
) {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    loop {
        print!("\n{}› {}", pal.green, pal.reset);
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            println!("\n{}Tschüss.{}", pal.gray, pal.reset);
            return;
        }
        let user = line.trim().to_string();
        if user.is_empty() {
            continue;
        }
        if user.starts_with('/') {
            if !handle_slash(&user, agent, plan, skills, roles, pal) {
                println!("{}Tschüss.{}", pal.gray, pal.reset);
                return;
            }
            continue;
        }
        // Agent kurz herausnehmen, auf dem Worker laufen lassen, zurückholen.
        let taken = std::mem::replace(agent, build_dummy());
        let (back, _final, _hard) = run_task(taken, &user, renderer);
        *agent = back;
    }
}

fn handle_slash(
    cmd: &str,
    agent: &mut Agent,
    plan: &Plan,
    skills: Option<&Skills>,
    roles: &[AgentRole],
    pal: Pal,
) -> bool {
    let name = cmd[1..].trim().to_lowercase();
    match name.as_str() {
        "exit" | "quit" | "q" => return false,
        "help" => println!("{}", help_text(pal)),
        "clear" => {
            let _ = std::process::Command::new(if cfg!(windows) { "cmd" } else { "clear" })
                .args(if cfg!(windows) {
                    vec!["/c", "cls"]
                } else {
                    vec![]
                })
                .status();
        }
        "reset" => {
            let sys = agent
                .memory
                .messages
                .iter()
                .find(|m| m["role"] == "system")
                .and_then(|m| m["content"].as_str())
                .map(|s| s.to_string());
            agent.memory = ShortTermMemory::new(sys.as_deref());
            println!("{}✓ Unterhaltung zurückgesetzt.{}", pal.green, pal.reset);
        }
        "plan" => println!("{}{}{}", pal.magenta, plan.render(), pal.reset),
        "tools" => {
            let mut names = agent.tools.names();
            names.sort();
            println!("{}Tools:{} {}", pal.bold, pal.reset, names.join(", "));
        }
        "agents" => {
            if !agent.tools.has("task") {
                println!(
                    "{}(Sub-Agenten deaktiviert — ohne --no-subagents starten){}",
                    pal.gray, pal.reset
                );
            } else {
                println!(
                    "{}Sub-Agent-Rollen (task subagent_type=…):{}",
                    pal.bold, pal.reset
                );
                println!(
                    "  {}general{} — beliebige abgegrenzte Teilaufgabe (voller Coding-Zugriff)",
                    pal.cyan, pal.reset
                );
                for r in roles {
                    println!("  {}{}{} — {}", pal.cyan, r.name, pal.reset, r.description);
                }
            }
        }
        "skills" => match skills {
            None => println!(
                "{}(keine Skills aktiv — mit --skills <ordner> starten){}",
                pal.gray, pal.reset
            ),
            Some(s) => {
                let idx = s.index();
                if idx.is_empty() {
                    println!("{}(keine Skills gefunden){}", pal.gray, pal.reset);
                }
                for info in idx {
                    println!(
                        "  {}{}{} — {}",
                        pal.cyan, info.name, pal.reset, info.description
                    );
                }
            }
        },
        _ => println!(
            "{}Unbekannter Befehl: {cmd}{}  ({}/help{})",
            pal.red, pal.reset, pal.cyan, pal.reset
        ),
    }
    true
}

fn help_text(p: Pal) -> String {
    format!(
        "{}Befehle{}\n  \
         {}/help{}      diese Hilfe\n  \
         {}/clear{}     Bildschirm leeren\n  \
         {}/reset{}     Unterhaltung vergessen (neues Kurzzeitgedächtnis)\n  \
         {}/plan{}      aktuellen Plan zeigen\n  \
         {}/tools{}     registrierte Tools auflisten\n  \
         {}/skills{}    verfügbare Skills auflisten\n  \
         {}/agents{}    verfügbare Sub-Agent-Rollen (task-Tool) auflisten\n  \
         {}/exit{}      beenden (auch /quit, Ctrl-D)\n\n\
         Sonst: einfach eine Aufgabe eintippen. Ctrl-C bricht die laufende Aufgabe ab.",
        p.bold,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset,
        p.cyan,
        p.reset
    )
}

fn banner(args: &Args, p: Pal) -> String {
    let ws = std::path::Path::new(&args.workspace)
        .canonicalize()
        .map(|x| x.display().to_string())
        .unwrap_or_else(|_| args.workspace.clone());
    let strat = match args.strategy {
        Strategy::React => "react",
        Strategy::Plan => "plan",
        Strategy::Plain => "plain",
    };
    format!(
        "{}== agentkit =={}  — ein LLM in einer Schleife mit Tools\n\
         {}Workspace:{} {}\n{}Strategie:{} {}\n\
         {}/help{} für Befehle, {}/exit{} zum Beenden",
        p.cyan,
        p.reset,
        p.gray,
        p.reset,
        abbrev(&ws, 60),
        p.gray,
        p.reset,
        strat,
        p.gray,
        p.reset,
        p.gray,
        p.reset
    )
}

/// Startet das TUI — nur, wenn das Binary mit Feature `tui` gebaut wurde.
fn launch_tui(args: &Args) -> std::io::Result<()> {
    #[cfg(feature = "tui")]
    {
        agentkit::tui::run(agentkit::tui::TuiConfig {
            strategy: args.strategy,
            force_demo: args.demo,
            workspace: args.workspace.clone(),
            skills: args.skills.clone(),
            agents: args.agents.clone(),
            memory: args.memory.clone(),
            subagents: !args.no_subagents,
            max_steps: args.max_steps,
            ask_approval: !args.yes,
        })
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = args;
        eprintln!(
            "Dieses Build enthält kein TUI. Neu bauen mit `--features tui` \
             oder den REPL-/One-shot-Modus nutzen."
        );
        Ok(())
    }
}

fn print_help() {
    println!(
        "agentkit {VERSION} — Claude-Code-artiges CLI/TUI für den agentkit-Agenten\n\n\
         AUFRUF:\n  agentkit [OPTIONEN] [AUFTRAG …]\n\n\
         BETRIEBSARTEN:\n  \
           agentkit \"Frage\"        One-shot: Auftrag ausführen, Antwort streamen\n  \
           agentkit                 interaktive Session (REPL)\n  \
           agentkit --tui           interaktives Terminal-UI (nur mit Feature `tui`)\n\n\
         UNIX-PIPE:\n  \
           stdin  = Kontext (per Pipe), wird an die Query angehängt\n  \
           stdout = nur das finale Resultat (bei Pipe/--format json/--print)\n  \
           stderr = Status, Tool-Spur, ReAct-Gedanken, Fehler\n  \
           Exit:  0 Erfolg · 1 Laufzeit · 2 API/Netz · 3 Kontext/Prompt · 4 Format\n\n\
         OPTIONEN:\n  \
           -w, --workspace DIR   Sandbox-/Arbeitsverzeichnis (Default: .)\n  \
           -s, --strategy S      react | plan | plain (Default: react)\n  \
           --skills DIR          Skills-Verzeichnis aktivieren (SKILL.md-Ordner)\n  \
           --agents DIR          Custom-Sub-Agenten aus *.md laden (subagent_type)\n  \
           --memory FILE         Langzeitgedächtnis (JSONL) für remember/recall\n  \
           --provider P          auto | azure | openai | demo (Default: auto)\n  \
           --demo                Demo-Modus erzwingen (netzfrei)\n  \
           --max-steps N         Max. Loop-Schritte (Default: 160)\n  \
           --no-subagents        das 'task'-Tool deaktivieren\n  \
           -y, --yes             Shell-Befehle ohne Rückfrage ausführen\n  \
           --steps               Schritt-Grenzen anzeigen\n  \
           --no-color            Farbausgabe aus\n  \
           -p, --print           One-shot: nur finale Antwort ausgeben\n  \
           --format T            text | json (json: erzwingt + validiert strukturierten Output)\n  \
           --dry-run             zerstörerische Schreibvorgänge blockieren (nur stderr-Log)\n  \
           --max-context N       Kontext-Limit in Tokens (Default: 128000) -> sonst Exit 3\n  \
           --json-retries N      Versuche für gültiges JSON (Default: 3) -> sonst Exit 4\n  \
           --tui                 Terminal-UI (nur mit Feature `tui`)\n  \
           -h, --help / -V, --version\n\n\
         LLM-AUSWAHL (ohne --demo): AZURE_OPENAI_* -> Azure, OPENAI_API_KEY -> OpenAI, sonst Demo."
    );
}
