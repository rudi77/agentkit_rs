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
    build_coding_agent, build_task, classify_outcome, config_path, config_status,
    count_tokens_text, extract_json, init_user_config, is_likely_destructive, load_dotenv,
    load_user_config, new_cancel, read_stdin_context, render_steps, strategy_from_str, Agent,
    AgentEvent, AgentRole, CodingAgentConfig, EventBus, EventData, ExitCode, Llm, McpHub,
    OutputFormat, Plan, ShortTermMemory, Skills, Strategy, ToolRegistry, DONE, JSON_SYSTEM,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// --- Globaler Ctrl-C-Zustand: der Handler setzt den Stop-Knopf des laufenden Tasks.
static INT_COUNT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CANCEL: Mutex<Option<agentkit::Cancel>> = Mutex::new(None);

fn main() -> std::io::Result<()> {
    // Sauberer Unix-Filter: bei `… | head` soll SIGPIPE den Prozess beenden statt eines
    // Broken-Pipe-Panics (Rust setzt SIGPIPE beim Start auf SIG_IGN). No-op außer Unix.
    reset_sigpipe();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| argv.iter().any(|a| a == flag);

    // `agentkit completions <shell>` — Shell-Vervollständigungen ausgeben (bash/zsh/fish/
    // PowerShell). Muss VOR dem normalen Parsen laufen (eigenes Verb, kein Auftrag).
    if argv.first().map(String::as_str) == Some("completions") {
        return emit_completions(argv.get(1).map(String::as_str));
    }

    // `agentkit read-pdf <datei>` — deterministische, tokenfreie PDF-Textextraktion auf
    // stdout (komponierbar: `agentkit read-pdf x.pdf > text.txt`). Nur mit Feature `pdf`.
    if argv.first().map(String::as_str) == Some("read-pdf") {
        return emit_pdf_text(argv.get(1).map(String::as_str));
    }

    if has("-h") || has("--help") {
        print_help();
        return Ok(());
    }
    if has("-V") || has("--version") {
        println!("agentkit {VERSION}");
        return Ok(());
    }

    // Konfigurationsquellen, absteigende Priorität: echte Umgebung > `.env` im
    // Arbeitsverzeichnis > `~/.agentkit/config.json`. Beide Lader setzen nur, was noch
    // nicht gesetzt ist — die Reihenfolge hier *ist* die Rangfolge. Muss vor
    // `Args::parse` laufen, weil der Provider-Default aus der Umgebung kommt.
    load_dotenv();
    load_user_config();

    // `agentkit config [path|init|show]` — die Benutzer-Config anlegen/prüfen. Eigenes
    // Verb, kein Auftrag; braucht die geladene Umgebung (daher nach den Ladern).
    if argv.first().map(String::as_str) == Some("config") {
        return run_config_cmd(argv.get(1).map(String::as_str));
    }

    let args = Args::parse(&argv);

    // Farben: nur, wenn ein Terminal vorliegt und nicht --no-color (auf Windows VT aktivieren).
    // `NO_COLOR` (https://no-color.org/) schaltet Farben unabhängig vom Terminal ab.
    let color = !args.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal()
        && enable_vt();
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
    // Ausnahme: `--repl` erzwingt die interaktive Session und liest Kommandos (und
    // Folge-Antworten auf Rückfragen des Agenten) von stdin — auch wenn es kein
    // Terminal ist (scriptbar).
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdin_ctx = if stdin_is_tty || args.repl {
        None
    } else {
        read_stdin_context()?
    };
    let have_task = !args.prompt.trim().is_empty() || stdin_ctx.is_some();
    if !args.repl && (have_task || args.print_mode) {
        let code = run_oneshot(&args, pal, stdin_ctx);
        std::process::exit(code.code());
    }

    // Ohne Auftrag und ohne Terminal (leere Pipe) gibt es nichts zu tun -> Exit 3
    // (der REPL braucht ein interaktives stdin, außer bei erzwungenem --repl).
    if !stdin_is_tty && !args.repl {
        eprintln!("[ERROR] Kein Prompt übergeben und stdin lieferte keine Daten.");
        std::process::exit(ExitCode::ContextError.code());
    }

    // Interaktive Session (stdin ist ein Terminal, kein Auftrag). MCP interaktiv:
    // alle Server vorverbinden (connect_all), damit `/mcp on …` ohne Reconnect greift.
    let hub = build_mcp_hub(&args, true);
    let Built {
        mut agent,
        plan,
        skills,
        roles,
        hub,
        mcp_base,
    } = build_agent(&args, pal, hub);
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
        &hub,
        &mcp_base,
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
    /// REPL erzwingen (auch bei gepiptem stdin) — scriptbare interaktive Session inkl. HITL.
    repl: bool,
    // Unix-Pipe-Optionen.
    format: OutputFormat,
    dry_run: bool,
    max_context: usize,
    json_retries: u32,
    // MCP-Optionen.
    mcp_config: Option<String>,
    /// Allowlist: nur diese Server aktiv (leer = alle nicht-`disabled` aus der Config).
    mcp_enable: Vec<String>,
    no_mcp: bool,
    /// Agenten-spezifischer Zusatz-System-Prompt (aus `--system`/`--system-file`/`--profile`).
    system: Option<String>,
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
            // Default aus der Umgebung (gespeist u. a. aus `"provider"` in
            // `~/.agentkit/config.json`); `--provider` überschreibt ihn weiterhin.
            provider: std::env::var("AGENTKIT_PROVIDER").unwrap_or_else(|_| "auto".to_string()),
            demo: false,
            max_steps: 160,
            no_subagents: false,
            yes: false,
            steps: false,
            no_color: false,
            print_mode: false,
            tui: false,
            repl: false,
            format: OutputFormat::Text,
            dry_run: false,
            max_context: 128_000,
            json_retries: 3,
            mcp_config: None,
            mcp_enable: Vec::new(),
            no_mcp: false,
            system: None,
        };
        // `--flag=value` in zwei Tokens aufspalten und `--` als Ende-der-Optionen-Marker
        // respektieren (GNU/POSIX): so greifen `--workspace=/tmp` und Prompts, die mit
        // `-` beginnen (`agentkit -- "-n als Text"`).
        let norm = normalize_args(argv);
        // Profil ZUERST anwenden (Basis), damit explizite Flags danach gewinnen.
        if let Some(path) = find_flag_value(&norm, "--profile") {
            apply_profile(&mut a, &path);
        }
        let mut prompt: Vec<String> = Vec::new();
        let mut it = norm.iter().peekable();
        let mut literal = false; // alles nach `--` ist wörtlicher Auftrag
        while let Some(arg) = it.next() {
            if literal {
                prompt.push(arg.clone());
                continue;
            }
            if arg == "--" {
                literal = true;
                continue;
            }
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
                "--repl" => a.repl = true, // REPL erzwingen (auch bei gepiptem stdin)
                "--format" => a.format = parse_format(&take()),
                "--dry-run" => a.dry_run = true,
                "--max-context" => a.max_context = take().parse().unwrap_or(128_000),
                "--json-retries" => a.json_retries = take().parse().unwrap_or(3),
                "--mcp-config" => a.mcp_config = Some(take()),
                "--mcp" => {
                    let name = take();
                    if !name.is_empty() {
                        a.mcp_enable.push(name);
                    }
                }
                "--no-mcp" => a.no_mcp = true,
                "--system" => a.system = Some(take()),
                "--system-file" => match std::fs::read_to_string(take()) {
                    Ok(s) => a.system = Some(s),
                    Err(e) => eprintln!("[WARN] --system-file nicht lesbar: {e}"),
                },
                // Bereits vor der Schleife angewandt — hier nur den Wert konsumieren.
                "--profile" => {
                    let _ = take();
                }
                other if other.starts_with('-') => {
                    // Nicht still verschlucken: ein Tippfehler soll sichtbar sein (stderr).
                    eprintln!("[WARN] unbekannte Option ignoriert: {other}");
                }
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

/// Ersten Wert eines `--flag WERT`-Paars aus `argv` ziehen (für Optionen, die VOR der
/// Haupt-Schleife gebraucht werden, z. B. `--profile`). Erwartet ein bereits durch
/// [`normalize_args`] normalisiertes `argv` und ignoriert alles ab `--` (literaler Auftrag).
fn find_flag_value(argv: &[String], flag: &str) -> Option<String> {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    argv[..end]
        .iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1).cloned())
}

/// Bereitet `argv` fürs Parsen vor (GNU/POSIX-Konventionen):
/// - `--flag=value` wird zu den zwei Tokens `--flag`, `value` (nur Lang-Optionen).
/// - Ein alleinstehendes `--` bleibt erhalten (Ende-der-Optionen-Marker); alles danach
///   wird unverändert durchgereicht (wörtlicher Auftrag, auch wenn es mit `-` beginnt).
fn normalize_args(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut literal = false;
    for a in argv {
        if literal {
            out.push(a.clone());
            continue;
        }
        if a == "--" {
            literal = true;
            out.push(a.clone());
            continue;
        }
        if a.starts_with("--") && a.len() > 2 {
            if let Some((k, v)) = a.split_once('=') {
                out.push(k.to_string());
                out.push(v.to_string());
                continue;
            }
        }
        out.push(a.clone());
    }
    out
}

/// Auf Unix: SIGPIPE auf das Standardverhalten (SIG_DFL) zurücksetzen, damit ein
/// nachgeschaltetes `head`/`grep -q`, das die Pipe früh schließt, den Prozess sauber
/// per Signal beendet (Exit 141) statt eines Broken-Pipe-Panics beim nächsten Schreiben.
/// Rust setzt SIGPIPE beim Start auf SIG_IGN — für einen Unix-Filter ist SIG_DFL richtig.
#[cfg(unix)]
fn reset_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

/// Eine **Profil-Datei** (JSON) auf die Args anwenden — ein Config-Bündel je Agent, damit
/// eine Pipe-Stage mit `--profile stage.json "…"` auskommt statt vieler Einzel-Flags.
/// Bewusst dependency-frei über `serde_json::Value` geparst. Explizite CLI-Flags werden
/// NACH diesem Aufruf verarbeitet und überschreiben die Profilwerte.
///
/// Erkannte Felder (alle optional):
/// `system` (Text) / `system_file` (Pfad), `workspace`, `skills`, `agents`, `memory`,
/// `provider`, `strategy` (react|plan|plain), `max_steps`, `no_subagents`, `demo`,
/// `format` (text|json), `dry_run`, `mcp_config`, `mcp` (Liste), `no_mcp`.
fn apply_profile(a: &mut Args, path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[WARN] --profile nicht lesbar ({path}): {e}");
            return;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[WARN] --profile kein gültiges JSON ({path}): {e}");
            return;
        }
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());

    if let Some(sys) = s("system") {
        a.system = Some(sys);
    }
    if let Some(file) = s("system_file") {
        match std::fs::read_to_string(&file) {
            Ok(t) => a.system = Some(t),
            Err(e) => eprintln!("[WARN] --profile: system_file nicht lesbar ({file}): {e}"),
        }
    }
    if let Some(w) = s("workspace") {
        a.workspace = w;
    }
    if let Some(x) = s("skills") {
        a.skills = Some(x);
    }
    if let Some(x) = s("agents") {
        a.agents = Some(x);
    }
    if let Some(x) = s("memory") {
        a.memory = Some(x);
    }
    if let Some(x) = s("provider") {
        a.provider = x;
    }
    if let Some(x) = s("strategy") {
        a.strategy = strategy_from_str(&x);
    }
    if let Some(n) = v.get("max_steps").and_then(|x| x.as_u64()) {
        a.max_steps = n as usize;
    }
    if let Some(x) = b("no_subagents") {
        a.no_subagents = x;
    }
    if let Some(x) = b("demo") {
        a.demo = x;
    }
    if let Some(x) = s("format") {
        a.format = parse_format(&x);
    }
    if let Some(x) = b("dry_run") {
        a.dry_run = x;
    }
    if let Some(x) = s("mcp_config") {
        a.mcp_config = Some(x);
    }
    if let Some(list) = v.get("mcp").and_then(|x| x.as_array()) {
        for name in list.iter().filter_map(|x| x.as_str()) {
            a.mcp_enable.push(name.to_string());
        }
    }
    if let Some(x) = b("no_mcp") {
        a.no_mcp = x;
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
            EventData::Plan(steps) => {
                self.end_stream();
                self.put(&format!("{}📋 Plan{}", p.magenta, p.reset));
                for line in render_steps(steps, "\n").lines() {
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
                    // Lokale OpenAI-kompatible Server im Label kenntlich machen.
                    let label = match std::env::var("OPENAI_BASE_URL") {
                        Ok(base) if !base.trim().is_empty() => {
                            format!("openai:{model} @ {}", base.trim())
                        }
                        _ => format!("openai:{model}"),
                    };
                    return (Arc::new(llm), label);
                }
                Err(e) => eprintln!("openai_from_env: {e} — Demo-Fallback"),
            }
        }
    }
    // auto (oder Feature `openai` aus): Azure -> OpenAI -> Demo.
    agentkit::demo::build_llm(false)
}

/// Das Ergebnis von [`build_agent`]: der Agent plus die Begleitobjekte für die
/// Slash-Befehle und die MCP-Laufzeit-Umschaltung.
struct Built {
    agent: Agent,
    plan: Plan,
    skills: Option<Skills>,
    roles: Vec<AgentRole>,
    /// Geteilter MCP-Hub (auch fürs `task`-Tool); umschaltbar via `/mcp`.
    hub: Arc<McpHub>,
    /// MCP-freie Basis-Registry des Haupt-Agenten (Grundlage fürs Neu-Verdrahten).
    mcp_base: ToolRegistry,
}

/// Baut den MCP-Hub aus `.mcp.json` (explizit via `--mcp-config` oder per Discovery im
/// Workspace/CWD). `--no-mcp` -> leerer Hub (MCP ist sonst auch im Demo-Modus aktiv).
/// `connect_all` (REPL/TUI) verbindet auch deaktivierte Server vor, damit sie später ohne
/// Reconnect zuschaltbar sind; im One-shot (`false`) werden nur die aktiven verbunden.
/// Ergebnisse gehen nach stderr.
fn build_mcp_hub(args: &Args, connect_all: bool) -> Arc<McpHub> {
    // MCP ist unabhängig vom LLM — auch im Demo-Modus nutzbar; nur --no-mcp schaltet ab.
    if args.no_mcp {
        return Arc::new(McpHub::empty());
    }
    let hub = match McpHub::from_config(
        &args.workspace,
        args.mcp_config.as_deref(),
        &args.mcp_enable,
        connect_all,
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[WARN] MCP-Config: {e}");
            McpHub::empty()
        }
    };
    if hub.is_empty() {
        if !args.mcp_enable.is_empty() {
            eprintln!("[WARN] --mcp gesetzt, aber keine MCP-Server geladen.");
        }
        return Arc::new(hub);
    }
    eprintln!("» MCP: {} Server", hub.servers.len());
    for s in &hub.servers {
        match (&s.client, &s.error) {
            (Some(_), _) => eprintln!(
                "  ⏺ {} — {} Tools{}",
                s.name(),
                s.tool_count(),
                if s.is_enabled() { ", aktiv" } else { " (aus)" }
            ),
            (None, Some(e)) => eprintln!("  ✖ {} — nicht verbunden: {e}", s.name()),
            (None, None) => {}
        }
    }
    Arc::new(hub)
}

/// Stellt den Agenten zusammen: voller Coding-Agent (echter LLM) oder schlanker
/// Demo-Agent. Der `hub` (MCP) wird hereingereicht, damit der One-shot ihn EINMAL baut
/// und über JSON-Retries hinweg wiederverwendet (kein Reconnect je Versuch).
fn build_agent(args: &Args, pal: Pal, hub: Arc<McpHub>) -> Built {
    let (llm, label) = build_llm(&args.provider, args.demo);
    eprintln!("{}» Modell: {label}{}", pal.gray, pal.reset);

    // Demo-Modus: schlanker, netzfreier Agent — MCP-Tools werden dennoch eingeklinkt.
    if label.starts_with("demo") {
        let mut builder = Agent::builder(llm)
            .tools(demo_tools())
            .strategy(args.strategy)
            .max_steps(args.max_steps);
        if let Some(sys) = args
            .system
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            builder = builder.system(sys);
        }
        let mut agent = builder.build();
        let mcp_base = hub.apply(&mut agent);
        return Built {
            agent,
            plan: Plan::new(),
            skills: None,
            roles: Vec::new(),
            hub,
            mcp_base,
        };
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
        system: args.system.as_deref(),
    };
    let (agent, plan, skills, roles, mcp_base) =
        build_coding_agent(llm, &cfg, approve, hub.clone());
    Built {
        agent,
        plan,
        skills,
        roles,
        hub,
        mcp_base,
    }
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

    // MCP-Hub EINMAL bauen (One-shot: nur aktive Server verbinden) und über alle
    // JSON-Retries hinweg wiederverwenden — kein Reconnect je Versuch.
    let hub = build_mcp_hub(args, false);

    for attempt in 1..=attempts {
        if attempt > 1 {
            eprintln!("[INFO] JSON ungültig — neuer Versuch {attempt}/{attempts} …");
        }

        // Frischer Agent pro Versuch (sauberes Gedächtnis bei JSON-Retry).
        let mut agent = build_agent(args, pal, hub.clone()).agent;
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

#[allow(clippy::too_many_arguments)]
fn repl(
    agent: &mut Agent,
    plan: &Plan,
    skills: Option<&Skills>,
    roles: &[AgentRole],
    hub: &McpHub,
    mcp_base: &ToolRegistry,
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
            if !handle_slash(&user, agent, plan, skills, roles, hub, mcp_base, pal) {
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

#[allow(clippy::too_many_arguments)]
fn handle_slash(
    cmd: &str,
    agent: &mut Agent,
    plan: &Plan,
    skills: Option<&Skills>,
    roles: &[AgentRole],
    hub: &McpHub,
    mcp_base: &ToolRegistry,
    pal: Pal,
) -> bool {
    // In Kopf + Argumente zerlegen (für mehrwortige Befehle wie `/mcp on <name>`).
    let raw = cmd[1..].trim();
    let mut it = raw.split_whitespace();
    let head = it.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = it.collect();
    match head.as_str() {
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
        "mcp" => handle_mcp(&rest, agent, hub, mcp_base, pal),
        _ => println!(
            "{}Unbekannter Befehl: {cmd}{}  ({}/help{})",
            pal.red, pal.reset, pal.cyan, pal.reset
        ),
    }
    true
}

/// `/mcp` — MCP-Server auflisten bzw. für den Agenten ein-/ausschalten.
/// `/mcp` (Liste) · `/mcp on <name>` · `/mcp off <name>`.
fn handle_mcp(rest: &[&str], agent: &mut Agent, hub: &McpHub, mcp_base: &ToolRegistry, pal: Pal) {
    if hub.is_empty() {
        println!(
            "{}(keine MCP-Server — .mcp.json anlegen oder --mcp-config <datei> nutzen){}",
            pal.gray, pal.reset
        );
        return;
    }
    match rest {
        [] => {
            println!("{}MCP-Server:{}", pal.bold, pal.reset);
            for s in &hub.servers {
                let (mark, col) = if s.is_enabled() {
                    ("●", pal.green)
                } else if s.is_connected() {
                    ("○", pal.gray)
                } else {
                    ("✖", pal.red)
                };
                let info = match &s.error {
                    Some(e) => format!("nicht verbunden: {e}"),
                    None => format!("{} Tools", s.tool_count()),
                };
                println!("  {}{}{} {} — {}", col, mark, pal.reset, s.name(), info);
            }
            println!(
                "{}  /mcp on <name>  ·  /mcp off <name>{}",
                pal.gray, pal.reset
            );
        }
        [action, name]
            if matches!(
                action.to_lowercase().as_str(),
                "on" | "off" | "enable" | "disable"
            ) =>
        {
            let on = matches!(action.to_lowercase().as_str(), "on" | "enable");
            match hub.set_enabled(name, on) {
                Ok(_) => {
                    hub.rewire(agent, mcp_base);
                    let state = if on { "aktiv" } else { "aus" };
                    println!("{}✓ MCP '{name}' {state}.{}", pal.green, pal.reset);
                }
                Err(e) => println!("{}✖ {e}{}", pal.red, pal.reset),
            }
        }
        _ => println!("{}Nutzung: /mcp [on|off <name>]{}", pal.yellow, pal.reset),
    }
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
         {}/mcp{}       MCP-Server auflisten / ein-/ausschalten (/mcp on|off <name>)\n  \
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
            mcp_config: args.mcp_config.clone(),
            mcp_enable: args.mcp_enable.clone(),
            no_mcp: args.no_mcp,
            system: args.system.clone(),
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

// --------------------------------------------------------------------- config

/// `agentkit config [show|path|init]` — die Benutzer-Config unter `~/.agentkit/config.json`
/// anlegen und prüfen (das, was `agentkit_setup.ps1` bei der Installation schreibt).
///
/// `show` (Default) zeigt, welche Variablen die aktuelle Umgebung liefert — Keys
/// maskiert, damit die Ausgabe in einen Bug-Report kopiert werden kann. Exit 3, wenn
/// gar kein Anbieter konfiguriert ist (dann liefe nur der Demo-Modus).
fn run_config_cmd(sub: Option<&str>) -> std::io::Result<()> {
    let path = config_path();
    match sub {
        Some("path") => {
            match &path {
                Some(p) => println!("{}", p.display()),
                None => {
                    eprintln!("[ERROR] Kein Benutzerverzeichnis gefunden (USERPROFILE/HOME).");
                    std::process::exit(ExitCode::ContextError.code());
                }
            }
            Ok(())
        }
        Some("init") => match init_user_config() {
            Ok((p, true)) => {
                println!("Konfiguration angelegt: {}", p.display());
                println!("Trage dort deine Azure-Werte ein (endpoint, api_key, deployment).");
                Ok(())
            }
            Ok((p, false)) => {
                println!("Konfiguration existiert bereits: {}", p.display());
                Ok(())
            }
            Err(e) => {
                eprintln!("[ERROR] {e}");
                std::process::exit(ExitCode::GeneralError.code());
            }
        },
        None | Some("show") => {
            match &path {
                Some(p) if p.exists() => println!("Config-Datei : {}", p.display()),
                Some(p) => println!(
                    "Config-Datei : {} (fehlt — `agentkit config init`)",
                    p.display()
                ),
                None => println!("Config-Datei : — (kein USERPROFILE/HOME)"),
            }
            println!("\nWirksame Umgebung (echte Env > .env > config.json):");
            for line in config_status() {
                println!("  {line}");
            }
            let azure = std::env::var("AZURE_OPENAI_API_KEY").is_ok()
                && std::env::var("AZURE_OPENAI_ENDPOINT").is_ok()
                && std::env::var("AZURE_OPENAI_DEPLOYMENT").is_ok();
            let openai = std::env::var("OPENAI_API_KEY").is_ok();
            let local = std::env::var("OPENAI_BASE_URL")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            println!();
            if azure {
                println!("✓ Azure ist vollständig konfiguriert.");
            } else if local {
                println!("✓ Lokaler/kompatibler OpenAI-Server ist konfiguriert (base_url).");
            } else if openai {
                println!("✓ OpenAI ist konfiguriert (Azure unvollständig).");
            } else {
                eprintln!(
                    "! Kein Anbieter konfiguriert — agentkit liefe im Demo-Modus.\n  \
                     Trage endpoint, api_key und deployment in die config.json ein —\n  \
                     oder openai.base_url für einen lokalen Server (Ollama & Co.)."
                );
                std::process::exit(ExitCode::ContextError.code());
            }
            Ok(())
        }
        Some(other) => {
            eprintln!("Unbekannt: `config {other}`. Nutzung: agentkit config [show|path|init]");
            std::process::exit(ExitCode::ContextError.code());
        }
    }
}

// ------------------------------------------------------------------- read-pdf

/// `agentkit read-pdf <datei>` — extrahiert PDF-Text (kein LLM) und schreibt ihn auf
/// stdout. Fehlende Datei ⇒ Exit 3, Lesefehler ⇒ Exit 1. Ohne Feature `pdf` ⇒ Hinweis.
#[cfg(feature = "pdf")]
fn emit_pdf_text(path: Option<&str>) -> std::io::Result<()> {
    let Some(p) = path else {
        eprintln!("Nutzung: agentkit read-pdf <datei.pdf>");
        std::process::exit(ExitCode::ContextError.code());
    };
    match agentkit::extract_pdf_text(std::path::Path::new(p)) {
        Ok(text) => {
            println!("{text}");
            Ok(())
        }
        Err(e) => {
            eprintln!("[ERROR] {e}");
            std::process::exit(ExitCode::GeneralError.code());
        }
    }
}

#[cfg(not(feature = "pdf"))]
fn emit_pdf_text(_path: Option<&str>) -> std::io::Result<()> {
    eprintln!(
        "Dieses Build hat kein PDF-Support. Neu bauen mit `--features pdf` \
         (z. B. cargo install --path . --bin agentkit --features \"pdf tui\")."
    );
    std::process::exit(ExitCode::GeneralError.code());
}

// ------------------------------------------------------------ Shell-Completions

/// `agentkit completions <shell>` — gibt ein Vervollständigungs-Skript auf stdout aus, das
/// in die jeweilige Shell eingebunden wird (siehe README/INSTALL). Unbekannte/fehlende
/// Shell ⇒ Hinweis auf stderr und Exit 3.
fn emit_completions(shell: Option<&str>) -> std::io::Result<()> {
    let script = match shell.map(|s| s.to_lowercase()) {
        Some(ref s) if s == "bash" => COMPLETIONS_BASH,
        Some(ref s) if s == "zsh" => COMPLETIONS_ZSH,
        Some(ref s) if s == "fish" => COMPLETIONS_FISH,
        Some(ref s) if s == "powershell" || s == "pwsh" => COMPLETIONS_PWSH,
        other => {
            eprintln!(
                "Nutzung: agentkit completions <bash|zsh|fish|powershell>{}",
                other
                    .map(|s| format!("\n[ERROR] unbekannte Shell: {s}"))
                    .unwrap_or_default()
            );
            std::process::exit(ExitCode::ContextError.code());
        }
    };
    print!("{script}");
    Ok(())
}

/// Gemeinsame Optionsliste (für die bash-`compgen`-Vervollständigung).
const COMPLETIONS_BASH: &str = r#"# bash-Vervollständigung für agentkit.
# Einbinden:  source <(agentkit completions bash)
# Dauerhaft:  agentkit completions bash > /etc/bash_completion.d/agentkit
_agentkit() {
    local cur prev opts
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    opts="-w --workspace -s --strategy --skills --agents --memory --provider --demo \
--max-steps --plan --plain --react --no-subagents -y --yes --steps --no-color -p --print \
--tui --repl --format --dry-run --max-context --json-retries --mcp-config --mcp --no-mcp \
--system --system-file --profile -h --help -V --version"
    # Erstes Wort: auch die Verben `completions`/`read-pdf`/`config` anbieten.
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "completions read-pdf config $opts" -- "$cur") )
        return 0
    fi
    case "$prev" in
        completions) COMPREPLY=( $(compgen -W "bash zsh fish powershell" -- "$cur") ); return 0;;
        read-pdf) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
        config) COMPREPLY=( $(compgen -W "show path init" -- "$cur") ); return 0;;
        -s|--strategy) COMPREPLY=( $(compgen -W "react plan plain" -- "$cur") ); return 0;;
        --provider) COMPREPLY=( $(compgen -W "auto azure openai demo" -- "$cur") ); return 0;;
        --format) COMPREPLY=( $(compgen -W "text json" -- "$cur") ); return 0;;
        -w|--workspace|--skills|--agents) COMPREPLY=( $(compgen -d -- "$cur") ); return 0;;
        --memory|--mcp-config|--system-file|--profile) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
    esac
    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
    else
        COMPREPLY=( $(compgen -f -- "$cur") )
    fi
}
complete -F _agentkit agentkit
"#;

const COMPLETIONS_ZSH: &str = r#"#compdef agentkit
# zsh-Vervollständigung für agentkit.
# Einbinden:  agentkit completions zsh > "${fpath[1]}/_agentkit"  (dann `compinit`)
_agentkit() {
    local -a opts
    opts=(
        '-w[Arbeitsverzeichnis]:dir:_files -/'
        '--workspace[Arbeitsverzeichnis]:dir:_files -/'
        '-s[Strategie]:strategy:(react plan plain)'
        '--strategy[Strategie]:strategy:(react plan plain)'
        '--skills[Skills-Verzeichnis]:dir:_files -/'
        '--agents[Custom-Rollen-Verzeichnis]:dir:_files -/'
        '--memory[Langzeitgedächtnis (JSONL)]:file:_files'
        '--provider[LLM-Anbieter]:provider:(auto azure openai demo)'
        '--demo[Demo-Modus erzwingen]'
        '--max-steps[Max. Loop-Schritte]:n:'
        '--plan[Plan-Strategie]'
        '--plain[Plain-Strategie]'
        '--react[ReAct-Strategie]'
        '--no-subagents[task-Tool deaktivieren]'
        '-y[Shell ohne Rückfrage]'
        '--yes[Shell ohne Rückfrage]'
        '--steps[Schritt-Grenzen anzeigen]'
        '--no-color[Farbe aus]'
        '-p[Nur finale Antwort]'
        '--print[Nur finale Antwort]'
        '--tui[Terminal-UI]'
        '--repl[Interaktive Session]'
        '--format[Ausgabeformat]:format:(text json)'
        '--dry-run[Schreibvorgänge blockieren]'
        '--max-context[Kontext-Limit (Tokens)]:n:'
        '--json-retries[JSON-Versuche]:n:'
        '--mcp-config[MCP-Config]:file:_files'
        '--mcp[MCP-Server-Allowlist]:name:'
        '--no-mcp[MCP aus]'
        '--system[Zusatz-System-Prompt]:text:'
        '--system-file[System-Prompt-Datei]:file:_files'
        '--profile[Config-Bündel (JSON)]:file:_files'
        '-h[Hilfe]'
        '--help[Hilfe]'
        '-V[Version]'
        '--version[Version]'
        '*:Auftrag:_files'
    )
    _arguments -s $opts
}
_agentkit "$@"
"#;

const COMPLETIONS_FISH: &str = r#"# fish-Vervollständigung für agentkit.
# Einbinden:  agentkit completions fish > ~/.config/fish/completions/agentkit.fish
complete -c agentkit -f
complete -c agentkit -n '__fish_use_subcommand' -a completions -d 'Shell-Vervollständigung ausgeben'
complete -c agentkit -n '__fish_use_subcommand' -a read-pdf -d 'PDF-Text extrahieren (kein LLM)'
complete -c agentkit -n '__fish_use_subcommand' -a config -d 'Konfiguration pruefen/anlegen'
complete -c agentkit -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell'
complete -c agentkit -n '__fish_seen_subcommand_from config' -a 'show path init'
complete -c agentkit -s w -l workspace -r -d 'Arbeitsverzeichnis'
complete -c agentkit -s s -l strategy -x -a 'react plan plain' -d 'Strategie'
complete -c agentkit -l skills -r -d 'Skills-Verzeichnis'
complete -c agentkit -l agents -r -d 'Custom-Rollen-Verzeichnis'
complete -c agentkit -l memory -r -d 'Langzeitgedächtnis (JSONL)'
complete -c agentkit -l provider -x -a 'auto azure openai demo' -d 'LLM-Anbieter'
complete -c agentkit -l demo -d 'Demo-Modus erzwingen'
complete -c agentkit -l max-steps -x -d 'Max. Loop-Schritte'
complete -c agentkit -l plan -d 'Plan-Strategie'
complete -c agentkit -l plain -d 'Plain-Strategie'
complete -c agentkit -l react -d 'ReAct-Strategie'
complete -c agentkit -l no-subagents -d 'task-Tool deaktivieren'
complete -c agentkit -s y -l yes -d 'Shell ohne Rückfrage'
complete -c agentkit -l steps -d 'Schritt-Grenzen anzeigen'
complete -c agentkit -l no-color -d 'Farbe aus'
complete -c agentkit -s p -l print -d 'Nur finale Antwort'
complete -c agentkit -l tui -d 'Terminal-UI'
complete -c agentkit -l repl -d 'Interaktive Session'
complete -c agentkit -l format -x -a 'text json' -d 'Ausgabeformat'
complete -c agentkit -l dry-run -d 'Schreibvorgänge blockieren'
complete -c agentkit -l max-context -x -d 'Kontext-Limit (Tokens)'
complete -c agentkit -l json-retries -x -d 'JSON-Versuche'
complete -c agentkit -l mcp-config -r -d 'MCP-Config'
complete -c agentkit -l mcp -x -d 'MCP-Server-Allowlist'
complete -c agentkit -l no-mcp -d 'MCP aus'
complete -c agentkit -l system -x -d 'Zusatz-System-Prompt'
complete -c agentkit -l system-file -r -d 'System-Prompt-Datei'
complete -c agentkit -l profile -r -d 'Config-Bündel (JSON)'
complete -c agentkit -s h -l help -d 'Hilfe'
complete -c agentkit -s V -l version -d 'Version'
"#;

const COMPLETIONS_PWSH: &str = r#"# PowerShell-Vervollständigung für agentkit.
# Einbinden:  agentkit completions powershell | Out-String | Invoke-Expression
# Dauerhaft:  agentkit completions powershell >> $PROFILE
Register-ArgumentCompleter -Native -CommandName agentkit -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $opts = @(
        'completions','read-pdf','config','-w','--workspace','-s','--strategy','--skills','--agents','--memory',
        '--provider','--demo','--max-steps','--plan','--plain','--react','--no-subagents',
        '-y','--yes','--steps','--no-color','-p','--print','--tui','--repl','--format',
        '--dry-run','--max-context','--json-retries','--mcp-config','--mcp','--no-mcp',
        '--system','--system-file','--profile','-h','--help','-V','--version'
    )
    $tokens = $commandAst.CommandElements
    # Bei nachfolgendem Leerzeichen ist $wordToComplete leer -> das vorherige Wort ist das
    # LETZTE Element; beim Teilwort das VORLETZTE. Sonst greift die Werte-Completion nicht.
    if ([string]::IsNullOrEmpty($wordToComplete)) {
        $prev = if ($tokens.Count -ge 1) { $tokens[$tokens.Count - 1].ToString() } else { '' }
    } else {
        $prev = if ($tokens.Count -ge 2) { $tokens[$tokens.Count - 2].ToString() } else { '' }
    }
    $values = switch ($prev) {
        'completions' { @('bash','zsh','fish','powershell') }
        'config'      { @('show','path','init') }
        '-s'          { @('react','plan','plain') }
        '--strategy'  { @('react','plan','plain') }
        '--provider'  { @('auto','azure','openai','demo') }
        '--format'    { @('text','json') }
        default       { $opts }
    }
    $values | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}
"#;

fn print_help() {
    println!(
        "agentkit {VERSION} — Claude-Code-artiges CLI/TUI für den agentkit-Agenten\n\n\
         AUFRUF:\n  agentkit [OPTIONEN] [AUFTRAG …]\n\n\
         BETRIEBSARTEN:\n  \
           agentkit \"Frage\"        One-shot: Auftrag ausführen, Antwort streamen\n  \
           agentkit                 interaktive Session (REPL)\n  \
           agentkit --tui           interaktives Terminal-UI (nur mit Feature `tui`)\n  \
           agentkit config          Konfiguration prüfen (show|path|init) — ~/.agentkit/config.json\n  \
           agentkit completions SH  Shell-Completion ausgeben (bash|zsh|fish|powershell)\n  \
           agentkit read-pdf FILE   PDF-Text extrahieren auf stdout (kein LLM; Feature `pdf`)\n\n\
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
           --mcp-config FILE     MCP-Server aus .mcp.json laden (sonst Auto-Discovery)\n  \
           --mcp NAME            nur diesen MCP-Server aktiv (mehrfach möglich)\n  \
           --no-mcp              MCP komplett deaktivieren\n  \
           --system TEXT         agenten-spezifischer Zusatz-System-Prompt (Pipe-Stage)\n  \
           --system-file FILE    System-Prompt aus Datei (überschreibt --system)\n  \
           --profile FILE        Config-Bündel (JSON) je Agent; explizite Flags gewinnen\n  \
           --tui                 Terminal-UI (nur mit Feature `tui`)\n  \
           --repl                interaktive Session erzwingen (auch bei gepiptem stdin; scriptbar)\n  \
           -h, --help / -V, --version\n\n\
         HUMAN-IN-THE-LOOP: Im REPL/TUI stellt der Agent eine Rückfrage einfach als Antwort und\n  \
           beendet seinen Zug; deine nächste Eingabe beantwortet sie, und er macht mit vollem\n  \
           Gesprächsverlauf weiter — kein Sonderwerkzeug nötig. `--repl` macht die Session scriptbar\n  \
           (Kommandos + Folge-Antworten via stdin).\n\n\
         MCP: .mcp.json im Format {{\"mcpServers\": {{name: {{command, args, env, disabled}}}}}}.\n  \
           Tools erscheinen namespaced als mcp__<server>__<tool>. Im REPL/TUI live umschaltbar.\n\n\
         LLM-AUSWAHL (ohne --demo): AZURE_OPENAI_* -> Azure, OPENAI_API_KEY oder OPENAI_BASE_URL\n  \
           -> OpenAI(-kompatibel), sonst Demo. Lokale Server (Ollama, LM Studio, vLLM, …):\n  \
           OPENAI_BASE_URL=http://localhost:11434/v1 + OPENAI_MODEL setzen; API-Key optional."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_splits_long_flag_equals() {
        assert_eq!(
            normalize_args(&v(&["--workspace=/tmp", "--format=json"])),
            v(&["--workspace", "/tmp", "--format", "json"])
        );
    }

    #[test]
    fn normalize_keeps_plain_flag_value_pairs() {
        assert_eq!(
            normalize_args(&v(&["--workspace", "/tmp", "-p"])),
            v(&["--workspace", "/tmp", "-p"])
        );
    }

    #[test]
    fn normalize_treats_everything_after_double_dash_as_literal() {
        // Nach `--` wird `--foo=bar` NICHT gespalten und `-p` bleibt wörtlich.
        assert_eq!(
            normalize_args(&v(&["-p", "--", "-p", "--foo=bar"])),
            v(&["-p", "--", "-p", "--foo=bar"])
        );
    }

    #[test]
    fn parse_flag_equals_is_applied() {
        let a = Args::parse(&v(&["--workspace=/tmp", "--format=json", "hallo"]));
        assert_eq!(a.workspace, "/tmp");
        assert_eq!(a.format, OutputFormat::Json);
        assert_eq!(a.prompt, "hallo");
    }

    #[test]
    fn parse_double_dash_prompt_starting_with_dash() {
        let a = Args::parse(&v(&["-p", "--", "-p", "als", "text"]));
        assert!(a.print_mode);
        assert_eq!(a.prompt, "-p als text");
    }

    #[test]
    fn find_flag_value_stops_at_double_dash() {
        let n = normalize_args(&v(&["--", "--profile", "x.json"]));
        assert_eq!(find_flag_value(&n, "--profile"), None);
        let n2 = normalize_args(&v(&["--profile", "x.json", "--", "rest"]));
        assert_eq!(
            find_flag_value(&n2, "--profile"),
            Some("x.json".to_string())
        );
    }
}
