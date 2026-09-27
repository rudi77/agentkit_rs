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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::coding::{ApproveFn, CodingTools};
use agentkit::demo::demo_tools;
use agentkit::{
    build_coding_agent, build_task, classify_outcome, config_path, config_status,
    count_tokens_text, extract_json, init_user_config, load_dotenv, load_user_config, new_cancel,
    read_stdin_context, render_steps, run_strategy_from_str, run_with_strategy, strategy_from_str,
    Agent, AgentEvent, AgentRole, CodingAgentConfig, EventBus, EventData, ExitCode, Llm, McpHub,
    OutputFormat, Plan, RewindOutcome, RunStrategy, ShortTermMemory, Skills, Strategy,
    ToolRegistry, TraceWriter, Usage, DONE, JSON_SYSTEM,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// --- Globaler Ctrl-C-Zustand: der Handler setzt den Stop-Knopf des laufenden Tasks.
static INT_COUNT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_CANCEL: Mutex<Option<agentkit::Cancel>> = Mutex::new(None);
/// `--timeout` ist abgelaufen — setzt der Wächter-Thread, der Exit-Code wird dann 124.
static TIMED_OUT: AtomicBool = AtomicBool::new(false);
/// `-o DATEI`: wohin das Resultat geht (`None` = stdout). Die Datei wird erst
/// beim ersten Schreiben angelegt — ein gescheiterter Lauf lässt eine
/// vorhandene Datei unangetastet, wie `sort -o`.
static OUTPUT: Mutex<Option<OutputFile>> = Mutex::new(None);

struct OutputFile {
    path: String,
    file: Option<std::fs::File>,
}

fn main() -> std::io::Result<()> {
    // Sauberer Unix-Filter: bei `… | head` soll SIGPIPE den Prozess beenden statt eines
    // Broken-Pipe-Panics (Rust setzt SIGPIPE beim Start auf SIG_IGN). No-op außer Unix.
    reset_sigpipe();

    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // Eigener Befehl: als Symlink (`summarize` → agentkit) oder `agentkit run NAME`.
    // Vor allem anderen, weil `run NAME` die ersten beiden Tokens verbraucht.
    let command = match resolve_command(&mut argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ERROR] {e}");
            std::process::exit(ExitCode::ContextError.code());
        }
    };
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

    // `work` und `viz` haben ihre EIGENE Hilfe (im jeweiligen Verb-Dispatch) —
    // der globale Scan hier würde sonst `agentkit work --help` abfangen, BEVOR
    // der Verb-Dispatch weiter unten überhaupt läuft.
    let eigenes_help_verb = matches!(argv.first().map(String::as_str), Some("work") | Some("viz"));
    if !eigenes_help_verb && (has("-h") || has("--help")) {
        print_help();
        return Ok(());
    }
    if has("-V") || has("--version") {
        println!("agentkit {VERSION}");
        return Ok(());
    }

    // `agentkit --upgrade [VERSION]` — Selbst-Update der installierten Binary.
    // Wie `-V`/`--version` ein reines Prozess-Kommando, kein Auftrag an das
    // Modell — muss also ebenfalls VOR `Args::parse` laufen. Das optionale
    // Versions-Argument ist nur das UNMITTELBAR folgende Token, wenn es nicht
    // mit `-` beginnt (sonst wäre `agentkit --upgrade --dry-run` mehrdeutig).
    if let Some(pos) = argv.iter().position(|a| a == "--upgrade") {
        let gewuenscht = argv
            .get(pos + 1)
            .filter(|a| !a.starts_with('-'))
            .map(String::as_str);
        return run_upgrade_cmd(gewuenscht);
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

    // `agentkit work <unterkommando>` — die persistente Arbeits-Runtime
    // (agentkit-work, Feature `work`). Eigenes Verb, kein Auftrag; braucht die
    // geladene Umgebung wie `config` (Provider-Default), und muss VOR
    // `Args::parse` laufen, weil die Work-CLI eine eigene, unabhängige
    // Argument-Grammatik hat (siehe `agentkit_work::cli`) statt der von `Args`.
    if argv.first().map(String::as_str) == Some("work") {
        return run_work_cmd(&argv[1..]);
    }

    // `agentkit viz` — der Betrachter für den `--trace`-Ereignisstrom
    // (agentkit-viz, Feature `viz`). Wie `work` ein eigenes Verb mit eigener
    // Argument-Grammatik, also vor `Args::parse`.
    if argv.first().map(String::as_str) == Some("viz") {
        return run_viz_cmd(&argv[1..]);
    }

    // `agentkit mcp-serve` / `agentkit acp` — agentkit als Server für andere
    // Agenten bzw. Editoren. Eigene Verben, aber mit der gewohnten
    // Options-Grammatik (Workspace, Provider, Graph, …) — daher nach den
    // Konfigurations-Ladern und mit `Args::parse` auf dem Rest.
    if argv.first().map(String::as_str) == Some("mcp-serve") {
        return run_mcp_serve(&argv[1..]);
    }
    if argv.first().map(String::as_str) == Some("acp") {
        return run_acp(&argv[1..]);
    }

    let mut args = Args::parse_with(&argv, command.as_ref());
    // `-q` als Erstes: ab hier soll keine Zeile mehr auf stderr landen.
    if args.quiet {
        silence_stderr();
    }
    apply_hooks_flag(&args);
    if let Some(path) = args.output.clone() {
        *OUTPUT.lock().unwrap() = Some(OutputFile { path, file: None });
    }

    // Farben: nur, wenn ein Terminal vorliegt und nicht --no-color (auf Windows VT aktivieren).
    // `NO_COLOR` (https://no-color.org/) schaltet Farben unabhängig vom Terminal ab.
    let color = !args.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal()
        && enable_vt();
    let pal = if color { Pal::color() } else { Pal::plain() };

    // One-shot (`-p`) hat keinen Verlauf zum Fortsetzen — ein still ignoriertes
    // Flag wäre schlimmer als eine Absage.
    if (args.continue_last || args.resume) && args.print_mode {
        eprintln!("[WARN] --continue/--resume wirken nur interaktiv — hier ignoriert.");
    }

    if args.tui {
        // Auswahl VOR ratatui::init(), solange das Terminal noch normal ist:
        // `--resume` druckt eine Liste und liest eine Zahl. Bewusst nur die
        // *gewählte* Datei — das TUI legt ohne Flag keine Sitzung an.
        if args.session.is_none() && std::io::stdin().is_terminal() {
            args.session = chosen_session(&args, pal);
            if args.session.is_none() && (args.continue_last || args.resume) {
                eprintln!("» Keine frühere Sitzung gefunden — starte ohne Verlauf.");
            }
        }
        // Das TUI behandelt Ctrl-C selbst als Taste (Raw-Mode); der REPL-Handler unten
        // würde dort nur bei einem externen SIGINT feuern und den Prozess beenden, OHNE
        // das Terminal wiederherzustellen. Stattdessen: wiederherstellen, dann Exit 130.
        #[cfg(feature = "tui")]
        let _ = ctrlc::set_handler(|| {
            agentkit::tui::restore_terminal();
            std::process::exit(130);
        });
        if args.trace.is_some() {
            eprintln!("[WARN] --trace wirkt nicht im TUI — es hat seinen eigenen Ereignis-Loop.");
        }
        return launch_tui(&args);
    }

    // Stop-Knopf: Ctrl-C bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).
    install_ctrlc_handler();

    // Trace-Sink: schreibt den kompletten Ereignisstrom als NDJSON mit. Wird
    // EINMAL für den ganzen Prozess angelegt (One-shot wie REPL), damit ein
    // Betrachter alle Züge einer Sitzung in einer Datei findet.
    let trace = args.trace.as_deref().and_then(open_trace);

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
    let have_task = !args.prompt.trim().is_empty() || stdin_ctx.is_some() || !args.files.is_empty();
    if !args.repl && (have_task || args.print_mode || args.each) {
        if let Err(e) = check_pipe_flags(&args) {
            eprintln!("[ERROR] {e}");
            std::process::exit(ExitCode::ContextError.code());
        }
        start_watchdog(args.timeout);
        let code = if args.each {
            run_each(&args, pal, stdin_ctx, trace.as_ref())
        } else if args.patch {
            run_patch(&args, pal, stdin_ctx, trace.as_ref())
        } else {
            run_oneshot(&args, pal, stdin_ctx, trace.as_ref())
        };
        // Ein abgelaufenes `--timeout` gewinnt: der Lauf endete deshalb, egal
        // wie der abgebrochene Auftrag sich selbst eingeordnet hat.
        let code = if TIMED_OUT.load(Ordering::SeqCst) {
            ExitCode::Timeout
        } else {
            code
        };
        std::process::exit(code.code());
    }

    // Ohne Auftrag und ohne Terminal (leere Pipe) gibt es nichts zu tun -> Exit 3
    // (der REPL braucht ein interaktives stdin, außer bei erzwungenem --repl).
    if !stdin_is_tty && !args.repl {
        eprintln!("[ERROR] Kein Prompt übergeben und stdin lieferte keine Daten.");
        std::process::exit(ExitCode::ContextError.code());
    }

    // Sitzung JETZT festlegen — vor build_agent: `graph_run_id` bindet den
    // Graph-Arbeitsstand an `args.session`, und ein `--continue`-Lauf soll
    // seinen Stand wiederfinden. Genau EIN Feld trägt die Antwort.
    args.session = resolve_session(&args, pal, stdin_is_tty);

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
        model_label,
        perms,
        coding,
    } = build_agent(&args, pal, hub, None);
    let mut renderer = Renderer {
        show_steps: args.steps,
        quiet: false,
        streaming: false,
        pal,
        to_stderr: false,
        // `color` ist nur wahr, wenn stdout ein Terminal ist und Farben
        // erlaubt sind — genau die Bedingung, unter der ANSI-Auszeichnung
        // Sinn ergibt.
        md: color.then(|| MarkdownStream::new(pal)),
        spinner: true,
        stream_json: false,
    };
    println!("{}", banner(&args, pal));
    if let Some(path) = args.session.as_deref() {
        load_session(&mut agent, path);
    }
    let ctx = ReplCtx {
        plan: &plan,
        skills: skills.as_ref(),
        roles: &roles,
        hub: &hub,
        mcp_base: &mcp_base,
        pal,
        session: args.session.as_deref(),
        workspace: &args.workspace,
        model_label: &model_label,
        notify: args.notify,
        perms: &perms,
        coding: coding.as_ref(),
        trace: trace.as_ref(),
        run_strategy: args.run_strategy,
        token_limit: args.token_limit,
    };
    // `stdin_is_tty` kommt von oben: die Entscheidung „Skript oder Mensch"
    // gehört zum stdin-Kontrakt und wird nur EINMAL getroffen.
    repl(&mut agent, &mut renderer, &ctx, stdin_is_tty);
    Ok(())
}

/// Legt `--hooks FILE` als `AGENTKIT_HOOKS` in die Umgebung — daraus lesen
/// alle Bauwege der Coding-Tools (siehe `agentkit::hooks`).
fn apply_hooks_flag(args: &Args) {
    if let Some(hooks) = args.hooks.as_deref() {
        // Absolut machen: der Workspace (`-w`) ist nicht das Verzeichnis, in
        // dem der Nutzer den Pfad getippt hat.
        let pfad = std::fs::canonicalize(hooks).unwrap_or_else(|_| PathBuf::from(hooks));
        if !pfad.is_file() {
            eprintln!("[WARN] --hooks: Datei nicht gefunden ({hooks})");
        }
        std::env::set_var(agentkit::HOOKS_ENV, pfad);
    }
}

/// Die Freigabe-Policy ohne Rückfrage-Kanal: `-y` und die dauerhafte
/// Allowlist aus `config.json` gelten, alles andere wird abgelehnt.
fn policy_ohne_rueckfrage(yes: bool) -> ApproveFn {
    let perms = Permissions::aus_umgebung(yes);
    Arc::new(move |cmd: &str| !perms.fragt_nach(cmd))
}

/// `agentkit mcp-serve [--expose-tools] [OPTIONEN]`: agentkit als MCP-Server
/// auf stdio. Standardmäßig EIN Tool — `agentkit`, das einen Auftrag an einen
/// frisch gebauten Coding-Agenten delegiert. `--expose-tools` stellt zusätzlich
/// dessen Werkzeuge direkt bereit (Sandbox, Git, mit `--graph` der Graph).
fn run_mcp_serve(rest: &[String]) -> std::io::Result<()> {
    let expose = rest.iter().any(|a| a == "--expose-tools");
    let rest: Vec<String> = rest
        .iter()
        .filter(|a| *a != "--expose-tools")
        .cloned()
        .collect();
    let args = Arc::new(Args::parse_with(&rest, None));
    apply_hooks_flag(&args);
    if !args.yes {
        eprintln!(
            "[INFO] mcp-serve: run_shell läuft nur für Programme aus der allow-Liste — \
             stdin gehört dem Protokoll, eine Rückfrage ist nicht möglich. -y erlaubt alles."
        );
    }
    let pal = Pal::plain();
    let hub = build_mcp_hub(&args, false);
    let approve = policy_ohne_rueckfrage(args.yes);
    let mut registry = if expose {
        build_agent(&args, pal, hub.clone(), Some(approve.clone()))
            .agent
            .tools
    } else {
        ToolRegistry::new()
    };
    let delegate_args = args.clone();
    registry.add(
        "agentkit",
        "Delegiert eine Aufgabe an den agentkit-Coding-Agenten. Er arbeitet \
         selbstständig im Workspace (Dateien lesen/ändern, Shell, Git, Sub-Agenten) \
         und liefert seine abschließende Antwort. Jeder Aufruf beginnt ohne Gedächtnis.",
        serde_json::json!({"type": "object", "properties": {
            "prompt": {"type": "string", "description": "Der Auftrag, vollständig formuliert."}},
            "required": ["prompt"]}),
        move |v: serde_json::Value| {
            let task = v["prompt"].as_str().unwrap_or("").trim().to_string();
            if task.is_empty() {
                return Err("'prompt' fehlt".to_string());
            }
            let agent = build_agent(&delegate_args, pal, hub.clone(), Some(approve.clone())).agent;
            // Die Spur des Agenten geht auf stderr — stdout gehört dem Protokoll.
            let mut renderer = Renderer {
                show_steps: false,
                quiet: false,
                streaming: false,
                pal,
                to_stderr: true,
                md: None,
                spinner: false,
                stream_json: false,
            };
            let (_, final_, hard_error, _) = run_task(
                agent,
                &task,
                &mut renderer,
                None,
                delegate_args.run_strategy,
                &TokenBudget::new(delegate_args.token_limit),
                None,
            );
            match classify_outcome(&final_, hard_error) {
                Some(_) => Err(final_),
                None => Ok(final_),
            }
        },
    );
    install_ctrlc_handler();
    agentkit::mcp_server::serve(
        &registry,
        std::io::stdin().lock(),
        std::io::stdout(),
        VERSION,
    )
}

/// `agentkit acp [OPTIONEN]`: agentkit als Agent für Editoren, die das Agent
/// Client Protocol sprechen (z. B. Zed). Jede Editor-Sitzung bekommt ihren
/// eigenen Agenten im Projektverzeichnis des Editors; Shell-Freigaben fragt der
/// Editor (außer bei `-y` bzw. für Programme aus der allow-Liste).
fn run_acp(rest: &[String]) -> std::io::Result<()> {
    let args = Args::parse_with(rest, None);
    apply_hooks_flag(&args);
    let hub = build_mcp_hub(&args, false);
    let strategy = args.run_strategy;
    let factory: agentkit::acp::AgentFactory = Arc::new(
        move |cwd: &str, frage: ApproveFn| -> Result<Agent, String> {
            let mut sitzung = args.clone();
            sitzung.workspace = cwd.to_string();
            let policy = policy_ohne_rueckfrage(args.yes);
            let approve: ApproveFn = Arc::new(move |cmd: &str| policy(cmd) || frage(cmd));
            Ok(build_agent(&sitzung, Pal::plain(), hub.clone(), Some(approve)).agent)
        },
    );
    agentkit::acp::serve(
        factory,
        strategy,
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        VERSION,
    );
    Ok(())
}

/// Richtet den Stop-Knopf ein: Ctrl-C bricht die laufende Aufgabe kooperativ ab
/// (zweimal = Prozess sofort beenden, Exit 130). Zwei Aufrufstellen teilen sich
/// diese Closure — der REPL-/One-shot-Pfad in `main` und `run_work_cmd` (der
/// Work-Runner reagiert kooperativ auf denselben Stop-Knopf und schreibt dann
/// einen Checkpoint) — deshalb eine Funktion statt zweier Kopien.
fn install_ctrlc_handler() {
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
}

// ------------------------------------------------------- Unix-Werkzeug-Rahmen

/// Kombinationen der Pipe-Optionen, die sich widersprechen — lieber eine klare
/// Absage (Exit 3) als ein still ignoriertes Flag.
fn check_pipe_flags(args: &Args) -> Result<(), String> {
    if args.check && args.schema.is_some() {
        return Err(
            "--check und --schema schließen sich aus (--check hat sein eigenes Schema).".into(),
        );
    }
    if args.patch && args.each {
        return Err("--patch und --each schließen sich aus.".into());
    }
    if args.patch && args.stream_json {
        return Err(
            "--patch und --format stream-json schließen sich aus (stdout trägt den Diff).".into(),
        );
    }
    if args.jobs > 1 && !args.each {
        eprintln!("[WARN] -j wirkt nur zusammen mit --each — hier ignoriert.");
    }
    Ok(())
}

/// `-q`: stderr auf das Null-Gerät umlenken. Auf Ebene des Dateideskriptors
/// statt über ein Flag an jedem `eprintln!` — so bleibt auch keine Meldung aus
/// einer Bibliothek, einem Tool oder einem Panic übrig.
#[cfg(unix)]
fn silence_stderr() {
    extern "C" {
        fn open(path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        fn dup2(old: i32, new: i32) -> i32;
    }
    const O_WRONLY: i32 = 1;
    unsafe {
        let fd = open(b"/dev/null\0".as_ptr().cast(), O_WRONLY);
        if fd >= 0 {
            dup2(fd, 2);
        }
    }
}

/// Windows: Rusts stderr holt den Handle bei jedem Schreiben neu
/// (`GetStdHandle`) — ihn auf `NUL` umzusetzen reicht.
#[cfg(windows)]
fn silence_stderr() {
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const std::ffi::c_void,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn SetStdHandle(which: u32, handle: isize) -> i32;
    }
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x3;
    const OPEN_EXISTING: u32 = 3;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // -12
    let name: Vec<u16> = "NUL\0".encode_utf16().collect();
    unsafe {
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if h != -1 {
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn silence_stderr() {}

/// Wie lange ein abgelaufenes `--timeout` auf den kooperativen Abbruch wartet,
/// bevor der Prozess hart endet.
const TIMEOUT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// `--timeout`: ein Wächter-Thread drückt nach Ablauf den Stop-Knopf. Reagiert
/// der Lauf nicht (ein Tool, das den Knopf nie prüft), endet der Prozess nach
/// [`TIMEOUT_GRACE`] hart — mit Exit 124 wie `timeout(1)`.
fn start_watchdog(limit: Option<std::time::Duration>) {
    let Some(limit) = limit else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        TIMED_OUT.store(true, Ordering::SeqCst);
        eprintln!(
            "[WARN] --timeout abgelaufen ({:.0} s) — Lauf wird abgebrochen.",
            limit.as_secs_f64()
        );
        if let Some(c) = CURRENT_CANCEL.lock().unwrap().clone() {
            c.store(true, Ordering::SeqCst);
        }
        std::thread::sleep(TIMEOUT_GRACE);
        eprintln!("[ERROR] Der Lauf reagiert nicht auf den Abbruch — Prozess wird beendet.");
        std::process::exit(ExitCode::Timeout.code());
    });
}

/// Schreibt eine Zeile Resultat: nach `-o DATEI`, sonst auf stdout. Eine
/// Stelle für alles, was stdout trägt (Antwort, JSONL, Ereignisstrom, Diff).
fn out_line(s: &str) -> std::io::Result<()> {
    let mut ziel = OUTPUT.lock().unwrap();
    match ziel.as_mut() {
        None => {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{s}")?;
            out.flush()
        }
        Some(of) => {
            if of.file.is_none() {
                of.file = Some(std::fs::File::create(&of.path)?);
            }
            let f = of.file.as_mut().expect("eben angelegt");
            writeln!(f, "{s}")?;
            f.flush()
        }
    }
}

// ------------------------------------------------------------ Eigene Befehle

/// Wo eigene Befehle liegen: `<config_dir>/commands/NAME.md`.
fn command_path(name: &str) -> Option<PathBuf> {
    Some(
        agentkit::config_dir()?
            .join("commands")
            .join(format!("{name}.md")),
    )
}

/// Erkennt einen eigenen Befehl und liefert sein Profil.
///
/// Zwei Wege: der Programmname (ein Symlink `summarize` → `agentkit`, für den
/// es `commands/summarize.md` gibt) oder das Verb `agentkit run NAME` (dann
/// werden `run NAME` aus `argv` entfernt). Ein Programmname OHNE Befehlsdatei
/// ist kein Fehler — dann ist es eben agentkit unter anderem Namen.
fn resolve_command(argv: &mut Vec<String>) -> Result<Option<serde_json::Value>, String> {
    let invoked = std::env::args_os().next().and_then(|a| {
        Path::new(&a)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
    });
    if let Some(name) = invoked.filter(|n| n != "agentkit" && valid_command_name(n)) {
        if let Some(path) = command_path(&name).filter(|p| p.is_file()) {
            return load_command(&path).map(Some);
        }
    }
    if argv.first().map(String::as_str) != Some("run") {
        return Ok(None);
    }
    argv.remove(0);
    if argv.is_empty() || argv[0].starts_with('-') {
        print_commands();
        std::process::exit(ExitCode::Success.code());
    }
    let name = argv.remove(0);
    if !valid_command_name(&name) {
        return Err(format!("ungültiger Befehlsname »{name}«"));
    }
    match command_path(&name).filter(|p| p.is_file()) {
        Some(path) => load_command(&path).map(Some),
        None => Err(format!(
            "unbekannter Befehl »{name}« — erwartet: {} (Liste: agentkit run)",
            command_path(&name)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.agentkit/commands/NAME.md".into())
        )),
    }
}

/// Nur schlichte Namen — `run ../x` darf keine Datei außerhalb lesen.
fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..")
}

fn load_command(path: &Path) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    Ok(command_profile(&text, base))
}

/// Übersetzt eine Befehlsdatei in ein Profil (dieselben Schlüssel wie
/// `--profile`): der Frontmatter liefert die Einstellungen, der Text darunter
/// den System-Prompt. `true`/`false` und Zahlen werden typisiert, `mcp` und
/// `allow_read` sind Komma-Listen, relative `schema`/`system_file`-Pfade
/// gelten relativ zur Befehlsdatei — der Befehl läuft ja aus jedem Verzeichnis.
fn command_profile(text: &str, base: &Path) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in agentkit::parse_frontmatter(text) {
        let v = match key.as_str() {
            "mcp" | "allow_read" => serde_json::Value::Array(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(|x| serde_json::Value::String(x.to_string()))
                    .collect(),
            ),
            "schema" | "system_file" if Path::new(&value).is_relative() => {
                serde_json::Value::String(base.join(&value).to_string_lossy().to_string())
            }
            "tools" | "model" | "system" => serde_json::Value::String(value),
            _ => match value.as_str() {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                n if n.parse::<u64>().is_ok() => serde_json::json!(n.parse::<u64>().unwrap()),
                _ => serde_json::Value::String(value),
            },
        };
        map.insert(key, v);
    }
    let body = agentkit::body_after_frontmatter(text).trim();
    if !body.is_empty() {
        map.insert("system".into(), serde_json::Value::String(body.to_string()));
    }
    serde_json::Value::Object(map)
}

/// `agentkit run` ohne Namen: die vorhandenen Befehle auflisten.
fn print_commands() {
    let dir = agentkit::config_dir().map(|d| d.join("commands"));
    let mut namen: Vec<String> = dir
        .as_ref()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md") {
                Some(p.file_stem()?.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    namen.sort();
    if namen.is_empty() {
        println!(
            "Keine eigenen Befehle. Anlegen: {}/NAME.md (Frontmatter = Einstellungen, \
             Text = System-Prompt), dann `agentkit run NAME` oder `ln -s $(which agentkit) NAME`.",
            dir.map(|d| d.display().to_string())
                .unwrap_or_else(|| "~/.agentkit/commands".into())
        );
    }
    for n in namen {
        println!("{n}");
    }
}

// ------------------------------------------------------------------- Argumente

#[derive(Clone)]
struct Args {
    prompt: String,
    workspace: String,
    strategy: Strategy,
    /// Wie der Auftrag ausgeführt wird (`-s plan_execute` → Phasen-Treiber);
    /// `strategy` daneben bleibt das Preamble der einzelnen Läufe.
    run_strategy: RunStrategy,
    skills: Option<String>,
    agents: Option<String>,
    /// `--agents-only`: geladene Rollen ERSETZEN die eingebauten.
    agents_only: bool,
    /// `--sub-rules TEXT`: wenige Sätze, die für jeden Sub-Agenten gelten.
    sub_rules: Option<String>,
    memory: Option<String>,
    provider: String,
    demo: bool,
    max_steps: usize,
    /// Selbstverifikation vor der finalen Antwort (`--verify`).
    verify: bool,
    /// Timeout (Sekunden) für run_shell (`--shell-timeout`, Default 120).
    shell_timeout: u64,
    no_subagents: bool,
    /// Das `swarm`-Tool abschalten (`--no-swarm`).
    no_swarm: bool,
    /// Projekt-Instruktionen (`AGENTS.md`) laden (`--no-project-instructions`
    /// setzt es auf `false`) — siehe
    /// [`agentkit::CodingAgentConfig::project_instructions`].
    project_instructions: bool,
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
    /// Session-Datei: Verlauf wird daraus geladen und nach jedem Auftrag dorthin
    /// gespeichert — Resume über Prozessgrenzen (One-shot-Ketten UND REPL).
    session: Option<String>,
    /// `--continue`/`-c`: die jüngste automatisch gespeicherte Sitzung dieses
    /// Projekts fortsetzen (statt `--session <datei>` von Hand).
    continue_last: bool,
    /// `--notify`: Glocke + Desktop-Meldung, wenn ein langer Auftrag fertig
    /// ist oder eine Freigabe wartet.
    notify: bool,
    /// `--model NAME`: überschreibt das Modell aus der Umgebung. Wird vor dem
    /// Bauen des LLM auf `OPENAI_MODEL` bzw. `AZURE_OPENAI_DEPLOYMENT`
    /// abgebildet — derselbe Weg, den auch `~/.agentkit/config.json` nimmt,
    /// statt ein zweites Modell-Konzept einzuführen.
    model: Option<String>,
    /// `--resume`: die Sitzungen dieses Projekts auflisten und auswählen lassen.
    /// Nimmt bewusst KEINEN Pfad — dafür gibt es `--session <datei>`.
    resume: bool,
    /// ctxman-Zustandsverzeichnis (`--ctx DIR`, nur mit Feature `ctxman`): aktiviert
    /// das volle Context-Management (Watermarks/GC/Externalisierung + Snapshot-Resume).
    ctx: Option<String>,
    /// Modell-Kontext-Budget B für ctxman (Tokens).
    ctx_budget: u32,
    /// Partielles Policy-Overlay als JSON-Datei (`--ctx-policy FILE`).
    ctx_policy: Option<String>,
    /// Separates Compaction-LLM (`--ctx-compaction-model NAME` — Azure-Deployment
    /// bzw. OpenAI-Modellname aus derselben Provider-Umgebung).
    ctx_compaction_model: Option<String>,
    /// Graph-Verzeichnis (`--graph DIR`, nur mit Feature `graph`): schaltet den
    /// Wissensgraphen frei (graph_search/-neighbors/-evidence/-remember/-promote).
    graph: Option<String>,
    /// `--graph-readonly`: der Agent darf den Graphen lesen, aber nicht schreiben.
    graph_readonly: bool,
    /// `--graph-scope ID`: Arbeits-Scope des Graphen. Läufe mit derselben ID
    /// teilen ihr vorläufiges Wissen; ohne Angabe bekommt jeder Lauf einen
    /// eigenen (siehe `graph_run_id`).
    graph_scope: Option<String>,
    /// `--protect-paths MUSTER[,MUSTER…]`: Pfade, die `write_file`/`edit_file`
    /// nicht anfassen dürfen (siehe [`agentkit::CodingTools::with_protected_paths`]).
    protect_paths: Vec<String>,
    /// `--allow-read DIR` (wiederholbar): zusätzliche NUR-LESBARE Sandbox-Wurzeln
    /// (siehe [`agentkit::CodingTools::with_read_roots`]). Bewusst read-only statt
    /// den Workspace (`-w`) weiter zu fassen — die Schreib-Sandbox soll eng
    /// bleiben. Öffnet keine neue Angriffsfläche: `run_shell` ist ohnehin nicht
    /// pfadbeschränkt, eine Shell in der Sandbox konnte diese Dateien schon immer
    /// per `cat`/`type` lesen.
    allow_read: Vec<String>,
    /// Trace-Verzeichnis (`--trace DIR`): schreibt den kompletten Ereignisstrom
    /// des Laufs als NDJSON dorthin — die Datengrundlage für `agentkit viz`.
    /// Ohne dieses Flag entsteht KEINE Datei (siehe `agentkit::trace`).
    trace: Option<String>,
    /// `--token-limit N`: bricht den Lauf ab, sobald die GEMESSENEN Tokens
    /// (Eingabe + Ausgabe, alle Agenten zusammen) N übersteigen. `None` = kein
    /// Limit.
    token_limit: Option<u64>,
    /// `--hooks FILE`: zusätzliche Hook-Datei (siehe `agentkit::hooks`). Wird
    /// als `AGENTKIT_HOOKS` in die Umgebung gelegt — daraus lesen alle
    /// Bauwege der Coding-Tools.
    hooks: Option<String>,
    // Unix-Werkzeug-Optionen (siehe `run_job`/`run_each`).
    /// `--format stream-json`: jedes Ereignis als JSON-Zeile auf stdout, zum
    /// Schluss ein `result`-Datensatz. Ein eigenes Feld statt einer weiteren
    /// [`OutputFormat`]-Variante: die Enum ist öffentliche API, und
    /// agentkit-work matcht sie erschöpfend.
    stream_json: bool,
    /// `--tools none|NAME,…`: `none` = ein reiner Modell-Aufruf ohne Werkzeuge,
    /// eine Liste = nur diese Werkzeuge (`read_only` und Claude-Code-Namen wie
    /// bei Rollen, siehe [`agentkit::parse_tools_field`]). `None` = alle.
    tools: Option<String>,
    /// `--check`: Ja/Nein-Prüfung — Exit 0 bei Ja, 1 bei Nein, Begründung auf stderr.
    check: bool,
    /// `--schema FILE`: Antwort als JSON nach diesem Schema (erzwingt JSON-Ausgabe).
    schema: Option<String>,
    /// `--each`: jede stdin-Zeile ist ein eigener Auftrag, Ausgabe als JSONL.
    each: bool,
    /// `-j N`: so viele `--each`-Aufträge gleichzeitig (Default 1).
    jobs: usize,
    /// `--patch`: auf einer Kopie arbeiten und einen Unified Diff ausgeben.
    patch: bool,
    /// `--cache DIR`: Ergebnisse gleicher Aufträge wiederverwenden.
    cache: Option<String>,
    /// `--timeout DAUER`: Obergrenze der Laufzeit, danach Exit 124.
    timeout: Option<std::time::Duration>,
    /// `-f DATEI` (wiederholbar): Dateien mit Namen als Kontext.
    files: Vec<String>,
    /// `-o DATEI`: das Resultat in diese Datei statt auf stdout.
    output: Option<String>,
    /// `-q`: stderr komplett stumm (Exit-Code trägt die Information).
    quiet: bool,
}

impl Args {
    /// Setzt BEIDE Strategie-Felder aus einem Konfigurationswert: das Preamble
    /// (`strategy`) und den Ausführungs-Treiber (`run_strategy`). Eine Stelle
    /// für Flag und Profil, damit die zwei Felder nicht auseinanderlaufen —
    /// z. B. muss ein explizites `--react` ein `"strategy": "plan_execute"`
    /// aus dem Profil vollständig überstimmen.
    fn set_strategy(&mut self, value: &str) {
        self.strategy = strategy_from_str(value);
        self.run_strategy = run_strategy_from_str(value);
    }

    /// `--format`-Wert: `text`, `json` oder `stream-json`.
    fn set_format(&mut self, value: &str) {
        self.stream_json = matches!(
            value.trim().to_lowercase().as_str(),
            "stream-json" | "stream_json"
        );
        self.format = parse_format(value);
    }

    /// `--tools none` (oder leer): gar keine Werkzeuge, ein reiner Modell-Aufruf.
    fn tools_none(&self) -> bool {
        self.tools
            .as_deref()
            .is_some_and(|t| t.trim().is_empty() || t.trim().eq_ignore_ascii_case("none"))
    }

    /// Antwortet der Lauf als JSON? (`--format json`, `--schema`, `--check`)
    fn structured(&self) -> bool {
        self.format == OutputFormat::Json || self.schema.is_some() || self.check
    }

    #[cfg(test)]
    fn parse(argv: &[String]) -> Args {
        Args::parse_with(argv, None)
    }

    /// Wie `parse`, mit einem Basis-Profil darunter — ein eigener Befehl
    /// (`agentkit run NAME`, Symlink), dessen Werte `--profile` und explizite
    /// Flags überstimmen.
    fn parse_with(argv: &[String], base: Option<&serde_json::Value>) -> Args {
        let mut a = Args {
            prompt: String::new(),
            workspace: ".".to_string(),
            strategy: Strategy::React,
            run_strategy: RunStrategy::Direct(Strategy::React),
            skills: None,
            agents: None,
            agents_only: false,
            sub_rules: None,
            memory: None,
            // Default aus der Umgebung (gespeist u. a. aus `"provider"` in
            // `~/.agentkit/config.json`); `--provider` überschreibt ihn weiterhin.
            provider: std::env::var("AGENTKIT_PROVIDER").unwrap_or_else(|_| "auto".to_string()),
            demo: false,
            max_steps: 600,
            verify: false,
            shell_timeout: 120,
            no_subagents: false,
            no_swarm: false,
            project_instructions: true,
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
            session: None,
            model: None,
            notify: false,
            continue_last: false,
            resume: false,
            ctx: None,
            ctx_budget: 100_000,
            ctx_policy: None,
            ctx_compaction_model: None,
            graph: None,
            graph_readonly: false,
            graph_scope: None,
            protect_paths: Vec::new(),
            allow_read: Vec::new(),
            trace: None,
            token_limit: None,
            hooks: None,
            tools: None,
            check: false,
            schema: None,
            each: false,
            jobs: 1,
            patch: false,
            cache: None,
            timeout: None,
            files: Vec::new(),
            output: None,
            quiet: false,
            stream_json: false,
        };
        // `--flag=value` in zwei Tokens aufspalten und `--` als Ende-der-Optionen-Marker
        // respektieren (GNU/POSIX): so greifen `--workspace=/tmp` und Prompts, die mit
        // `-` beginnen (`agentkit -- "-n als Text"`).
        let norm = normalize_args(argv);
        // Profil ZUERST anwenden (Basis), damit explizite Flags danach gewinnen.
        if let Some(v) = base {
            apply_profile_value(&mut a, v);
        }
        if let Some(path) = find_flag_value(&norm, "--profile") {
            apply_profile(&mut a, &path);
        }
        let mut prompt: Vec<String> = Vec::new();
        let mut it = norm.iter().peekable();
        let mut literal = false; // alles nach `--` ist wörtlicher Auftrag
                                 // `--skills` ist als einziges Flag mehrfach angebbar und verkettet seine
                                 // Werte. Gegenüber dem PROFIL muss es sich trotzdem verhalten wie jedes
                                 // andere Flag: ersetzen, nicht anhängen. Ohne diese Merker-Variable hinge
                                 // die erste `--skills`-Angabe an den Profilwert an, und der ließe sich
                                 // über die Kommandozeile gar nicht mehr abwählen.
        let mut skills_von_flag = false;
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
                "-s" | "--strategy" => a.set_strategy(&take()),
                "--skills" => {
                    // Mehrfach angebbar: Werte intern mit `;` verketten (Skills::new
                    // erschließt mehrere Wurzelverzeichnisse getrennt durch `;`).
                    // Verkettet wird aber nur mit VORHERIGEN `--skills`-Flags — ein
                    // Profilwert wird von der ersten Angabe ersetzt.
                    let v = take();
                    a.skills = Some(match a.skills.take() {
                        Some(prev) if skills_von_flag => format!("{prev};{v}"),
                        _ => v,
                    });
                    skills_von_flag = true;
                }
                "--agents" => a.agents = Some(take()),
                "--agents-only" => a.agents_only = true,
                "--sub-rules" => a.sub_rules = Some(take()),
                "--memory" => a.memory = Some(take()),
                "--provider" => a.provider = take(),
                "--max-steps" => a.max_steps = take().parse().unwrap_or(600),
                "--verify" => a.verify = true,
                "--shell-timeout" => a.shell_timeout = take().parse().unwrap_or(120),
                "--plan" => a.set_strategy("plan"),
                "--plain" => a.set_strategy("plain"),
                "--react" => a.set_strategy("react"),
                "--demo" => a.demo = true,
                "--no-subagents" => a.no_subagents = true,
                "--no-swarm" => a.no_swarm = true,
                "--no-project-instructions" => a.project_instructions = false,
                "-y" | "--yes" => a.yes = true,
                "--steps" => a.steps = true,
                "--no-color" => a.no_color = true,
                "-p" | "--print" => a.print_mode = true,
                "--tui" => a.tui = true,
                "--repl" => a.repl = true, // REPL erzwingen (auch bei gepiptem stdin)
                "--format" => a.set_format(&take()),
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
                "--session" => a.session = Some(take()),
                "--model" => a.model = Some(take()),
                "--notify" => a.notify = true,
                "--continue" | "-c" => a.continue_last = true,
                // `--resume` nimmt optional einen Pfad: das nächste Token gehört
                // nur dazu, wenn es kein weiteres Flag ist.
                // `--resume <datei>` ist nichts anderes als `--session <datei>`;
                // ohne Pfad die Auswahlliste.
                "--resume" => match it.peek().filter(|s| !s.starts_with('-')) {
                    Some(p) => {
                        a.session = Some((*p).clone());
                        it.next();
                    }
                    None => a.resume = true,
                },
                "--ctx" => a.ctx = Some(take()),
                "--ctx-budget" => a.ctx_budget = take().parse().unwrap_or(100_000),
                "--ctx-policy" => a.ctx_policy = Some(take()),
                "--ctx-compaction-model" => a.ctx_compaction_model = Some(take()),
                "--graph" => a.graph = Some(take()),
                "--graph-readonly" => a.graph_readonly = true,
                "--graph-scope" => a.graph_scope = Some(take()),
                "--protect-paths" => {
                    a.protect_paths = take()
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                "--allow-read" => {
                    let dir = take();
                    if !dir.is_empty() {
                        a.allow_read.push(dir);
                    }
                }
                "--trace" => a.trace = Some(take()),
                "--token-limit" => a.token_limit = take().parse().ok().filter(|n| *n > 0),
                "--hooks" => a.hooks = Some(take()),
                "--tools" => a.tools = Some(take()),
                "--check" => a.check = true,
                "--schema" => a.schema = Some(take()),
                "--each" => a.each = true,
                "-j" | "--jobs" => a.jobs = parse_jobs(&take()),
                "--patch" => a.patch = true,
                "--cache" => a.cache = Some(take()),
                "--timeout" => {
                    let v = take();
                    a.timeout = parse_duration(&v);
                    if a.timeout.is_none() {
                        eprintln!(
                            "[WARN] --timeout: ungültige Dauer »{v}« (z. B. 90, 30s, 5m, 1h)"
                        );
                    }
                }
                "-f" | "--file" => {
                    let f = take();
                    if !f.is_empty() {
                        a.files.push(f);
                    }
                }
                "-o" | "--output" => a.output = Some(take()),
                "-q" | "--quiet" => a.quiet = true,
                // `-j8` wie bei make/xargs, ohne Leerzeichen.
                other if other.starts_with("-j") && other[2..].parse::<usize>().is_ok() => {
                    a.jobs = parse_jobs(&other[2..])
                }
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
        // Ohne Werkzeuge gibt es nichts zu planen und nichts zu begründen: ein
        // ReAct- oder Plan-Preamble würde von Tools reden, die es nicht gibt.
        if a.tools_none() {
            a.set_strategy("plain");
        }
        a
    }
}

/// `-j N` → mindestens 1.
fn parse_jobs(s: &str) -> usize {
    s.trim().parse::<usize>().unwrap_or(1).max(1)
}

/// Dauer für `--timeout`: Sekunden (auch mit Nachkommastellen) oder mit
/// Einheit `s`/`m`/`h` wie bei `timeout(1)`. `None` bei Unsinn oder 0.
fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (zahl, faktor) = match s.char_indices().last()? {
        (i, 's') => (&s[..i], 1.0),
        (i, 'm') => (&s[..i], 60.0),
        (i, 'h') => (&s[..i], 3600.0),
        _ => (s, 1.0),
    };
    let secs = zahl.trim().parse::<f64>().ok()? * faktor;
    (secs.is_finite() && secs > 0.0).then(|| std::time::Duration::from_secs_f64(secs))
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
/// `provider`, `strategy` (react|plan|plain|plan_execute), `strategy_params`
/// (Objekt: `max_plan_steps`, `plan_max_steps`, `step_max_steps`, `reflect`,
/// `max_rework_per_step`, `max_replans` — nur für `plan_execute`), `max_steps`,
/// `no_subagents`,
/// `no_project_instructions`, `demo`, `format` (text|json), `dry_run`,
/// `mcp_config`, `mcp` (Liste), `no_mcp`, `tools`, `schema`, `check`, `timeout`,
/// `cache`, `model`.
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
    apply_profile_value(a, &v);
}

/// Wendet ein bereits geparstes Profil an — gemeinsam für `--profile FILE` und
/// eigene Befehle (`~/.agentkit/commands/NAME.md`).
fn apply_profile_value(a: &mut Args, v: &serde_json::Value) {
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
        a.set_strategy(&x);
    }
    // Feintuning für `"strategy": "plan_execute"` — bei jeder anderen
    // Strategie wirkungslos, gewarnt wird nicht (ein Profil darf die Werte
    // vorhalten und die Strategie separat umschalten).
    if let Some(p) = v.get("strategy_params").and_then(|x| x.as_object()) {
        if let RunStrategy::PlanExecute(params) = &mut a.run_strategy {
            let n = |k: &str| p.get(k).and_then(|x| x.as_u64()).map(|x| x as usize);
            if let Some(x) = n("max_plan_steps") {
                params.max_plan_steps = x;
            }
            if let Some(x) = n("plan_max_steps") {
                params.plan_max_steps = x;
            }
            if let Some(x) = n("step_max_steps") {
                params.step_max_steps = x;
            }
            if let Some(x) = n("max_rework_per_step") {
                params.max_rework_per_step = x;
            }
            if let Some(x) = n("max_replans") {
                params.max_replans = x;
            }
            if let Some(x) = p.get("reflect").and_then(|x| x.as_bool()) {
                params.reflect = x;
            }
        }
    }
    if let Some(n) = v.get("max_steps").and_then(|x| x.as_u64()) {
        a.max_steps = n as usize;
    }
    if let Some(x) = s("hooks") {
        a.hooks = Some(x);
    }
    if let Some(n) = v.get("token_limit").and_then(|x| x.as_u64()) {
        a.token_limit = Some(n).filter(|n| *n > 0);
    }
    if let Some(x) = b("no_subagents") {
        a.no_subagents = x;
    }
    if let Some(x) = b("no_swarm") {
        a.no_swarm = x;
    }
    if let Some(x) = b("no_project_instructions") {
        a.project_instructions = !x;
    }
    if let Some(x) = b("verify") {
        a.verify = x;
    }
    if let Some(n) = v.get("shell_timeout").and_then(|x| x.as_u64()) {
        a.shell_timeout = n;
    }
    if let Some(x) = b("demo") {
        a.demo = x;
    }
    if let Some(x) = s("format") {
        a.set_format(&x);
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
    // `tools` als Text (`"none"`, `"read_file, grep"`) oder als Liste.
    match v.get("tools") {
        Some(serde_json::Value::String(t)) => a.tools = Some(t.clone()),
        Some(serde_json::Value::Array(xs)) => {
            let names: Vec<&str> = xs.iter().filter_map(|x| x.as_str()).collect();
            a.tools = Some(if names.is_empty() {
                "none".to_string()
            } else {
                names.join(",")
            });
        }
        _ => {}
    }
    if let Some(x) = s("schema") {
        a.schema = Some(x);
    }
    if let Some(x) = b("check") {
        a.check = x;
    }
    if let Some(x) = s("cache") {
        a.cache = Some(x);
    }
    if let Some(x) = s("model") {
        a.model = Some(x);
    }
    match v.get("timeout") {
        Some(serde_json::Value::Number(n)) => {
            a.timeout = n.as_f64().and_then(|f| parse_duration(&f.to_string()))
        }
        Some(serde_json::Value::String(t)) => a.timeout = parse_duration(t),
        _ => {}
    }
    if let Some(x) = s("session") {
        a.session = Some(x);
    }
    if let Some(x) = s("ctx") {
        a.ctx = Some(x);
    }
    if let Some(n) = v.get("ctx_budget").and_then(|x| x.as_u64()) {
        a.ctx_budget = n as u32;
    }
    if let Some(x) = s("graph") {
        a.graph = Some(x);
    }
    if let Some(x) = b("graph_readonly") {
        a.graph_readonly = x;
    }
    // Wie `mcp`: Werte werden APPENDIERT, nicht ersetzt — Profil und ein
    // späteres `--allow-read` sollen sich addieren.
    // Leere Einträge werden verworfen wie beim CLI-Flag: ein leerer Pfad ist
    // Präfix von JEDEM Pfad und würde die Lese-Sandbox ganz aufheben.
    if let Some(list) = v.get("allow_read").and_then(|x| x.as_array()) {
        for dir in list.iter().filter_map(|x| x.as_str()) {
            if !dir.trim().is_empty() {
                a.allow_read.push(dir.to_string());
            }
        }
    }
}

// --------------------------------------------------------- Session-Persistenz

/// Lädt eine `--session`-Datei in den Agenten (falls vorhanden und nicht leer).
/// Der gespeicherte Verlauf ersetzt das frische Gedächtnis KOMPLETT — inklusive
/// des damaligen System-Prompts, damit der Resume exakt dort weitermacht, wo die
/// letzte Sitzung endete. Fehlt in der Datei ein System-Prompt, bleibt der frische.
/// Welche Sitzungsdatei gilt für diesen interaktiven Lauf?
///
/// Reihenfolge: explizites `--session` schlägt alles (`--resume <datei>` landet
/// beim Parsen schon dort); `--resume` lässt aus der Liste wählen,
/// `--continue` nimmt die jüngste. Sonst wird automatisch eine neue angelegt —
/// aber **nur am Terminal**: Skripte (`-p`, oder `--repl` mit gepiptem stdin,
/// wie die Benchmark-Pipeline) sollen keine Dateien hinterlassen.
fn resolve_session(args: &Args, pal: Pal, stdin_is_tty: bool) -> Option<String> {
    if args.session.is_some() {
        return args.session.clone();
    }
    if !stdin_is_tty {
        // Kein Mensch da: weder auswählen lassen noch stillschweigend anlegen.
        return None;
    }
    if let Some(pfad) = chosen_session(args, pal) {
        return Some(pfad);
    }
    if args.resume || args.continue_last {
        eprintln!("» Keine frühere Sitzung übernommen — eine neue wird angelegt.");
    }

    // Auto-Sitzung: der Verlauf ist damit auch ohne Flag wiederauffindbar.
    agentkit::new_session_path(&args.workspace).map(|p| p.to_string_lossy().to_string())
}

/// Die per `--resume`/`--continue` gewählte Sitzungsdatei — ohne den Rückfall
/// auf eine frisch angelegte. Getrennt von [`resolve_session`], weil das TUI
/// genau diesen Teil braucht: es soll eine *gewählte* Sitzung fortsetzen, aber
/// nicht ungefragt anfangen, Sitzungsdateien anzulegen.
fn chosen_session(args: &Args, pal: Pal) -> Option<String> {
    if args.resume {
        let sitzungen = agentkit::list_sessions(&args.workspace);
        if sitzungen.is_empty() {
            return None;
        }
        print_sessions(&sitzungen, pal);
        frage_sitzung(&sitzungen, pal)
    } else if args.continue_last {
        agentkit::latest_session(&args.workspace).map(|p| p.to_string_lossy().to_string())
    } else {
        None
    }
}

/// Sitzungsliste ausgeben (`--resume`, `/sessions`).
fn print_sessions(sitzungen: &[agentkit::SessionInfo], pal: Pal) {
    println!("{}Sitzungen dieses Projekts{}", pal.bold, pal.reset);
    for (n, s) in sitzungen.iter().enumerate() {
        let zuege = if s.turns == 1 { "Zug" } else { "Züge" };
        println!(
            "  {}{:>3}{}  {:<14} {:>3} {zuege:<5} {}",
            pal.cyan,
            n + 1,
            pal.reset,
            agentkit::relatives_alter(s.modified),
            s.turns,
            s.title
        );
    }
}

/// Nummer abfragen (leer = keine Auswahl). Nur sinnvoll am Terminal.
fn frage_sitzung(sitzungen: &[agentkit::SessionInfo], pal: Pal) -> Option<String> {
    eprint!("{}Nummer (Enter = neue Sitzung): {}", pal.gray, pal.reset);
    let _ = std::io::stderr().flush();
    let mut zeile = String::new();
    std::io::stdin().read_line(&mut zeile).ok()?;
    let n: usize = zeile.trim().parse().ok()?;
    sitzungen
        .get(n.checked_sub(1)?)
        .map(|s| s.path.to_string_lossy().to_string())
}

fn load_session(agent: &mut Agent, path: &str) {
    match ShortTermMemory::load(path) {
        Ok(mut loaded) if !loaded.messages.is_empty() => {
            // Trägt die Datei keinen System-Prompt (z. B. ein von Hand
            // gekürzter Export), den frisch gebauten voranstellen.
            let has_system = loaded.messages.iter().any(|m| m["role"] == "system");
            if !has_system {
                if let Some(sys) = agent
                    .memory
                    .messages
                    .iter()
                    .find(|m| m["role"] == "system")
                    .cloned()
                {
                    loaded.messages.insert(0, sys);
                }
            }
            // adopt_history statt `agent.memory = …`: mit frischem --ctx muss
            // der Verlauf auch in den verwalteten Kontext, sonst begänne das
            // Modell bei null (siehe Agent::adopt_history).
            agent.adopt_history(loaded);
            eprintln!("» Session geladen: {path}");
        }
        Ok(_) => {}
        Err(e) => eprintln!("[WARN] --session nicht ladbar: {e}"),
    }
}

/// Speichert den Verlauf des Agenten in die `--session`-Datei (Warnung statt Abbruch).
fn save_session(agent: &Agent, path: &str) {
    if let Err(e) = agent.memory.save(path) {
        eprintln!("[WARN] --session nicht speicherbar: {e}");
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

/// `/undo` — die jüngste Datei-Änderung zurücknehmen.
///
/// `/undo` nimmt eine zurück, `/undo alle` alle. Betrifft nur Dateien: was ein
/// `run_shell` angerichtet hat, weiß agentkit nicht und behauptet es auch nicht.
fn handle_undo(rest: &[&str], ctx: &ReplCtx) {
    let pal = ctx.pal;
    let Some(coding) = ctx.coding else {
        println!(
            "{}Im Demo-Modus gibt es keine Datei-Werkzeuge — nichts zurückzunehmen.{}",
            pal.gray, pal.reset
        );
        return;
    };
    if coding.checkpoint_count() == 0 {
        println!(
            "{}Keine Datei-Änderung zum Zurücknehmen.{}",
            pal.gray, pal.reset
        );
        return;
    }
    // Ohne Argument die Liste zeigen, mit `alle` alles zurücknehmen, sonst eine.
    match rest.first().copied() {
        Some("alle") | Some("all") => {
            while let Some(meldung) = coding.undo_last() {
                println!("{}✓ {meldung}{}", pal.green, pal.reset);
            }
        }
        Some("liste") | Some("list") => {
            println!("{}Rücknehmbar (jüngste zuerst){}", pal.bold, pal.reset);
            for pfad in coding.checkpoint_paths() {
                println!("  {}{pfad}{}", pal.cyan, pal.reset);
            }
        }
        _ => {
            if let Some(meldung) = coding.undo_last() {
                println!("{}✓ {meldung}{}", pal.green, pal.reset);
            }
            let rest_n = coding.checkpoint_count();
            if rest_n > 0 {
                println!(
                    "{}Noch {rest_n} Änderung(en) rücknehmbar (/undo alle){}",
                    pal.gray, pal.reset
                );
            }
        }
    }
}

/// `/init` — legt ein Grundgerüst für die Projekt-Instruktionen an.
///
/// Nur ein Gerüst mit Fragen, kein generierter Inhalt: was ein Projekt
/// ausmacht, weiß der Mensch — eine erfundene Beschreibung wäre schlimmer als
/// eine leere. Eine vorhandene Datei wird NICHT überschrieben.
fn handle_init(workspace: &str, pal: Pal) {
    let pfad = std::path::Path::new(workspace).join(agentkit::PROJECT_INSTRUCTIONS);
    if pfad.exists() {
        println!(
            "{}{} gibt es schon — nichts geändert.{}",
            pal.yellow,
            pfad.display(),
            pal.reset
        );
        return;
    }
    let vorlage = "# Projekt-Instruktionen für Coding-Agenten\n\n\
         Diese Datei wird bei jedem Start in diesem Verzeichnis an den System-Prompt\n\
         angehängt. Halte sie kurz — sie kostet in jedem Zug Kontext.\n\n\
         ## Was ist das hier?\n\n\
         (Ein bis zwei Sätze: Zweck des Projekts, Sprache, Aufbau.)\n\n\
         ## Bauen und Testen\n\n\
         (Die Befehle, die wirklich laufen — z. B. `cargo test`, `npm test`.)\n\n\
         ## Konventionen\n\n\
         (Was der Agent beachten muss: Stil, Sprache der Kommentare, verbotene Pfade.)\n\n\
         ## Leitplanken\n\n\
         Ein Frontmatter GANZ OBEN in dieser Datei setzt harte Regeln für `run_shell`.\n\
         `deny` lehnt einen Befehl ab, ohne zu fragen — auch mit `-y`; `allow` spart die\n\
         Rückfrage. Getrennt wird am Komma, ein Muster darf mehrere Wörter haben.\n\
         Zum Aktivieren die vier Zeilen an den Anfang der Datei verschieben:\n\n\
         ```\n\
         ---\n\
         deny: git push, npm publish\n\
         allow: cargo, ls, git status\n\
         ---\n\
         ```\n";
    match std::fs::write(&pfad, vorlage) {
        Ok(()) => println!(
            "{}✓ {} angelegt — ausfüllen und neu starten.{}",
            pal.green,
            pfad.display(),
            pal.reset
        ),
        Err(e) => println!("{}Anlegen fehlgeschlagen: {e}{}", pal.red, pal.reset),
    }
}

// ------------------------------------------------------------- Freigabe-Regeln

/// Freigabe-Regeln für `run_shell` — die Policy hinter dem [`ApproveFn`].
///
/// Zwei Wege zu „ohne Rückfrage": sitzungsweit (Antwort „immer" oder `-y`) und
/// **explizit dauerhaft** über `~/.agentkit/config.json` (`allow`, Umweg über
/// `AGENTKIT_ALLOW` — siehe `agent_framework_rs/src/config.rs`). Die dauerhafte
/// Freigabe ist pro Programm begrenzt (kein Blanko-„alles erlauben") und wird
/// beim Start sichtbar gemeldet, damit sie niemand vergisst. Ein Blanko-Auto-
/// Modus bleibt `-y` und wird weiterhin nicht gespeichert.
///
/// Geregelt wird nach dem **ersten Wort** des Befehls (`cargo`, `git`, `ls`).
/// Feiner wäre trügerisch: `cargo test` und `cargo publish` unterscheiden sich
/// nicht an der Länge des Präfixes, sondern in dem, was sie tun — dafür ist die
/// Einzelfrage der ehrlichere Weg.
#[derive(Default)]
struct Permissions {
    /// Erste Wörter, die nicht mehr nachfragen (Sitzung + `config.json`-Allowlist).
    erlaubt: std::collections::BTreeSet<String>,
    /// `-y`: alles ohne Rückfrage.
    alles: bool,
}

impl Permissions {
    /// Das erste Wort eines Befehls — der Schlüssel der Regel (siehe
    /// [`agentkit::config::shell_programm`], dieselbe Zerlegung nutzt das TUI).
    fn programm(command: &str) -> &str {
        agentkit::config::shell_programm(command)
    }

    /// Baut die Regeln aus der Umgebung: `alles` wie bisher aus `-y`, dazu die
    /// dauerhafte Allowlist aus `AGENTKIT_ALLOW` (von `config.json` gesetzt).
    fn aus_umgebung(alles: bool) -> Permissions {
        Permissions {
            erlaubt: agentkit::config::allow_liste(),
            alles,
        }
    }

    /// Braucht dieser Befehl noch eine Rückfrage?
    fn fragt_nach(&self, command: &str) -> bool {
        !self.alles && !self.erlaubt.contains(Self::programm(command))
    }

    /// Merkt „für diese Sitzung immer erlauben" — gibt das gemerkte Wort zurück.
    fn erlaube_dauerhaft(&mut self, command: &str) -> String {
        let prog = Self::programm(command).to_string();
        self.erlaubt.insert(prog.clone());
        prog
    }
}

/// Merkt das Programm für diese Sitzung UND trägt es in `~/.agentkit/config.json` ein.
/// Gibt die fertige Meldung zurück statt sie zu drucken — den Strom wählt der Aufrufer
/// (`/permissions` schreibt auf stdout, die Rückfrage auf stderr).
fn dauerhaft_erlauben(command: &str, perms: &Mutex<Permissions>, pal: Pal) -> String {
    let gemerkt = perms.lock().unwrap().erlaube_dauerhaft(command);
    match agentkit::config::add_allow_entry(&gemerkt) {
        Ok((pfad, _neu)) => format!(
            "{}✓ »{gemerkt}« steht jetzt in {} — auch in künftigen Läufen ohne Rückfrage.{}",
            pal.green,
            pfad.display(),
            pal.reset
        ),
        // Die Sitzungsregel greift trotzdem — nur das Merken über den Lauf hinaus fehlt.
        Err(e) => format!(
            "{}[WARN] »{gemerkt}« konnte nicht dauerhaft eingetragen werden: {e}{}",
            pal.yellow, pal.reset
        ),
    }
}

/// `/permissions` — die Regeln dieser Sitzung zeigen bzw. zurücksetzen.
fn handle_permissions(rest: &[&str], perms: &Mutex<Permissions>, pal: Pal) {
    if matches!(rest.first(), Some(&"allow") | Some(&"erlauben")) {
        let Some(prog) = rest.get(1).copied() else {
            println!(
                "{}Nutzung: /permissions allow <programm>{}",
                pal.gray, pal.reset
            );
            return;
        };
        println!("{}", dauerhaft_erlauben(prog, perms, pal));
        return;
    }
    let mut p = perms.lock().unwrap();
    if matches!(rest.first(), Some(&"reset") | Some(&"zurücksetzen")) {
        p.erlaubt.clear();
        p.alles = false;
        println!(
            "{}✓ Freigabe-Regeln zurückgesetzt — es wird wieder jedes Mal gefragt.{}",
            pal.green, pal.reset
        );
        println!(
            "{}Einträge aus der config.json gelten beim nächsten Start wieder.{}",
            pal.gray, pal.reset
        );
        return;
    }
    let aus_config = agentkit::config::allow_liste();
    if p.alles {
        println!(
            "{}Alle Shell-Befehle laufen ohne Rückfrage (-y).{}",
            pal.yellow, pal.reset
        );
    } else if p.erlaubt.is_empty() {
        println!(
            "{}Jeder Shell-Befehl wird einzeln freigegeben.{}",
            pal.gray, pal.reset
        );
    } else {
        println!("{}Ohne Rückfrage in dieser Sitzung{}", pal.bold, pal.reset);
        for prog in &p.erlaubt {
            let herkunft = if aus_config.contains(prog) {
                " (config.json)"
            } else {
                ""
            };
            println!("  {}{prog}{herkunft}{}", pal.cyan, pal.reset);
        }
    }
    println!(
        "{}/permissions reset setzt die Regeln zurück, /permissions allow <programm> trägt eine dauerhafte Freigabe ein.{}",
        pal.gray, pal.reset
    );
}

// ---------------------------------------------------------- Benachrichtigung

/// Meldet sich, wenn ein langer Auftrag fertig ist oder eine Freigabe wartet —
/// damit man nebenher etwas anderes tun kann.
///
/// Zwei Wege, beide ohne zusätzliche Abhängigkeit:
/// die Terminal-Glocke (`\x07`, funktioniert überall) und die OSC-9-Sequenz,
/// die moderne Terminals (Windows Terminal, iTerm2, WezTerm, Kitty) in eine
/// Desktop-Benachrichtigung übersetzen. Terminals, die OSC 9 nicht kennen,
/// verschlucken die Sequenz stillschweigend.
///
/// Geht auf **stderr**: stdout gehört im Pipe-Modus der Antwort. Und nur, wenn
/// stderr ein Terminal ist — in einer Logdatei wären Steuerzeichen nur Müll.
fn notify(text: &str, an: bool) {
    if !an || !std::io::stderr().is_terminal() {
        return;
    }
    eprint!("\x07\x1b]9;{text}\x07");
    let _ = std::io::stderr().flush();
}

/// Ab wann ein Lauf als „lang" gilt und eine Meldung rechtfertigt. Darunter
/// steht der Mensch ohnehin davor, und ein Piepsen wäre nur lästig.
const NOTIFY_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

// ------------------------------------------------------- Markdown im Terminal

/// Zeilenweise Markdown-Auszeichnung mit ANSI-Codes.
///
/// Warum zeilenweise und nicht als Block: der REPL streamt die Antwort Token
/// für Token: ein Block ließe sich erst am Ende rendern, und der gestreamte
/// Rohtext stünde dann doppelt da. Der Puffer hier gibt eine Zeile frei,
/// sobald ihr `\n` kommt — Überschriften, Aufzählungen, `**fett**`,
/// `` `code` `` und Code-Fences greifen alle auf Zeilenebene. Tabellen bleiben
/// roh: ausrichten ließe sich nur der ganze Block.
struct MarkdownStream {
    pal: Pal,
    /// Angefangene, noch nicht abgeschlossene Zeile.
    rest: String,
    /// Innerhalb eines ```-Blocks? Dann wird nicht inline ausgezeichnet.
    im_fence: bool,
}

impl MarkdownStream {
    fn new(pal: Pal) -> Self {
        MarkdownStream {
            pal,
            rest: String::new(),
            im_fence: false,
        }
    }

    /// Nimmt ein Stück Stream und gibt zurück, was davon fertig ausgezeichnet
    /// ist (inklusive Zeilenumbrüche). Angefangene Zeilen bleiben im Puffer.
    fn push(&mut self, chunk: &str) -> String {
        self.rest.push_str(chunk);
        let mut out = String::new();
        while let Some(pos) = self.rest.find('\n') {
            let zeile: String = self.rest.drain(..=pos).collect();
            out.push_str(&self.style_line(zeile.trim_end_matches('\n')));
            out.push('\n');
        }
        out
    }

    /// Gibt den Rest ohne abschließendes `\n` frei (Ende der Antwort).
    fn flush(&mut self) -> String {
        if self.rest.is_empty() {
            return String::new();
        }
        let zeile = std::mem::take(&mut self.rest);
        self.style_line(&zeile)
    }

    fn style_line(&mut self, zeile: &str) -> String {
        let p = self.pal;
        let trimmed = zeile.trim_start();

        // Fence-Grenzen schalten den Modus um und werden selbst dezent gesetzt.
        if trimmed.starts_with("```") {
            self.im_fence = !self.im_fence;
            let tag = trimmed.trim_start_matches('`').trim();
            return if self.im_fence && !tag.is_empty() {
                format!("{}▏ {tag}{}", p.gray, p.reset)
            } else {
                format!("{}▏{}", p.gray, p.reset)
            };
        }
        if self.im_fence {
            return format!("{}▏ {zeile}{}", p.cyan, p.reset);
        }

        let einzug = &zeile[..zeile.len() - trimmed.len()];

        // Überschrift: Rauten weg, fett.
        if let Some(rest) = trimmed.strip_prefix('#') {
            let titel = rest.trim_start_matches('#').trim();
            return format!("{einzug}{}{}{}", p.bold, titel, p.reset);
        }
        // Aufzählung: Marker zu einem Punkt vereinheitlichen.
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                return format!("{einzug}{}•{} {}", p.cyan, p.reset, self.inline(rest));
            }
        }
        format!("{einzug}{}", self.inline(trimmed))
    }

    /// `**fett**` und `` `code` `` auszeichnen; alles andere bleibt stehen.
    fn inline(&self, s: &str) -> String {
        let p = self.pal;
        let mit_code = umschliessen(s, "`", p.cyan, p.reset);
        umschliessen(&mit_code, "**", p.bold, p.reset)
    }
}

/// Ersetzt paarweise `marker`-Vorkommen durch `an`…`aus`. Ein einzelnes,
/// unpaariges Vorkommen bleibt unangetastet — sonst würde ein Sternchen im
/// Fließtext den Rest der Zeile einfärben.
fn umschliessen(s: &str, marker: &str, an: &str, aus: &str) -> String {
    let teile: Vec<&str> = s.split(marker).collect();
    if teile.len() < 3 {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for (i, teil) in teile.iter().enumerate() {
        if i > 0 {
            // Ungerade Indizes sind der Inhalt zwischen einem Markerpaar.
            let innen = i % 2 == 1;
            let paar_vollstaendig = i + 1 < teile.len();
            if innen && paar_vollstaendig {
                out.push_str(an);
            } else if innen {
                out.push_str(marker); // unpaarig: wörtlich stehen lassen
            } else {
                out.push_str(aus);
            }
        }
        out.push_str(teil);
    }
    out
}

// ----------------------------------------------------------------- Rendering

/// Wie [`agentkit::one_line`], aber für den Tool-Trace: Umbrüche werden zu `↵`
/// (statt zu Leerzeichen) und die Kürzung nennt die Zeilenzahl. Kein dritter
/// Kürzungs-Helfer nötig — einer von beiden reicht immer.
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
    /// Zeichnet die gestreamte Antwort als Markdown aus (`None` = roh
    /// durchreichen). Aus, sobald die Ausgabe kein Terminal ist oder Farben
    /// abgeschaltet sind — in einer Pipe wären ANSI-Codes nur Ballast.
    md: Option<MarkdownStream>,
    /// Wartezeichen auf stderr, solange noch nichts kam. Aus bei `--each`:
    /// parallele Läufe würden sich um dieselbe Zeile streiten.
    spinner: bool,
    /// `--format stream-json`: jedes Ereignis als JSON-Zeile auf stdout
    /// (bzw. `-o`) statt als Terminal-Spur.
    stream_json: bool,
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
            // Angefangene Schlusszeile noch ausgeben, bevor der Umbruch kommt.
            if let Some(md) = self.md.as_mut() {
                let rest = md.flush();
                if !rest.is_empty() {
                    self.put_raw(&rest);
                }
            }
            self.put("");
            self.streaming = false;
        }
    }

    fn handle(&mut self, ev: &AgentEvent) {
        if self.stream_json {
            emit_event_json(ev);
            return;
        }
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
            // Geht die fertige Antwort ohnehin sauber nach stdout, wird sie hier
            // NICHT auch noch live auf stderr mitgeschrieben — sonst steht sie
            // zweimal im Terminal. Bei `--format json` sah das aus wie zwei
            // JSON-Dokumente hintereinander, also wie eine kaputte Ausgabe,
            // obwohl stdout allein immer gültig war. Der Tool-Trace (`--steps`)
            // bleibt davon unberührt.
            if self.to_stderr {
                return;
            }
            self.streaming = true;
            match self.md.as_mut() {
                Some(md) => {
                    let fertig = md.push(t);
                    if !fertig.is_empty() {
                        self.put_raw(&fertig);
                    }
                }
                None => self.put_raw(t),
            }
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
            // `Structured` ist Nutzlast für Konsumenten, die `kind` kennen
            // (Trace, Betrachter) — bewusst NICHTS fürs Terminal: wer sie
            // schickt, legt daneben schon die menschenlesbare Zeile auf den Bus
            // (so macht es agentkit-swarm), und eine zweite Zeile mit rohem JSON
            // wäre dieselbe Information ein zweites Mal.
            EventData::Structured { .. } => {}
            // Verbrauch fasst `run_task` zusammen und meldet ihn am Ende EINMAL.
            EventData::TokenUsage(_) => {}
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

/// Ein Ereignis als JSON-Zeile (`--format stream-json`). Dieselbe
/// Serialisierung wie im Trace (`agentkit::trace`), nur ungekürzt und mit
/// `text_delta` — wer live mitliest, will die Tokens.
fn emit_event_json(ev: &AgentEvent) {
    let data = serde_json::to_value(&ev.data).unwrap_or(serde_json::Value::Null);
    let line = serde_json::json!({"type": ev.etype, "source": ev.source, "data": data});
    if let Err(e) = out_line(&line.to_string()) {
        eprintln!("[WARN] Ereignis nicht schreibbar: {e}");
    }
}

// ------------------------------------------------------------------ Approval

/// Solange eine Freigabe aussteht, gehört die stderr-Zeile der Rückfrage.
/// Der Spinner im Hauptthread schreibt sonst mit `\r` die Auswahlzeile weg —
/// der Anwender tippt dann blind in ein „denkt nach …".
static FREIGABE_LAEUFT: AtomicBool = AtomicBool::new(false);
/// Wer auf die Statuszeile schreibt, hält diese Sperre. Der Spinner gibt sie
/// nach jedem Bild wieder her; die Rückfrage hält sie bis zur Antwort.
static STDERR_ZEILE: Mutex<()> = Mutex::new(());

/// approve-Callback für `run_shell`: fragt mit eingefärbtem Prompt nach.
fn confirm_shell(command: &str, pal: Pal, notify_on: bool, perms: &Mutex<Permissions>) -> bool {
    // Schon erlaubt (per `-y` oder „immer") -> gar nicht erst fragen.
    if !perms.lock().unwrap().fragt_nach(command) {
        return true;
    }
    // Eine wartende Freigabe blockiert den Agenten — wer nebenher etwas
    // anderes tut, soll das mitbekommen.
    notify("agentkit: Freigabe nötig", notify_on);
    let prog = Permissions::programm(command);

    // Ab hier gehört die Statuszeile der Rückfrage — der Spinner im Hauptthread
    // muss draußen bleiben, sonst überschreibt sein `\r` die Auswahlzeile.
    // `Drop` statt manuellem Rücksetzen vor jedem `return`: kein Pfad kann das
    // Zurücksetzen vergessen, auch der Fehlerpfad von `read_line` nicht.
    struct FreigabeGuard;
    impl Drop for FreigabeGuard {
        fn drop(&mut self) {
            FREIGABE_LAEUFT.store(false, Ordering::SeqCst);
        }
    }
    FREIGABE_LAEUFT.store(true, Ordering::SeqCst);
    let _guard = FreigabeGuard;
    // Wartet, bis ein laufender Spinner-Tick fertig ist, und blockiert alle
    // weiteren — der Guard bleibt über das `read_line` gehalten, denn solange
    // eine Antwort aussteht, darf nichts anderes in die Statuszeile schreiben.
    let _zeile = STDERR_ZEILE.lock().unwrap();
    // Spinner-Reste wegräumen, bevor die Frage kommt.
    eprint!("\r{}\r", " ".repeat(20));

    eprintln!(
        "\n{}⚠  Shell-Befehl ausführen?{}\n  {}{command}{}",
        pal.yellow, pal.reset, pal.bold, pal.reset
    );
    eprint!(
        "{}  [j]a / [N]ein / [i]mmer ({prog}) / [d]auerhaft › {}",
        pal.yellow, pal.reset
    );
    let _ = std::io::stderr().flush();
    let mut ans = String::new();
    if std::io::stdin().read_line(&mut ans).is_err() {
        return false;
    }
    match ans.trim().to_lowercase().as_str() {
        "j" | "ja" | "y" | "yes" => true,
        "i" | "immer" | "a" | "always" => {
            let gemerkt = perms.lock().unwrap().erlaube_dauerhaft(command);
            eprintln!(
                "{}  ✓ »{gemerkt}« läuft in dieser Sitzung ohne Rückfrage (/permissions){}",
                pal.gray, pal.reset
            );
            true
        }
        "d" | "dauerhaft" => {
            eprintln!("  {}", dauerhaft_erlauben(command, perms, pal));
            true
        }
        _ => false,
    }
}

// --------------------------------------------------------------------- Setup

/// Wählt den LLM und gibt `(llm, label)` zurück.
/// Bildet `--model NAME` auf die Umgebungsvariable ab, aus der `build_llm` das
/// Modell ohnehin liest — je nach Anbieter `AZURE_OPENAI_DEPLOYMENT`,
/// `OPENAI_MODEL` oder `ANTHROPIC_MODEL`. Kein zweites Modell-Konzept: genau so verfährt schon
/// `~/.agentkit/config.json`. Bei `auto` wird beides gesetzt, damit der Name
/// greift, egal welcher Anbieter gewinnt.
fn apply_model_override(args: &Args) {
    let Some(name) = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    match args.provider.as_str() {
        "azure" => std::env::set_var("AZURE_OPENAI_DEPLOYMENT", name),
        "openai" => std::env::set_var("OPENAI_MODEL", name),
        "anthropic" => std::env::set_var("ANTHROPIC_MODEL", name),
        _ => {
            std::env::set_var("AZURE_OPENAI_DEPLOYMENT", name);
            std::env::set_var("OPENAI_MODEL", name);
            std::env::set_var("ANTHROPIC_MODEL", name);
        }
    }
}

/// `schema`: ein Antwort-Schema, das der Anbieter selbst erzwingen soll
/// (`--schema`/`--check`, nur wenn [`agentkit::schema::native_compatible`]).
#[cfg_attr(not(feature = "openai"), allow(unused_variables))]
fn build_llm(
    provider: &str,
    force_demo: bool,
    schema: Option<&serde_json::Value>,
) -> (Arc<dyn Llm>, String) {
    if force_demo || provider == "demo" {
        return agentkit::demo::build_llm(true);
    }
    #[cfg(feature = "openai")]
    {
        if provider == "azure" {
            match agentkit::azure_from_env() {
                Ok(mut llm) => {
                    if let Some(s) = schema {
                        llm = llm.with_response_format(agentkit::schema::openai_response_format(s));
                    }
                    let dep =
                        std::env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "?".into());
                    return (Arc::new(llm), format!("azure:{dep}"));
                }
                Err(e) => eprintln!("azure_from_env: {e} — Demo-Fallback"),
            }
        }
        if provider == "openai" {
            match agentkit::openai_from_env() {
                Ok(mut llm) => {
                    if let Some(s) = schema {
                        llm = llm.with_response_format(agentkit::schema::openai_response_format(s));
                    }
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
        if provider == "anthropic" {
            match agentkit::anthropic_from_env() {
                Ok(mut llm) => {
                    if let Some(s) = schema {
                        llm = llm.with_output_schema(s.clone());
                    }
                    return (Arc::new(llm), agentkit::demo::anthropic_label());
                }
                Err(e) => eprintln!("anthropic_from_env: {e} — Demo-Fallback"),
            }
        }
    }
    // auto (oder Feature `openai` aus): Azure -> OpenAI -> Anthropic -> Demo.
    match schema {
        Some(s) => agentkit::demo::build_llm_with_schema(false, s),
        None => agentkit::demo::build_llm(false),
    }
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
    /// Anzeigename des Modells (`azure:…`, `openai:…`, `demo`) — fürs `/model`.
    model_label: String,
    /// Freigabe-Regeln dieser Sitzung — geteilt mit dem Approve-Callback.
    perms: Arc<Mutex<Permissions>>,
    /// Die Sandbox-Tools — halten die Checkpoints für `/undo`. `None` im
    /// Demo-Zweig: dort gibt es keine schreibenden Werkzeuge.
    coding: Option<CodingTools>,
}

/// Baut den MCP-Hub aus `.mcp.json` (explizit via `--mcp-config` oder per Discovery im
/// Workspace/CWD). `--no-mcp` -> leerer Hub (MCP ist sonst auch im Demo-Modus aktiv).
/// `connect_all` (REPL/TUI) verbindet auch deaktivierte Server vor, damit sie später ohne
/// Reconnect zuschaltbar sind; im One-shot (`false`) werden nur die aktiven verbunden.
/// Ergebnisse gehen nach stderr.
fn build_mcp_hub(args: &Args, connect_all: bool) -> Arc<McpHub> {
    // MCP ist unabhängig vom LLM — auch im Demo-Modus nutzbar; nur --no-mcp schaltet ab.
    // `--tools none`: auch keine MCP-Werkzeuge — und keine Server starten.
    if args.no_mcp || args.tools_none() {
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
    // Zuerst, nicht zwischen den Serverzeilen: eine übernommene Server-Identität
    // ist das Wichtigste an dieser Ausgabe (MCP-Server starten ohne Rückfrage).
    for w in &hub.shadow_warnings {
        eprintln!("[WARN] {w}");
    }
    eprintln!("» MCP: {} Server", hub.servers.len());
    for s in &hub.servers {
        match (&s.client, &s.error) {
            (Some(_), _) => eprintln!(
                "  ⏺ {} — {}{}",
                s.name(),
                agentkit::tool_count_label(s.active_tool_count(), s.tool_count()),
                if s.is_enabled() { ", aktiv" } else { " (aus)" }
            ),
            (None, Some(e)) => eprintln!("  ✖ {} — nicht verbunden: {e}", s.name()),
            (None, None) => {}
        }
        let unbekannt = s.unknown_tools();
        if !unbekannt.is_empty() {
            eprintln!(
                "  [WARN] MCP '{}': unbekannte Tools in 'tools'-Allowlist (Tippfehler?): {}",
                s.name(),
                unbekannt.join(", ")
            );
        }
    }
    Arc::new(hub)
}

/// Stellt den Agenten zusammen: voller Coding-Agent (echter LLM) oder schlanker
/// Demo-Agent. Der `hub` (MCP) wird hereingereicht, damit der One-shot ihn EINMAL baut
/// und über JSON-Retries hinweg wiederverwendet (kein Reconnect je Versuch).
/// `approve` ersetzt die Rückfrage auf stdin — für `mcp-serve` und `acp`, wo
/// stdin dem Protokoll gehört. `None` = die gewohnte Frage im Terminal.
fn build_agent(args: &Args, pal: Pal, hub: Arc<McpHub>, approve: Option<ApproveFn>) -> Built {
    if args.tools_none() {
        return build_bare_agent(args, pal);
    }
    let mut built = build_full_agent(args, pal, hub, approve);
    if let Some(auswahl) = args.tools.as_deref() {
        restrict_tools(&mut built, auswahl);
    }
    built
}

/// Gibt eine Statusmeldung beim Bauen nur beim ERSTEN Agenten des Prozesses
/// aus: `--each` und JSON-Wiederholungen bauen viele — dieselben Zeilen
/// hundertfach auf stderr wären nur Rauschen.
static AGENT_ANGEKUENDIGT: AtomicBool = AtomicBool::new(false);

fn announce_once() -> bool {
    !AGENT_ANGEKUENDIGT.swap(true, Ordering::SeqCst)
}

/// Das Schema, das der Anbieter selbst erzwingen soll — `None`, wenn es keins
/// gibt oder die Anbieter es nicht annehmen würden (dann prüft agentkit selbst).
fn native_schema(args: &Args) -> Option<serde_json::Value> {
    let schema = load_schema(args).ok().flatten()?;
    agentkit::schema::native_compatible(&schema).then_some(schema)
}

/// LLM bauen und das Modell (einmal) melden.
fn build_announced_llm(args: &Args, pal: Pal) -> (Arc<dyn Llm>, String) {
    apply_model_override(args);
    let (llm, label) = build_llm(&args.provider, args.demo, native_schema(args).as_ref());
    if announce_once() {
        eprintln!("{}» Modell: {label}{}", pal.gray, pal.reset);
    }
    (llm, label)
}

/// `--tools none`: ein reiner Modell-Aufruf — keine Werkzeuge, keine
/// Sandbox, keine Projekt-Instruktionen, keine Rückfrage. Nur der
/// Zusatz-System-Prompt (`--system`/Befehl) bleibt.
fn build_bare_agent(args: &Args, pal: Pal) -> Built {
    let (llm, label) = build_announced_llm(args, pal);
    let mut builder = Agent::builder(llm.clone())
        .tools(ToolRegistry::new())
        .strategy(Strategy::Plain)
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
    let mut mcp_base = ToolRegistry::new();
    attach_ctx(&mut agent, &mut mcp_base, args, llm, &label);
    Built {
        agent,
        plan: Plan::new(),
        skills: None,
        roles: Vec::new(),
        hub: Arc::new(McpHub::empty()),
        mcp_base,
        model_label: label,
        perms: Arc::new(Mutex::new(Permissions::aus_umgebung(args.yes))),
        coding: None,
    }
}

/// `--tools NAME,…`: nur diese Werkzeuge behalten. Die Liste versteht dieselbe
/// Schreibweise wie eine Rolle (`read_only`, Claude-Code-Namen wie `Read`).
/// Unbekannte Namen werden gemeldet, nicht still verschluckt.
fn restrict_tools(built: &mut Built, auswahl: &str) {
    let keep = agentkit::parse_tools_field(Some(auswahl)).unwrap_or_default();
    let vorhanden = built.agent.tools.names();
    let unbekannt: Vec<&String> = keep.iter().filter(|n| !vorhanden.contains(n)).collect();
    if !unbekannt.is_empty() && announce_tools_once() {
        eprintln!(
            "[WARN] --tools: unbekannt und ignoriert: {}",
            unbekannt
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let behalten = |n: &str| keep.iter().any(|k| k == n);
    built.agent.tools.retain(behalten);
    built.mcp_base.retain(behalten);
}

static TOOLS_GEWARNT: AtomicBool = AtomicBool::new(false);

fn announce_tools_once() -> bool {
    !TOOLS_GEWARNT.swap(true, Ordering::SeqCst)
}

/// Der volle Agent (Coding-Agent oder Demo) mit allen Werkzeugen.
fn build_full_agent(args: &Args, pal: Pal, hub: Arc<McpHub>, approve: Option<ApproveFn>) -> Built {
    let (llm, label) = build_announced_llm(args, pal);
    let ansagen = !MELDUNGEN_GEZEIGT.swap(true, Ordering::SeqCst);
    // Eine stehende Freigabe muss sichtbar sein — sonst merkt niemand, dass
    // `config.json` hier schon Programme ohne Rückfrage laufen lässt.
    let erlaubt = agentkit::config::allow_liste();
    if ansagen && !erlaubt.is_empty() {
        eprintln!(
            "{}» Ohne Rückfrage (config.json): {}{}",
            pal.gray,
            erlaubt.into_iter().collect::<Vec<_>>().join(", "),
            pal.reset
        );
    }
    // Sichtbar machen, WELCHE Datei den System-Prompt ergänzt und welche Regeln
    // daraus gelten — eine still wirkende Datei wäre ein Rätsel bei unerwartetem
    // Verhalten, und seit der Standardname `AGENTS.md` gilt, kann sie auch aus
    // einem fremden Repo stammen.
    if ansagen && args.project_instructions {
        if let Some(instr) = agentkit::load_project_instructions(&args.workspace) {
            for pfad in &instr.sources {
                let groesse = std::fs::metadata(pfad).map(|m| m.len()).unwrap_or(0);
                eprintln!(
                    "{}» Projekt-Instruktionen geladen: {} ({} Bytes){}",
                    pal.gray,
                    pfad.display(),
                    groesse,
                    pal.reset
                );
            }
            if !instr.guardrails.deny.is_empty() {
                eprintln!(
                    "{}»   gesperrt (auch mit -y): {}{}",
                    pal.yellow,
                    instr.guardrails.deny.join(", "),
                    pal.reset
                );
            }
            if !instr.guardrails.allow.is_empty() {
                eprintln!(
                    "{}»   ohne Rückfrage: {}{}",
                    pal.gray,
                    instr.guardrails.allow.join(", "),
                    pal.reset
                );
            }
        }
    }

    // Demo-Modus: schlanker, netzfreier Agent — MCP-Tools werden dennoch eingeklinkt.
    if label.starts_with("demo") {
        #[allow(unused_mut)]
        let mut tools = demo_tools();
        // Der Graph ist netzfrei und funktioniert ohne Coding-Sandbox — anders als
        // das `swarm`-Tool gibt es hier also keinen Grund, ihn wegzulassen. Damit
        // bleibt `--demo --graph` der Weg, die Verdrahtung ohne API-Key zu prüfen.
        #[cfg(feature = "graph")]
        if let Some(setup) = frontend_tools(args).graph {
            agentkit_graph::register_graph_tools(&mut tools, setup.store, setup.access);
        }
        let mut builder = Agent::builder(llm.clone())
            .tools(tools)
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
        let mut mcp_base = hub.apply(&mut agent);
        attach_ctx(&mut agent, &mut mcp_base, args, llm, &label);
        return Built {
            agent,
            plan: Plan::new(),
            skills: None,
            roles: Vec::new(),
            hub,
            mcp_base,
            model_label: label,
            // Im Demo-Zweig gibt es keine Shell — `/permissions` soll trotzdem
            // die Wahrheit sagen, statt `-y` zu unterschlagen.
            perms: Arc::new(Mutex::new(Permissions::aus_umgebung(args.yes))),
            coding: None,
        };
    }

    // Freigabe-Policy steckt im Callback: bei `--yes` immer erlauben, sonst nachfragen.
    let yes = args.yes;
    let notify_on = args.notify;
    // Die Regeln leben in EINEM geteilten Objekt: der Approve-Callback fragt
    // sie, `/permissions` zeigt und ändert sie.
    let perms = Arc::new(Mutex::new(Permissions::aus_umgebung(yes)));
    let perms_cb = perms.clone();
    let approve: ApproveFn = approve.unwrap_or_else(|| {
        Arc::new(move |cmd: &str| confirm_shell(cmd, pal, notify_on, &perms_cb))
    });

    // Frontend-eigene Fähigkeiten: das `swarm`-Tool aus agentkit-swarm und die
    // Graph-Tools aus agentkit-graph, plus die Prompt-Zusätze, die dem Modell
    // erklären, wann sie sich lohnen.
    let extras = frontend_tools(args);
    let werkzeug_doku = agentkit_app::tool_system(!args.no_swarm, graph_active(args));
    let cfg = CodingAgentConfig {
        workspace: &args.workspace,
        strategy: args.strategy,
        max_steps: args.max_steps,
        skills: args.skills.as_deref(),
        agents: args.agents.as_deref(),
        agents_only: args.agents_only,
        protect_paths: &args.protect_paths,
        allow_read: &args.allow_read,
        sub_rules: args.sub_rules.as_deref(),
        memory: args.memory.as_deref(),
        subagents: !args.no_subagents,
        system: args.system.as_deref(),
        tool_system: werkzeug_doku.as_deref(),
        verify: args.verify,
        shell_timeout: args.shell_timeout,
        dry_run: args.dry_run,
        extra_tools: extras.build(),
        // Mit `--ctx` bekommt JEDER Helfer (Sub-Agent, Schwarm-Mitglied) einen
        // eigenen, nicht persistenten Kontext. Ohne das hätte der Orchestrator
        // sein Kontext-Management und die Helfer nicht — die Arbeit, die den
        // Kontext wirklich aufbläht, machen aber sie. Enger als das Budget des
        // Haupt-Agenten: ein Helfer erledigt eine abgegrenzte Aufgabe.
        helper_ctx_budget: args
            .ctx
            .as_deref()
            .map(|_| (args.ctx_budget / 3).max(8_000)),
        project_instructions: args.project_instructions,
    };
    let (mut agent, plan, skills, roles, mut mcp_base, coding) =
        build_coding_agent(llm.clone(), &cfg, approve, hub.clone());
    attach_ctx(&mut agent, &mut mcp_base, args, llm, &label);
    Built {
        agent,
        plan,
        skills,
        roles,
        hub,
        mcp_base,
        model_label: label,
        perms,
        coding: Some(coding),
    }
}

/// Wie [`AGENT_ANGEKUENDIGT`], für die übrigen Startmeldungen des vollen Agenten.
static MELDUNGEN_GEZEIGT: AtomicBool = AtomicBool::new(false);

/// Baut das Frontend-Tool-Bündel: Schwarm-Tool und (mit `--graph DIR`) die
/// Graph-Tools.
///
/// Ein nicht öffenbarer Graph ist ein **harter** Fehler, kein stiller Rückfall auf
/// „ohne Graph": wer `--graph` setzt, will ihn — und ein kaputtes Journal
/// unbemerkt zu überschreiben wäre der schlechteste denkbare Ausgang.
fn frontend_tools(args: &Args) -> agentkit_app::FrontendTools {
    #[allow(unused_mut)]
    #[cfg_attr(not(feature = "graph"), allow(clippy::needless_update))]
    let mut extras = agentkit_app::FrontendTools {
        swarm: !args.no_swarm,
        ..Default::default()
    };
    #[cfg(feature = "graph")]
    if let Some(dir) = args.graph.as_deref() {
        match agentkit_app::open_graph(
            dir,
            &args.workspace,
            &graph_run_id(args),
            args.graph_readonly,
        ) {
            Ok(setup) => {
                let stats = setup.store.stats();
                eprintln!(
                    "» Graph: {dir} — {} Aussagen, {} Entities, Revision {}{}",
                    stats.claims,
                    stats.entities,
                    stats.revision,
                    if args.graph_readonly {
                        " (nur lesend)"
                    } else {
                        ""
                    }
                );
                extras.graph = Some(setup);
            }
            Err(e) => {
                eprintln!("[FEHLER] --graph: {e}");
                std::process::exit(ExitCode::GeneralError.code());
            }
        }
    }
    #[cfg(not(feature = "graph"))]
    if args.graph.is_some() {
        eprintln!(
            "[WARN] --graph ignoriert — Binary ohne Feature `graph` gebaut \
             (cargo build --features graph)."
        );
    }
    extras
}

/// Scope des vorläufigen Arbeitswissens. An `--session` gebunden, damit ein
/// wiederaufgenommener Lauf seinen Arbeitsstand wiederfindet; sonst pro Prozess.
#[cfg(feature = "graph")]
fn graph_run_id(args: &Args) -> String {
    // Reihenfolge: ausdrücklicher Wunsch > Sitzungsdatei > Notnagel.
    //
    // `--graph-scope` gibt es, weil der Notnagel als Default nicht trug: Er war
    // `pid-<id>`, und Prozess-IDs kollidieren. Im Benchmark-Lauf 2026-08-08
    // verteilten sich 81 Task-Läufe — jeder in einem frischen Container, jeder
    // mit kleiner, vorhersagbarer PID — auf ganze 13 Scopes: `poker`,
    // `book-store`, `bowling` und `dot-dsl` liefen alle als PID 73 und lasen
    // deshalb einander, `grade-school` lief als PID 72 und sah nichts davon.
    // Wer wessen Wissen erbt, war damit ausgelost. Wer Tasks bewusst
    // zusammenschalten will, sagt es jetzt: `--graph-scope lauf-7`.
    if let Some(scope) = args.graph_scope.as_deref().map(str::trim) {
        if !scope.is_empty() {
            return scope.to_string();
        }
    }
    if let Some(stem) = args
        .session
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
    {
        return stem.to_string_lossy().to_string();
    }
    // Ohne Angabe: EIGENER Scope, nicht der eines fremden Prozesses. Lieber
    // nichts erben als zufällig etwas.
    format!("run-{}-{}", std::process::id(), zeitstempel_suffix())
}

/// Millisekunden seit Epoch als Suffix — genug, um zwei Läufe auf demselben
/// Host auseinanderzuhalten, ohne eine Zufallszahlen-Abhängigkeit einzuführen.
fn zeitstempel_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// true ⇔ der Graph ist wirklich aktiv (Flag gesetzt UND Feature gebaut).
fn graph_active(args: &Args) -> bool {
    cfg!(feature = "graph") && args.graph.is_some()
}

/// Klinkt ctxman als Context-Manager ein (`--ctx DIR`, Feature `ctxman`) — die
/// Logik teilt sich das CLI mit dem TUI ([`agentkit::attach_managed_context`]);
/// hier passieren nur Config-Bau (Policy-Datei, Compaction-LLM) und stderr-Meldung.
/// `label` ist das Label des Agent-LLM (ehrliches `compaction.model`-Metadatum,
/// solange kein separates Compaction-Modell konfiguriert ist).
#[cfg(feature = "ctxman")]
fn attach_ctx(
    agent: &mut Agent,
    mcp_base: &mut ToolRegistry,
    args: &Args,
    llm: std::sync::Arc<dyn Llm>,
    label: &str,
) {
    let Some(dir) = args.ctx.as_deref() else {
        return;
    };
    let mut cfg = agentkit::ManagedContextConfig::new(dir);
    cfg.budget_tokens = args.ctx_budget;
    // Fakten-Promotion in die --memory-Datei lenken, damit `recall` sie später findet.
    if let Some(mem) = args.memory.as_deref() {
        cfg.facts_path = Some(std::path::PathBuf::from(mem));
    }
    // Policy-Overlay: eine kaputte Datei aktiviert ctxman NICHT halbherzig mit
    // Default-Policy — der Nutzer hat explizit eine andere verlangt.
    if let Some(path) = args.ctx_policy.as_deref() {
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string()))
        {
            Ok(overlay) => cfg.policy_overlay = Some(overlay),
            Err(e) => {
                eprintln!("[WARN] --ctx-policy {path}: {e} — ctxman NICHT aktiviert.");
                return;
            }
        }
    }
    // Separates Compaction-LLM; scheitert der Bau, übernimmt sichtbar das Agent-LLM.
    match args.ctx_compaction_model.as_deref() {
        Some(name) => match agentkit::compaction_llm_from_env(name) {
            Ok(cllm) => {
                cfg.compaction_llm = Some(cllm);
                cfg.compaction_model_label = Some(name.to_string());
            }
            Err(e) => eprintln!(
                "[WARN] --ctx-compaction-model {name}: {e} — Compaction läuft über das Agent-LLM."
            ),
        },
        None => cfg.compaction_model_label = Some(label.to_string()),
    }
    match agentkit::attach_managed_context(agent, mcp_base, cfg, llm) {
        Ok(info) => {
            eprintln!(
                "» ctxman: Kontext-Management aktiv ({dir}, Budget {}, Tokenizer {})",
                args.ctx_budget, info.tokenizer
            );
            if info.resumed {
                eprintln!(
                    "» ctxman: Session aus Snapshot fortgesetzt — die eingefrorene Policy gilt; \
                     --ctx-policy/--ctx-budget wirken erst auf eine neue Session."
                );
            }
        }
        Err(e) => eprintln!("[WARN] --ctx: {e}"),
    }
}

/// Ohne Feature `ctxman` ist `--ctx` ein sichtbarer No-op (Hinweis statt stillem Ignorieren).
#[cfg(not(feature = "ctxman"))]
fn attach_ctx(
    _agent: &mut Agent,
    _mcp_base: &mut ToolRegistry,
    args: &Args,
    _llm: std::sync::Arc<dyn Llm>,
    _label: &str,
) {
    if args.ctx.is_some() {
        eprintln!("[WARN] --ctx ignoriert — Binary ohne Feature `ctxman` gebaut (cargo build --features ctxman).");
    }
}

// ------------------------------------------------------------ One-shot / Pipe

/// Was One-shot und `--each` einmal vorab vorbereiten: das Antwort-Schema
/// und die `-f`-Dateien. Fehler hier sind Eingabefehler (Exit 3).
struct PipeSetup {
    /// `--schema FILE` bzw. das eingebaute Schema von `--check`.
    schema: Option<serde_json::Value>,
    /// `-f`-Dateien als `(Name, Inhalt)`.
    files: Vec<(String, String)>,
}

/// Das Antwort-Schema des Laufs: `--check` bringt sein eigenes mit.
fn load_schema(args: &Args) -> Result<Option<serde_json::Value>, String> {
    if args.check {
        return Ok(Some(agentkit::check_schema()));
    }
    let Some(path) = args.schema.as_deref() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("--schema {path}: {e}"))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("--schema {path}: kein gültiges JSON: {e}"))
}

fn prepare_pipe(args: &Args) -> Result<PipeSetup, String> {
    let schema = load_schema(args)?;
    if let Some(s) = &schema {
        if args.schema.is_some() && !agentkit::schema::native_compatible(s) {
            eprintln!(
                "[INFO] --schema: der Anbieter kann dieses Schema nicht selbst erzwingen \
                 (Objekte brauchen additionalProperties: false und alle Felder in required, \
                 keine Zahlen-/Längengrenzen) — agentkit prüft die Antwort und wiederholt."
            );
        }
    }
    let mut files = Vec::new();
    for f in &args.files {
        let content = std::fs::read_to_string(f).map_err(|e| format!("-f {f}: {e}"))?;
        files.push((f.clone(), content));
    }
    if args.cache.is_some() && !args.tools_none() {
        eprintln!(
            "[INFO] --cache mit Werkzeugen: der Cache kennt nur den Auftrag, nicht den \
             Stand des Workspaces. Für reine Filter --tools none verwenden."
        );
    }
    Ok(PipeSetup { schema, files })
}

/// Das Ergebnis EINES Auftrags — ob One-shot oder eine Zeile von `--each`.
/// Ausgegeben wird vom Aufrufer, weil One-shot, `--each` und `--patch` es
/// verschieden darstellen.
struct JobResult {
    code: ExitCode,
    /// Das bereinigte Resultat (Text bzw. kompaktes JSON); leer bei Fehlern.
    output: String,
    /// `--check`: `(ja?, Begründung)`.
    verdict: Option<(bool, String)>,
    usage: Usage,
    /// Aus dem `--cache` statt vom Modell.
    cached: bool,
    /// Die Antwort stand schon live im Terminal (kein zweites Drucken).
    streamed: bool,
}

impl JobResult {
    fn failed(code: ExitCode, usage: Usage) -> Self {
        JobResult {
            code,
            output: String::new(),
            verdict: None,
            usage,
            cached: false,
            streamed: false,
        }
    }

    /// Das Resultat als JSON-Wert: bei strukturierter Ausgabe das Objekt
    /// selbst, sonst der Text — `null` bei einem Fehlschlag.
    fn value(&self, structured: bool) -> serde_json::Value {
        if self.code != ExitCode::Success && self.verdict.is_none() {
            return serde_json::Value::Null;
        }
        if structured {
            serde_json::from_str(&self.output).unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::String(self.output.clone())
        }
    }
}

/// Wie ein Auftrag läuft: was One-shot und `--each` unterscheidet.
struct JobEnv<'a> {
    pipe: &'a PipeSetup,
    hub: &'a Arc<McpHub>,
    trace: Option<&'a TraceSink>,
    budget: &'a TokenBudget,
    /// Gemeinsamer Stop-Knopf aller Aufträge (`--each`).
    cancel: Option<&'a agentkit::Cancel>,
    /// Freigabe ohne Rückfrage (`-q`, `--each`); `None` = fragen.
    approve: Option<ApproveFn>,
    /// Spur, Spinner und Token-Zeile zeigen? Aus bei `--each`.
    interactive: bool,
}

/// One-shot mit Exit-Code-Vertrag und strikter Stream-Trennung.
fn run_oneshot(
    args: &Args,
    pal: Pal,
    stdin_ctx: Option<String>,
    trace: Option<&TraceSink>,
) -> ExitCode {
    let r = match oneshot_result(args, pal, stdin_ctx, trace) {
        Ok(r) => r,
        Err(code) => return code,
    };
    emit_single(args, &r)
}

/// Baut den Auftrag aus Prompt, stdin und `-f`-Dateien und führt ihn aus.
fn oneshot_result(
    args: &Args,
    pal: Pal,
    stdin_ctx: Option<String>,
    trace: Option<&TraceSink>,
) -> Result<JobResult, ExitCode> {
    let pipe = prepare_pipe(args).map_err(|e| {
        eprintln!("[ERROR] {e}");
        ExitCode::ContextError
    })?;
    let task = agentkit::attach_files(
        &build_task(args.prompt.trim(), stdin_ctx.as_deref()),
        &pipe.files,
    );
    // MCP-Hub EINMAL bauen (One-shot: nur aktive Server verbinden) und über alle
    // JSON-Retries hinweg wiederverwenden — kein Reconnect je Versuch.
    let hub = build_mcp_hub(args, false);
    let budget = TokenBudget::new(args.token_limit);
    let env = JobEnv {
        pipe: &pipe,
        hub: &hub,
        trace,
        budget: &budget,
        cancel: None,
        approve: args.quiet.then(|| policy_ohne_rueckfrage(args.yes)),
        interactive: true,
    };
    Ok(run_job(args, pal, &task, &env))
}

/// Gibt das Ergebnis eines einzelnen Auftrags aus und liefert den Exit-Code.
fn emit_single(args: &Args, r: &JobResult) -> ExitCode {
    if args.stream_json {
        let line = serde_json::json!({
            "type": "result",
            "exit": r.code.code(),
            "result": r.value(args.structured()),
            "usage": r.usage,
            "cached": r.cached,
        });
        return match out_line(&line.to_string()) {
            Ok(()) => r.code,
            Err(e) => {
                eprintln!("[ERROR] Schreiben fehlgeschlagen: {e}");
                ExitCode::GeneralError
            }
        };
    }
    // `--check`: stdout bleibt leer wie bei `grep -q`, das Urteil ist der
    // Exit-Code, die Begründung steht auf stderr.
    if let Some((ja, warum)) = &r.verdict {
        eprintln!("{} {warum}", if *ja { "✓ Ja:" } else { "✗ Nein:" });
        return r.code;
    }
    if r.code != ExitCode::Success || r.streamed {
        return r.code;
    }
    print_result_stdout(&r.output)
}

/// Führt EINEN Auftrag aus: Cache, Agent bauen, Lauf, Format prüfen (mit
/// Wiederholungen). Im JSON-/Schema-Modus wird die Antwort validiert und bei
/// Bedarf neu erzeugt — mit den Verstößen als Rückmeldung; gelingt das
/// nicht, ist der Exit-Code 4.
fn run_job(args: &Args, pal: Pal, task: &str, env: &JobEnv) -> JobResult {
    if TIMED_OUT.load(Ordering::SeqCst) {
        return JobResult::failed(ExitCode::Timeout, Usage::default());
    }
    if task.is_empty() {
        eprintln!("Keine Aufgabe übergeben.");
        return JobResult::failed(ExitCode::ContextError, Usage::default());
    }
    // Validierung: passt der (geschätzte) Kontext ins Fenster? -> sonst Exit 3.
    let tokens = count_tokens_text(task);
    if tokens > args.max_context {
        eprintln!(
            "[ERROR] Kontext zu groß: ~{tokens} Tokens > Limit {}. \
             (Anpassbar via --max-context.)",
            args.max_context
        );
        return JobResult::failed(ExitCode::ContextError, Usage::default());
    }

    let structured = args.structured();
    let schema = env.pipe.schema.as_ref();
    // Sobald die Ausgabe gepipt wird, im JSON- oder --print-Modus läuft: stdout
    // bleibt dem reinen Resultat vorbehalten, die Spur geht auf stderr.
    let clean_stdout = !env.interactive
        || structured
        || args.stream_json
        || args.print_mode
        || args.patch
        || args.output.is_some()
        || !std::io::stdout().is_terminal();
    let attempts = if structured {
        args.json_retries.max(1)
    } else {
        1
    };
    let mut last_final = String::new();
    let mut feedback: Option<String> = None;
    let mut total = Usage::default();
    let mut cache_key: Option<String> = None;

    for attempt in 1..=attempts {
        if attempt > 1 {
            eprintln!("[INFO] Antwort ungültig — neuer Versuch {attempt}/{attempts} …");
        }
        // Frischer Agent pro Versuch (sauberes Gedächtnis bei JSON-Retry).
        let built = build_agent(args, pal, env.hub.clone(), env.approve.clone());
        let mut agent = built.agent;

        // Cache: erst jetzt steht das Modell fest — es gehört zum Schlüssel.
        if attempt == 1 {
            if let Some(dir) = args.cache.as_deref() {
                let key = job_cache_key(args, &built.model_label, schema, task);
                if let Some(hit) = agentkit::cache_load(Path::new(dir), &key) {
                    if env.interactive {
                        eprintln!("{}» Cache-Treffer ({dir}){}", pal.gray, pal.reset);
                    }
                    return finish_job(args, hit, total, true, false);
                }
                cache_key = Some(key);
            }
        }
        // Resume: gespeicherten Verlauf laden (auch je JSON-Retry — derselbe Stand).
        if let Some(path) = args.session.as_deref() {
            load_session(&mut agent, path);
        }
        // Die Sperre selbst setzt `build_coding_agent` (für Haupt-Agent, Sub-Agenten
        // und Schwarm-Mitglieder gleichermaßen) — hier bleibt nur die Meldung.
        if args.dry_run && env.interactive {
            eprintln!("[INFO] Dry-Run aktiv — zerstörerische Schreibvorgänge werden blockiert.");
        }
        match schema {
            Some(s) => {
                inject_system(&mut agent, &agentkit::schema_system(s));
                if args.check {
                    inject_system(&mut agent, agentkit::CHECK_SYSTEM);
                }
            }
            None if structured => inject_system(&mut agent, JSON_SYSTEM),
            None => {}
        }

        let mut renderer = Renderer {
            show_steps: args.steps,
            quiet: args.print_mode || !env.interactive,
            streaming: false,
            pal,
            to_stderr: clean_stdout,
            // One-shot bleibt roh: der Unix-Filter-Kontrakt sagt zu, dass
            // stdout die unverfälschte Antwort trägt.
            md: None,
            spinner: env.interactive,
            stream_json: env.interactive && args.stream_json,
        };
        let prompt = match &feedback {
            Some(f) => format!("{task}\n\n{f}"),
            None => task.to_string(),
        };
        let (agent, final_, hard_error, usage) = run_task(
            agent,
            &prompt,
            &mut renderer,
            env.trace,
            args.run_strategy,
            env.budget,
            env.cancel,
        );
        total.add(&usage);
        // Verbrauch auf stderr — stdout bleibt dem Resultat. `-p` schweigt
        // auch hier, wie beim übrigen Trace.
        if usage.total() > 0 && !args.print_mode && env.interactive {
            eprintln!(
                "{}  ↳ Tokens {}{}",
                pal.gray,
                agentkit::fmt_usage(&usage),
                pal.reset
            );
        }
        // Verlauf sichern, BEVOR der Exit-Code fällt — auch ein Fehl-Lauf ist Verlauf.
        if let Some(path) = args.session.as_deref() {
            save_session(&agent, path);
        }
        if TIMED_OUT.load(Ordering::SeqCst) {
            return JobResult::failed(ExitCode::Timeout, total);
        }
        // Harte Fehler (Modell unerreichbar) / Sentinels -> direkter Exit-Code.
        if let Some(code) = classify_outcome(&final_, hard_error) {
            return JobResult::failed(code, total);
        }

        let output = if structured {
            // Gültiges JSON (und passend zum Schema) -> fertig; sonst nächster
            // Versuch, diesmal mit dem Grund.
            let Some(clean) = extract_json(&final_) else {
                feedback = Some(
                    "Deine letzte Antwort war kein gültiges JSON. Antworte NUR mit dem JSON."
                        .to_string(),
                );
                last_final = final_;
                continue;
            };
            if let Some(s) = schema {
                let value: serde_json::Value =
                    serde_json::from_str(&clean).unwrap_or(serde_json::Value::Null);
                if let Err(fehler) = agentkit::schema::validate(s, &value) {
                    eprintln!("[INFO] Antwort verletzt das Schema: {}", fehler.join("; "));
                    feedback = Some(format!(
                        "Deine letzte Antwort verletzte das JSON-Schema:\n- {}\n\
                         Antworte erneut, diesmal passend zum Schema.",
                        fehler.join("\n- ")
                    ));
                    last_final = final_;
                    continue;
                }
            }
            clean
        } else {
            final_.trim_end().to_string()
        };

        if let (Some(dir), Some(key)) = (args.cache.as_deref(), cache_key.as_deref()) {
            if let Err(e) = agentkit::cache_store(Path::new(dir), key, &output, &built.model_label)
            {
                eprintln!("[WARN] --cache nicht schreibbar ({dir}): {e}");
            }
        }
        // Text-Modus am Terminal: der Renderer hat die Antwort schon live gezeigt.
        return finish_job(args, output, total, false, !clean_stdout);
    }

    eprintln!(
        "[ERROR] Konnte trotz {attempts} Versuchen kein gültiges JSON erzeugen. \
         Letzte Antwort (gekürzt): {}",
        last_final.chars().take(200).collect::<String>()
    );
    JobResult::failed(ExitCode::FormatError, total)
}

/// Ein fertiges Resultat einordnen: bei `--check` entscheidet das Urteil
/// über den Exit-Code (Ja = 0, Nein = 1).
fn finish_job(
    args: &Args,
    output: String,
    usage: Usage,
    cached: bool,
    streamed: bool,
) -> JobResult {
    let verdict = if args.check {
        serde_json::from_str(&output)
            .ok()
            .and_then(|v| agentkit::check_verdict(&v))
    } else {
        None
    };
    let code = match &verdict {
        Some((false, _)) => ExitCode::GeneralError,
        _ => ExitCode::Success,
    };
    JobResult {
        code,
        output,
        verdict,
        usage,
        cached,
        streamed,
    }
}

/// Schlüssel für `--cache`: alles, was die Antwort bestimmt — Modell,
/// Zusatz-Prompt, Werkzeuge, Strategie, Format/Schema und der Auftrag selbst.
fn job_cache_key(
    args: &Args,
    model: &str,
    schema: Option<&serde_json::Value>,
    task: &str,
) -> String {
    let schema = schema.map(|s| s.to_string()).unwrap_or_default();
    let strategy = format!("{:?}", args.strategy);
    let format = format!("{:?}/{}", args.format, args.check);
    agentkit::cache_key(&[
        "agentkit-cache-v1",
        model,
        args.system.as_deref().unwrap_or(""),
        args.tools.as_deref().unwrap_or("*"),
        &strategy,
        &format,
        &schema,
        task,
    ])
}

/// Schreibt das finale Resultat (getrimmt, eine abschließende Zeile) auf stdout
/// bzw. in die `-o`-Datei.
fn print_result_stdout(text: &str) -> ExitCode {
    match out_line(text.trim_end()) {
        Ok(()) => ExitCode::Success,
        Err(e) => {
            eprintln!("[ERROR] Schreiben des Resultats fehlgeschlagen: {e}");
            ExitCode::GeneralError
        }
    }
}

/// Hängt eine System-Anweisung an die System-Nachricht des Agenten an (bzw.
/// legt eine an), damit auch Modelle ohne nativen JSON-Mode strukturiert
/// antworten.
fn inject_system(agent: &mut Agent, text: &str) {
    let msgs = &mut agent.memory.messages;
    if let Some(sys) = msgs.iter_mut().find(|m| m["role"] == "system") {
        if let Some(c) = sys["content"].as_str() {
            sys["content"] = serde_json::Value::String(format!("{c}\n\n{text}"));
            return;
        }
    }
    msgs.insert(0, serde_json::json!({"role": "system", "content": text}));
}

// ------------------------------------------------------------------- --each

/// `--each`: jede stdin-Zeile wird ein eigener Auftrag (`{}` im Prompt wird
/// durch die Zeile ersetzt, sonst hängt sie als Kontext an). Bis zu `-j N`
/// laufen gleichzeitig; die Ergebnisse gehen als JSONL in EINGABE-Reihenfolge
/// hinaus, jede Zeile sobald sie und alle davor fertig sind.
///
/// Exit-Code: 0, wenn jeder Auftrag gelang, sonst der Code des ersten
/// fehlgeschlagenen (in Eingabe-Reihenfolge). `--token-limit` und `--timeout`
/// gelten für den ganzen Lauf.
fn run_each(
    args: &Args,
    pal: Pal,
    stdin_ctx: Option<String>,
    trace: Option<&TraceSink>,
) -> ExitCode {
    let Some(input) = stdin_ctx else {
        eprintln!("[ERROR] --each braucht Datensätze auf stdin (eine Zeile je Auftrag).");
        return ExitCode::ContextError;
    };
    let records: Vec<&str> = input
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .collect();
    let pipe = match prepare_pipe(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[ERROR] {e}");
            return ExitCode::ContextError;
        }
    };
    let mut args = args.clone();
    if args.session.take().is_some() {
        eprintln!("[WARN] --session wirkt nicht mit --each — jeder Auftrag beginnt frisch.");
    }
    let args = &args;
    let hub = build_mcp_hub(args, false);
    let budget = TokenBudget::new(args.token_limit);
    // EIN Stop-Knopf für alle: Ctrl-C, `--timeout` und das Token-Limit
    // beenden den ganzen Lauf, nicht nur einen Auftrag.
    let cancel = new_cancel();
    *CURRENT_CANCEL.lock().unwrap() = Some(cancel.clone());
    let env = JobEnv {
        pipe: &pipe,
        hub: &hub,
        trace,
        budget: &budget,
        cancel: Some(&cancel),
        // stdin sind die Datensätze, und parallele Rückfragen wären Chaos:
        // es gilt die Freigabe-Policy ohne Rückfrage (-y bzw. allow-Liste).
        approve: Some(policy_ohne_rueckfrage(args.yes)),
        interactive: false,
    };
    let n = records.len();
    let next = AtomicUsize::new(0);
    let fertig = Mutex::new(EachOutput {
        next: 0,
        lines: vec![None; n],
    });
    let workers = args.jobs.max(1).min(n.max(1));
    let structured = args.structured();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n {
                    break;
                }
                let record = records[i];
                let r = if TIMED_OUT.load(Ordering::SeqCst) {
                    JobResult::failed(ExitCode::Timeout, Usage::default())
                } else if cancel.load(Ordering::SeqCst) || budget.exhausted() {
                    // Abgebrochen oder Budget erschöpft: nicht mehr anfangen.
                    JobResult::failed(ExitCode::GeneralError, Usage::default())
                } else {
                    let task = agentkit::attach_files(
                        &agentkit::expand_template(&args.prompt, record),
                        &pipe.files,
                    );
                    run_job(args, pal, &task, &env)
                };
                eprintln!(
                    "{}[{}/{n}] {} {}{}",
                    pal.gray,
                    i + 1,
                    if r.code == ExitCode::Success {
                        "✓"
                    } else {
                        "✖"
                    },
                    agentkit::one_line(record, 60),
                    pal.reset
                );
                let mut line = serde_json::json!({
                    "index": i + 1,
                    "input": record,
                    "exit": r.code.code(),
                    "result": r.value(structured),
                    "usage": r.usage,
                });
                if r.cached {
                    line["cached"] = serde_json::Value::Bool(true);
                }
                let mut f = fertig.lock().unwrap();
                f.lines[i] = Some((r.code, line.to_string()));
                // Alles ausgeben, was jetzt lückenlos fertig ist.
                while let Some(Some((_, text))) = f.lines.get(f.next) {
                    if let Err(e) = out_line(text) {
                        eprintln!("[ERROR] Schreiben fehlgeschlagen: {e}");
                    }
                    f.next += 1;
                }
            });
        }
    });
    *CURRENT_CANCEL.lock().unwrap() = None;

    let f = fertig.into_inner().unwrap();
    let total: u64 = budget.used.load(Ordering::SeqCst);
    if total > 0 {
        eprintln!(
            "{}» {n} Aufträge, {} Tokens{}",
            pal.gray,
            agentkit::fmt_tokens(total as usize),
            pal.reset
        );
    }
    f.lines
        .into_iter()
        .flatten()
        .map(|(code, _)| code)
        .find(|c| *c != ExitCode::Success)
        .unwrap_or(ExitCode::Success)
}

/// Die JSONL-Ausgabe von `--each`: fertige Zeilen je Eingabe-Index und die
/// nächste, die hinaus darf — so bleibt die Reihenfolge trotz Parallelität.
struct EachOutput {
    next: usize,
    lines: Vec<Option<(ExitCode, String)>>,
}

// ------------------------------------------------------------------ --patch

/// `--patch`: der Agent arbeitet auf einer Kopie des Workspaces; auf stdout
/// kommt der Unified Diff (für `git apply`), seine Antwort geht auf stderr.
/// Das Original bleibt unberührt.
fn run_patch(
    args: &Args,
    pal: Pal,
    stdin_ctx: Option<String>,
    trace: Option<&TraceSink>,
) -> ExitCode {
    let wt = match agentkit_app::patch::Worktree::create(Path::new(&args.workspace)) {
        Ok(wt) => wt,
        Err(e) => {
            eprintln!("[ERROR] --patch: Arbeitskopie nicht anlegbar: {e}");
            return ExitCode::GeneralError;
        }
    };
    eprintln!(
        "{}» --patch: Agent arbeitet auf einer Kopie ({}){}",
        pal.gray,
        wt.path().display(),
        pal.reset
    );
    let mut kopie = args.clone();
    kopie.workspace = wt.path().to_string_lossy().to_string();
    let r = match oneshot_result(&kopie, pal, stdin_ctx, trace) {
        Ok(r) => r,
        Err(code) => return code,
    };
    if r.code != ExitCode::Success {
        return r.code;
    }
    if !r.output.is_empty() {
        eprintln!("{}", r.output);
    }
    let (patch, hinweise) = match wt.diff() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[ERROR] --patch: Vergleich fehlgeschlagen: {e}");
            return ExitCode::GeneralError;
        }
    };
    for h in hinweise {
        eprintln!("[WARN] {h}");
    }
    if patch.is_empty() {
        eprintln!("{}» keine Änderungen{}", pal.gray, pal.reset);
        return ExitCode::Success;
    }
    print_result_stdout(&patch)
}

// ------------------------------------------------------------------ Ausführen

/// Treibt EINE Aufgabe auf einem Worker-Thread an und rendert die Events live. Gibt
/// `(Agent, finale Antwort, harter_Fehler)` zurück; `harter_Fehler` markiert einen
/// Modell-/Stream-Ausfall (ERROR-Event ohne Tool-Namen) für die Exit-Code-Abbildung.
///
/// Der Bus wandert per Move in den Worker — er darf NICHT im Aufrufer liegen
/// bleiben: die Subscriber-Sender hängen daran, und solange einer lebt, blockiert
/// `q.recv()` ewig. Panickt der Worker (z. B. ein Tool), fällt mit ihm der letzte
/// Sender, die Schleife endet, und der Lauf wird als Absturz gemeldet statt den
/// Prozess hängen zu lassen.
/// Wartezeichen auf stderr, solange der Agent noch nichts gemeldet hat.
///
/// Schreibt genau eine Zeile und räumt sie selbst wieder weg (`\r` + Leerzeichen),
/// damit nichts stehen bleibt, wenn der Trace loslegt. Auf stderr, weil der REPL
/// seine Ausgabe auf stdout schreibt — so kommen sich beide nicht ins Gehege.
/// Ohne Farbunterstützung (kein Terminal) bleibt der Spinner ganz aus.
struct Spinner {
    pal: Pal,
    frame: usize,
    sichtbar: bool,
}

impl Spinner {
    /// Wartezeit je Bild — auch die Auflösung, mit der auf Ereignisse gewartet wird.
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);
    const FRAMES: [&'static str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

    fn new(pal: Pal) -> Self {
        Spinner {
            pal,
            frame: 0,
            sichtbar: false,
        }
    }

    /// Aus, wenn stderr kein Terminal ist: in einer Datei oder Pipe wären die
    /// `\r`-Zeilen nur Müll.
    fn aktiv(&self) -> bool {
        std::io::stderr().is_terminal()
    }

    fn tick(&mut self) {
        if !self.aktiv() || FREIGABE_LAEUFT.load(Ordering::SeqCst) {
            return;
        }
        let _zeile = STDERR_ZEILE.lock().unwrap();
        // Zwischen der Prüfung oben und dem Lock kann `confirm_shell` die
        // Rückfrage begonnen haben — erneut prüfen, jetzt unter der Sperre.
        if FREIGABE_LAEUFT.load(Ordering::SeqCst) {
            return;
        }
        eprint!(
            "\r{}{} denkt nach …{}",
            self.pal.gray,
            Self::FRAMES[self.frame % Self::FRAMES.len()],
            self.pal.reset
        );
        let _ = std::io::stderr().flush();
        self.frame += 1;
        self.sichtbar = true;
    }

    fn clear(&mut self) {
        if !self.sichtbar {
            return;
        }
        if FREIGABE_LAEUFT.load(Ordering::SeqCst) {
            return;
        }
        let _zeile = STDERR_ZEILE.lock().unwrap();
        if FREIGABE_LAEUFT.load(Ordering::SeqCst) {
            return;
        }
        eprint!("\r{}\r", " ".repeat(20));
        let _ = std::io::stderr().flush();
        self.sichtbar = false;
    }
}

/// Zählt dieses Event als HARTER Lauf-Fehler (Modell/Netz unerreichbar)?
///
/// Zwei Einschränkungen, beide nötig:
///
/// - **Nur der Orchestrator** (leere `source`) — genau wie beim `DONE` im Loop
///   darunter. Ein Schwarm-Mitglied oder Sub-Agent taggt seine Events mit seiner
///   ID; ein transienter 429 dort darf den ganzen Aufruf nicht kippen. Sonst
///   liefert [`classify_outcome`] Exit 2, BEVOR das (gültige) Ergebnis des
///   Orchestrators auf stdout geht — beobachtet bei einem Schwarm-Lauf, der per
///   Konsens abschloss und dessen Ergebnis trotzdem verworfen wurde.
/// - **Nur `name: None`** — das ist agentkits Abgrenzung von Modell-/Streamfehlern
///   gegen Tool-Fehler. Tool-Fehler sind weich, das Modell korrigiert sich selbst.
fn ist_harter_fehler(ev: &AgentEvent) -> bool {
    ev.source.is_empty() && matches!(&ev.data, EventData::Error { name: None, .. })
}

/// Der Trace-Sink dieses Prozesses. Den Kontext-Datensatz schreibt der Agent
/// selbst über den Bus ([`agentkit::CONTEXT_SNAPSHOT`]) — hier bleibt nur der
/// Schreiber, an dem der Bus hängt.
struct TraceSink {
    writer: Arc<TraceWriter>,
}

/// Öffnet den Trace-Sink für `--trace DIR`. Ein Fehler ist kein Abbruchgrund:
/// der Trace beobachtet nur, der Auftrag läuft auch ohne ihn.
fn open_trace(dir: &str) -> Option<TraceSink> {
    match TraceWriter::create(Path::new(dir)) {
        Ok(w) => Some(TraceSink {
            writer: Arc::new(w),
        }),
        Err(e) => {
            eprintln!("[WARN] --trace nicht schreibbar ({dir}): {e}");
            None
        }
    }
}

/// Token-Budget eines Aufrufs (`--token-limit`). Geteilt, damit bei `--each`
/// alle parallelen Aufträge gegen DASSELBE Limit zählen.
struct TokenBudget {
    limit: Option<u64>,
    used: std::sync::atomic::AtomicU64,
}

impl TokenBudget {
    fn new(limit: Option<u64>) -> Self {
        TokenBudget {
            limit,
            used: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Bucht Verbrauch; `true` genau bei dem Call, der das Limit überschreitet
    /// — gemeldet wird einmal, nicht bei jedem weiteren.
    fn book(&self, n: u64) -> bool {
        let vorher = self.used.fetch_add(n, Ordering::SeqCst);
        self.limit.is_some_and(|l| vorher <= l && vorher + n > l)
    }

    fn exhausted(&self) -> bool {
        self.limit
            .is_some_and(|l| self.used.load(Ordering::SeqCst) > l)
    }
}

/// `cancel`: ein vom Aufrufer verwalteter Stop-Knopf (`--each` teilt EINEN
/// für alle Aufträge, damit Ctrl-C, `--timeout` und das Token-Limit alle
/// treffen). `None` = `run_task` legt einen eigenen an und hängt ihn an Ctrl-C.
fn run_task(
    agent: Agent,
    task: &str,
    renderer: &mut Renderer,
    trace: Option<&TraceSink>,
    strategy: RunStrategy,
    budget: &TokenBudget,
    cancel: Option<&agentkit::Cancel>,
) -> (Agent, String, bool, Usage) {
    // Der Mitschnitt hängt am BUS, nicht an dieser Schleife: so landen auch
    // Nachzügler eines Sub-Agenten im Trace, die nach dem Abschluss-DONE
    // kommen und die Anzeige hier nicht mehr sieht.
    let bus = match trace {
        Some(sink) => EventBus::with_trace(sink.writer.clone()),
        None => EventBus::new(),
    };
    let q = bus.subscribe();
    let eigener_knopf = cancel.is_none();
    let cancel = cancel.cloned().unwrap_or_else(new_cancel);
    if eigener_knopf {
        *CURRENT_CANCEL.lock().unwrap() = Some(cancel.clone());
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let task_owned = task.to_string();
    let cancel_worker = cancel.clone();
    let mut agent = agent;
    std::thread::spawn(move || {
        let final_ = run_with_strategy(
            &mut agent,
            &task_owned,
            &bus,
            -1,
            Some(&cancel_worker),
            &strategy,
        );
        let _ = tx.send((agent, final_));
    });

    // Nur das Root-DONE (leere `source`) beendet die Anzeige; Sub-Agent-DONEs nicht.
    //
    // `recv_timeout` statt `recv`, damit in der Wartezeit ein Spinner laufen
    // kann — bewusst OHNE zweiten Thread: der würde sich mit der Ausgabe des
    // Renderers um dieselbe Zeile streiten. Der Spinner läuft nur bis zum
    // ersten Ereignis; danach zeigt der Trace selbst den Fortschritt.
    let mut hard_error = false;
    // Summe über ALLE Agenten dieses Auftrags (Sub-Agenten, Schwarm) — bezahlt
    // wird jeder Call, also zählt auch jeder gegen `--token-limit`.
    let mut usage = Usage::default();
    let mut spinner = Spinner::new(renderer.pal);
    loop {
        let ev = match q.recv_timeout(Spinner::INTERVAL) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if renderer.spinner {
                    spinner.tick();
                }
                continue;
            }
            Err(_) => break,
        };
        spinner.clear();
        if ev.etype == DONE && ev.source.is_empty() {
            break;
        }
        if ist_harter_fehler(&ev) {
            hard_error = true;
        }
        if let EventData::TokenUsage(u) = &ev.data {
            usage.add(u);
            // Der Stop-Knopf, nicht ein eigener Abbruchweg: der Lauf endet
            // kooperativ wie bei Ctrl-C und liefert "(abgebrochen)" (Exit 1).
            // Gemeldet wird nur beim Überschreiten, nicht bei jedem weiteren
            // Call eines noch auslaufenden Sub-Agenten.
            if budget.book(u.total()) {
                spinner.clear();
                eprintln!(
                    "[WARN] Token-Limit {} überschritten ({} Tokens) — Lauf wird abgebrochen.",
                    agentkit::fmt_tokens(budget.limit.unwrap_or(0) as usize),
                    agentkit::fmt_tokens(budget.used.load(Ordering::SeqCst) as usize)
                );
            }
            if budget.exhausted() {
                cancel.store(true, Ordering::SeqCst);
            }
        }
        // Unter derselben Sperre wie der Spinner: der Tool-Call wird publiziert,
        // BEVOR das Tool läuft — die `⏺ run_shell(…)`-Zeile landet also genau in
        // dem Moment hier, in dem `confirm_shell` im Worker-Thread schon fragt.
        // Ohne die Sperre schriebe sie über die Auswahlzeile der Rückfrage.
        let _zeile = STDERR_ZEILE.lock().unwrap();
        renderer.handle(&ev);
    }
    spinner.clear();
    // Kein Ergebnis heißt: der Worker ist gestorben (die Panik-Meldung steht schon
    // auf stderr). Als abgebrochenen Lauf melden -> Exit 1, nicht als API-Fehler.
    let (agent, final_) = match rx.recv() {
        Ok(pair) => pair,
        Err(_) => {
            eprintln!("[ERROR] Der Agenten-Thread ist abgestürzt — Lauf abgebrochen.");
            (build_dummy(), "(abgebrochen)".to_string())
        }
    };
    if eigener_knopf {
        *CURRENT_CANCEL.lock().unwrap() = None;
        // Zähler zurücksetzen: ein einzelnes Ctrl-C während des Laufs soll nach
        // Lauf-Ende nicht als "erstes von zwei" weiterzählen und den nächsten
        // Ctrl-C am Prompt sofort beenden lassen.
        INT_COUNT.store(0, Ordering::SeqCst);
    }
    (agent, final_, hard_error, usage)
}

/// Notnagel, wenn der Worker-Thread gestorben ist: der echte Agent ist mit ihm
/// verloren, der REPL braucht aber einen, um weiterlaufen zu können.
fn build_dummy() -> Agent {
    Agent::builder(agentkit::demo::build_llm(true).0).build()
}

// -------------------------------------------------------------- Slash-Befehle

/// Das Eingabe-Zeichen des REPL. Eine Quelle für beide Schleifen: rustyline
/// misst die Cursorspalte am übergebenen Prompt, ein Auseinanderlaufen mit der
/// gepipten Variante würde den Cursor verschieben.
const PROMPT: &str = "› ";

/// Alles, was der REPL zum Abarbeiten einer Eingabe braucht und dabei NICHT
/// verändert. Gebündelt, weil sonst dieselben sieben Parameter durch vier
/// Ebenen gereicht würden (`agent` und `renderer` bleiben separat: `&mut`).
struct ReplCtx<'a> {
    plan: &'a Plan,
    skills: Option<&'a Skills>,
    roles: &'a [AgentRole],
    hub: &'a McpHub,
    mcp_base: &'a ToolRegistry,
    pal: Pal,
    session: Option<&'a str>,
    /// Für `/sessions`: die Sitzungen sind projektbezogen abgelegt.
    workspace: &'a str,
    /// Für `/model`: dasselbe Label, das beim Start auf stderr steht.
    model_label: &'a str,
    /// `--notify`: bei langen Läufen melden.
    notify: bool,
    /// Freigabe-Regeln dieser Sitzung (`/permissions`).
    perms: &'a Mutex<Permissions>,
    /// Für `/undo`: hält die Checkpoints der Datei-Änderungen.
    coding: Option<&'a CodingTools>,
    /// `--trace DIR`: der Ereignisstrom wird zusätzlich als NDJSON mitgeschrieben.
    trace: Option<&'a TraceSink>,
    /// `-s plan_execute`: Aufträge dieser REPL-Sitzung laufen über den
    /// Phasen-Treiber statt als einzelner Loop-Durchlauf.
    run_strategy: RunStrategy,
    /// `--token-limit`: gilt je Auftrag, nicht für die ganze Sitzung.
    token_limit: Option<u64>,
}

/// Verarbeitet EINE REPL-Eingabe (Slash-Befehl oder Auftrag). `false` = beenden.
fn repl_dispatch(user: &str, agent: &mut Agent, renderer: &mut Renderer, ctx: &ReplCtx) -> bool {
    let pal = ctx.pal;
    if user.starts_with('/') {
        if !handle_slash(user, agent, ctx) {
            println!("{}Tschüss.{}", pal.gray, pal.reset);
            return false;
        }
        return true;
    }
    // Agent kurz herausnehmen, auf dem Worker laufen lassen, zurückholen.
    let vorher = agentkit::context_report(agent).total;
    let start = std::time::Instant::now();
    let taken = std::mem::replace(agent, build_dummy());
    let (back, _final, _hard, usage) = run_task(
        taken,
        user,
        renderer,
        ctx.trace,
        ctx.run_strategy,
        &TokenBudget::new(ctx.token_limit),
        None,
    );
    *agent = back;
    // Bilanz des Zuges: nur GEMESSENE Werte — belegter Kontext und Dauer.
    // Bewusst keine Kostenschätzung: die bräuchte eine Preistabelle, die schon
    // beim nächsten Preisschritt falsch wäre.
    let nachher = agentkit::context_report(agent).total;
    let pal = ctx.pal;
    // Nur bei langen Läufen melden — bei einer Antwort in zwei Sekunden sitzt
    // der Mensch ohnehin davor.
    if start.elapsed() >= NOTIFY_AFTER {
        notify("agentkit: Auftrag fertig", ctx.notify);
    }
    let dauer = format!("{:.1}", start.elapsed().as_secs_f64()).replace('.', ",");
    // Der Verbrauch ist gemessen (Provider-`usage`), der Kontext geschätzt —
    // beides nebeneinander, weil gerade die Differenz zeigt, was der Cache spart.
    let verbrauch = if usage.total() > 0 {
        format!(" · Tokens {}", agentkit::fmt_usage(&usage))
    } else {
        String::new()
    };
    println!(
        "{}  ↳ Kontext {} Tokens (+{}){verbrauch} · {dauer} s{}",
        pal.gray,
        agentkit::fmt_tokens(nachher),
        agentkit::fmt_tokens(nachher.saturating_sub(vorher)),
        pal.reset
    );
    // Nach jedem Auftrag sichern — ein Absturz kostet höchstens den letzten Zug.
    if let Some(path) = ctx.session {
        save_session(agent, path);
    }
    true
}

fn repl(agent: &mut Agent, renderer: &mut Renderer, ctx: &ReplCtx, stdin_is_tty: bool) {
    // Interaktives Terminal -> Zeileneditor (History, Ctrl-A/E/W/U, Ctrl-R,
    // Mehrzeilen-Eingabe). Gepipter stdin -> schlichter Zeilen-Loop wie bisher:
    // der REPL bleibt scriptbar (liest Kommandos und Folge-Antworten bis EOF).
    if stdin_is_tty {
        repl_editor(agent, renderer, ctx);
    } else {
        repl_piped(agent, renderer, ctx);
    }
}

fn repl_piped(agent: &mut Agent, renderer: &mut Renderer, ctx: &ReplCtx) {
    use std::io::BufRead;
    let pal = ctx.pal;
    let stdin = std::io::stdin();
    loop {
        print!("\n{}{PROMPT}{}", pal.green, pal.reset);
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            println!("\n{}Tschüss.{}", pal.gray, pal.reset);
            return;
        }
        let user = line.trim();
        if user.is_empty() {
            continue;
        }
        if !repl_dispatch(user, agent, renderer, ctx) {
            return;
        }
    }
}

/// „Eingabe noch unvollständig?" — ein offener ```-Fence oder ein Zeilenende
/// mit `\` heißt: Enter fügt eine Zeile an, statt zu senden.
fn input_incomplete(input: &str) -> bool {
    input.matches("```").count() % 2 == 1 || input.ends_with('\\')
}

/// Wie viele Vorschläge höchstens — eine Bildschirmseite reicht, sonst
/// scrollt der Verlauf weg.
const MAX_CANDIDATES: usize = 50;

/// Vervollständigt an der Cursorposition: `/befehl` am Zeilenanfang aus
/// [`COMMANDS`], `@pfad` überall aus dem Workspace.
///
/// Gibt den Startindex des zu ersetzenden Stücks und die Kandidaten zurück
/// (rustylines `Completer`-Kontrakt). Reine Funktion — deshalb testbar, ohne
/// ein Terminal zu bauen.
fn complete_at(line: &str, pos: usize, workspace: &Path) -> (usize, Vec<String>) {
    let bis_cursor = &line[..pos.min(line.len())];

    // Slash-Befehl: nur am Anfang der Eingabe und solange kein Leerzeichen kam
    // (danach sind es Argumente, z. B. `/rewind 2`).
    if let Some(rest) = bis_cursor.strip_prefix('/') {
        if !rest.contains(char::is_whitespace) {
            let treffer = COMMANDS
                .iter()
                .map(|(c, _)| *c)
                .filter(|c| c.starts_with(bis_cursor))
                .map(|c| format!("{c} "))
                .collect();
            return (0, treffer);
        }
    }

    // `@pfad`: ab dem letzten `@` des aktuellen Wortes.
    if let Some(at) = bis_cursor.rfind('@') {
        let fragment = &bis_cursor[at + 1..];
        if !fragment.contains(char::is_whitespace) {
            return (at + 1, pfad_kandidaten(workspace, fragment));
        }
    }

    (pos, Vec::new())
}

/// Workspace-relative Pfade, die auf `fragment` passen. Verzeichnisse bekommen
/// ein `/`, damit man weitertabben kann; versteckte Einträge bleiben außen vor,
/// solange nicht ausdrücklich mit `.` gesucht wird.
fn pfad_kandidaten(workspace: &Path, fragment: &str) -> Vec<String> {
    let (rel_dir, prefix) = match fragment.rsplit_once('/') {
        Some((d, p)) => (d, p),
        None => ("", fragment),
    };
    let Ok(eintraege) = std::fs::read_dir(workspace.join(rel_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = eintraege
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let ist_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let voll = if rel_dir.is_empty() {
                name
            } else {
                format!("{rel_dir}/{name}")
            };
            Some(if ist_dir { format!("{voll}/") } else { voll })
        })
        .collect();
    out.sort();
    out.truncate(MAX_CANDIDATES);
    out
}

/// Der rustyline-Helfer: Mehrzeilen-Erkennung ([`input_incomplete`]) und
/// Tab-Vervollständigung ([`complete_at`]). Hints und Highlighting bleiben leer.
struct ReplHelper {
    /// Ausgangspunkt der `@pfad`-Vervollständigung.
    ///
    /// Bewusst **ohne** Sandbox-Prüfung: der Vorschlag ist reine Tipphilfe,
    /// und tippen kann der Mensch ohnehin jeden Pfad. `@../x` oder `@/etc/x`
    /// lassen sich also vervollständigen — lesen kann der Agent sie trotzdem
    /// nicht, die Grenze zieht `CodingTools::safe` beim Werkzeugaufruf.
    workspace: PathBuf,
}

impl rustyline::validate::Validator for ReplHelper {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        use rustyline::validate::ValidationResult;
        if input_incomplete(ctx.input()) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }
}

impl rustyline::completion::Completer for ReplHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(complete_at(line, pos, &self.workspace))
    }
}

impl rustyline::hint::Hinter for ReplHelper {
    type Hint = String;
}

impl rustyline::highlight::Highlighter for ReplHelper {}

impl rustyline::Helper for ReplHelper {}

fn repl_editor(agent: &mut Agent, renderer: &mut Renderer, ctx: &ReplCtx) {
    use rustyline::error::ReadlineError;
    let pal = ctx.pal;

    let mut rl: rustyline::Editor<ReplHelper, rustyline::history::FileHistory> =
        match rustyline::Editor::new() {
            Ok(rl) => rl,
            Err(e) => {
                // Kein Editor möglich (exotisches Terminal) -> schlichter Loop.
                eprintln!(
                    "{}Zeileneditor nicht verfügbar ({e}) — einfacher Modus.{}",
                    pal.gray, pal.reset
                );
                return repl_piped(agent, renderer, ctx);
            }
        };
    rl.set_helper(Some(ReplHelper {
        workspace: PathBuf::from(ctx.workspace),
    }));

    // Persistente History über Sessions hinweg (Pfeiltasten, Ctrl-R).
    let history = agentkit::config::history_path();
    if let Some(p) = &history {
        let _ = rl.load_history(p);
    }
    // Farbcodes im Prompt sind erlaubt: rustyline überspringt ANSI-Sequenzen
    // beim Messen der Cursorspalte.
    let prompt = format!("{}{PROMPT}{}", pal.green, pal.reset);

    loop {
        println!();
        match rl.readline(&prompt) {
            Ok(line) => {
                let raw = line.trim();
                if raw.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(raw);
                // Anhängen statt Neuschreiben: zwei parallele REPLs überschreiben
                // sich sonst gegenseitig die History.
                if let Some(p) = &history {
                    let _ = rl.append_history(p);
                }
                // `\`-Fortsetzungen: der Backslash war nur der Umbruch-Marker.
                let user = raw.replace("\\\n", "\n");
                if !repl_dispatch(&user, agent, renderer, ctx) {
                    return;
                }
            }
            // Ctrl-C am Prompt: Zeile verworfen, weiter (Beenden via Ctrl-D//exit).
            Err(ReadlineError::Interrupted) => {
                println!(
                    "{}(Eingabe verworfen — Ctrl-D oder /exit beendet){}",
                    pal.gray, pal.reset
                );
            }
            Err(ReadlineError::Eof) => {
                println!("{}Tschüss.{}", pal.gray, pal.reset);
                return;
            }
            Err(e) => {
                eprintln!("{}Eingabefehler: {e}{}", pal.red, pal.reset);
                return;
            }
        }
    }
}

fn handle_slash(cmd: &str, agent: &mut Agent, ctx: &ReplCtx) -> bool {
    let ReplCtx {
        plan,
        skills,
        roles,
        hub,
        mcp_base,
        pal,
        ..
    } = *ctx;
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
        "export" => handle_export(&rest, agent, pal),
        "compact" => handle_compact(&rest, agent, pal),
        "model" => handle_model(&rest, ctx),
        "permissions" | "perms" => handle_permissions(&rest, ctx.perms, pal),
        "init" => handle_init(ctx.workspace, pal),
        "undo" => handle_undo(&rest, ctx),
        "context" | "ctx" => handle_context(agent, &rest, pal),
        "rewind" | "fork" => handle_rewind(&head, &rest, agent, ctx),
        "sessions" => {
            let sitzungen = agentkit::list_sessions(ctx.workspace);
            if sitzungen.is_empty() {
                println!(
                    "{}(noch keine gespeicherten Sitzungen für dieses Projekt){}",
                    pal.gray, pal.reset
                );
            } else {
                print_sessions(&sitzungen, pal);
                println!(
                    "{}Fortsetzen: agentkit --continue (jüngste) oder --resume{}",
                    pal.gray, pal.reset
                );
            }
            if let Some(p) = ctx.session {
                println!("{}Aktuell: {p}{}", pal.gray, pal.reset);
            }
        }
        "mcp" => handle_mcp(&rest, agent, hub, mcp_base, pal),
        _ => println!(
            "{}Unbekannter Befehl: {cmd}{}  ({}/help{})",
            pal.red, pal.reset, pal.cyan, pal.reset
        ),
    }
    true
}

/// `/export` — den Gesprächsverlauf ausgeben oder schreiben.
///
/// `/export` (gekürzt ins Terminal) · `/export <datei>` (volles Markdown) ·
/// `/export <datei> --json` (die rohen Messages, wie `--session`).
fn handle_export(rest: &[&str], agent: &Agent, pal: Pal) {
    let as_json = rest.contains(&"--json");
    let path = rest.iter().find(|a| !a.starts_with("--"));

    let Some(path) = path else {
        if as_json {
            println!(
                "{}Für JSON braucht es eine Datei: /export <datei> --json{}",
                pal.yellow, pal.reset
            );
            return;
        }
        // Ohne Datei: gekürzte Ansicht, damit ein Coding-Verlauf mit großen
        // Tool-Ergebnissen nicht durchs Terminal rauscht.
        println!("{}", agent.memory.to_markdown(false));
        println!(
            "{}Gekürzte Ansicht — `/export <datei>` schreibt den vollen Verlauf.{}",
            pal.gray, pal.reset
        );
        return;
    };

    let result = if as_json {
        agent.memory.save(path)
    } else {
        std::fs::write(path, agent.memory.to_markdown(true)).map_err(|e| e.to_string())
    };
    match result {
        Ok(()) => println!(
            "{}✓ Verlauf geschrieben: {path}{} ({} Nachrichten)",
            pal.green,
            pal.reset,
            agent.memory.messages.len()
        ),
        Err(e) => println!("{}Export fehlgeschlagen: {e}{}", pal.red, pal.reset),
    }
}

/// `/context` — zeigt die Kontext-Belegung als Balken plus Abschnitts-Legende.
///
/// Dieselben Daten wie im TUI (`context_report`), nur als Text statt als
/// ratatui-Zeilen: ohne ctxman die Zeichen/4-Schätzung über die
/// `ShortTermMemory`, mit ctxman die echte Segment-Statistik.
fn handle_context(agent: &Agent, rest: &[&str], pal: Pal) {
    /// Breite des Belegungsbalkens in Zeichen.
    const BAR: usize = 40;

    // `/context alles` bzw. `/context <n>`: die Nachrichten statt der Belegung.
    let wahl = rest.first().copied().unwrap_or("");
    if !wahl.is_empty() {
        return handle_context_messages(agent, wahl, pal);
    }

    let r = agentkit::context_report(agent);
    let gefuellt = (BAR * r.total / r.budget.max(1)).min(BAR);
    let quelle = if r.managed {
        "Verwaltung: ctxman"
    } else {
        "Schätzung: Zeichen/4"
    };
    println!(
        "{}Kontext{}  {} von {} Tokens ({})  {}{}{}",
        pal.bold,
        pal.reset,
        agentkit::fmt_tokens(r.total),
        agentkit::fmt_tokens(r.budget),
        agentkit::fmt_pct(r.total, r.budget),
        pal.gray,
        quelle,
        pal.reset
    );
    println!(
        "  {}{}{}{}{}",
        pal.cyan,
        "█".repeat(gefuellt),
        pal.gray,
        "░".repeat(BAR - gefuellt),
        pal.reset
    );
    for seg in &r.segments {
        let note = seg
            .note
            .as_deref()
            .map(|n| format!(" — {n}"))
            .unwrap_or_default();
        println!(
            "  {:<22} {:>10} Tokens  {:>7}  {}{}{}",
            seg.label,
            agentkit::fmt_tokens(seg.tokens),
            agentkit::fmt_pct(seg.tokens, r.budget),
            pal.gray,
            format_args!("{}{note}", agentkit::fmt_count(seg.count)),
            pal.reset
        );
    }
    match r.budget.checked_sub(r.total) {
        Some(frei) => println!(
            "  {}{:<22} {:>10} Tokens{}",
            pal.gray,
            "frei",
            agentkit::fmt_tokens(frei),
            pal.reset
        ),
        None => println!(
            "  {}Budget um {} Tokens überschritten{}",
            pal.yellow,
            agentkit::fmt_tokens(r.total - r.budget),
            pal.reset
        ),
    }
    println!(
        "  {}/context alles zeigt die Nachrichten selbst{}",
        pal.gray, pal.reset
    );
}

/// `/context alles` und `/context <n>` — WAS im Kontext steht, nicht bloß wie
/// viel. Die Belegung allein reicht zum Debuggen nicht.
///
/// Nur der Haupt-Agent: der REPL hält die Sub-Agenten nicht (sie leben
/// innerhalb eines Tool-Aufrufs). Deren Kontext zeigt das TUI (`/context
/// <agent>`) und der Betrachter — beide bekommen ihn über den Bus bzw. den
/// Trace, wo der Agent ihn selbst ablegt.
fn handle_context_messages(agent: &Agent, wahl: &str, pal: Pal) {
    let messages = &agent.memory.messages;

    if let Ok(n) = wahl.parse::<usize>() {
        match messages.get(n) {
            Some(m) => println!(
                "{}Nachricht {n} von {}{}\n{}",
                pal.bold,
                messages.len(),
                pal.reset,
                serde_json::to_string_pretty(m).unwrap_or_default()
            ),
            None => println!(
                "{}Es gibt keine Nachricht {n} (0…{}).{}",
                pal.yellow,
                messages.len(),
                pal.reset
            ),
        }
        return;
    }

    println!(
        "{}Kontext{}  {} Nachrichten",
        pal.bold,
        pal.reset,
        messages.len()
    );
    for (i, m) in messages.iter().enumerate() {
        let rolle = m["role"].as_str().unwrap_or("?");
        let inhalt = m["content"].as_str().unwrap_or("");
        let text = match m["tool_calls"].as_array() {
            Some(calls) if !calls.is_empty() => {
                let namen: Vec<&str> = calls
                    .iter()
                    .map(|c| c["function"]["name"].as_str().unwrap_or("?"))
                    .collect();
                format!("→ {}  {inhalt}", namen.join(", "))
            }
            _ => inhalt.to_string(),
        };
        let flach: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { '⏎' } else { c })
            .take(100)
            .collect();
        println!(
            "  {:>3}  {}{:<10}{} {:>9} Tokens  {flach}",
            i,
            pal.cyan,
            rolle,
            pal.reset,
            agentkit::fmt_tokens(agentkit::count_tokens_text(&text)),
        );
    }
    println!(
        "  {}/context <n> zeigt eine Nachricht vollständig{}",
        pal.gray, pal.reset
    );
}

/// `/model` — zeigt das aktive Modell.
///
/// Bewusst nur Anzeige: das LLM steckt beim Bau des Agenten auch in den
/// Sub-Agenten (`task`) und im Schwarm-Werkzeug. Ein Umschalten zur Laufzeit
/// träfe nur den Haupt-Agenten, und die Sub-Agenten liefen still auf dem alten
/// Modell weiter — eine Halbwahrheit, die schlimmer wäre als der Neustart.
fn handle_model(rest: &[&str], ctx: &ReplCtx) {
    let pal = ctx.pal;
    println!("{}Modell:{} {}", pal.bold, pal.reset, ctx.model_label);
    if !rest.is_empty() {
        println!(
            "{}Umschalten geht nur beim Start — der Agent reicht das Modell an Sub-Agenten \
             und Schwarm weiter. Neu starten mit: agentkit --model {} --continue{}",
            pal.gray,
            rest.join(" "),
            pal.reset
        );
    }
}

/// `/compact` — den Kontext sofort verdichten, statt auf das Token-Budget
/// (bzw. mit `--ctx` die Watermark) zu warten. Ein Hinweis lenkt die
/// Zusammenfassung: `/compact behalte die API-Details`.
fn handle_compact(rest: &[&str], agent: &mut Agent, pal: Pal) {
    let hint = rest.join(" ");
    // ctxmans Compaction kennt keinen Hinweis-Eingang. Das gehört gesagt —
    // sonst tippt man ihn und er verschwindet wortlos.
    if !hint.is_empty() && agent.context_managed() {
        println!(
            "{}Hinweis ohne Wirkung: mit --ctx verdichtet ctxman, dessen Compaction \
             nimmt keinen Hinweis entgegen.{}",
            pal.yellow, pal.reset
        );
    }
    // Über context_report, nicht memory.tokens(): mit ctxman ist `memory` nur
    // der Spiegel und bliebe unverändert — die Anzeige meldete dann stur
    // „vorher == nachher".
    let vorher = agentkit::context_report(agent).total;
    println!("{}Kompaktiere …{}", pal.gray, pal.reset);
    if agent.compact_now(Some(hint.as_str())) {
        let nachher = agentkit::context_report(agent).total;
        println!(
            "{}✓ Kontext kompaktiert{} (~{vorher} → ~{nachher} Tokens)",
            pal.green, pal.reset
        );
    } else {
        println!(
            "{}Nichts zu kompaktieren — der Verlauf ist noch kurz.{}",
            pal.gray, pal.reset
        );
    }
}

/// `/rewind` und `/fork` — im Gesprächsverlauf zurückgehen.
///
/// Ohne Argument listen beide die Züge auf. `/rewind <n>` verwirft Zug `n` und
/// alles danach, man steht also wieder davor und kann ihn anders stellen.
/// `/fork <n> [datei]` macht dasselbe, sichert den bisherigen Verlauf aber
/// vorher als Session-Datei — der alte Ast bleibt so erhalten.
fn handle_rewind(head: &str, rest: &[&str], agent: &mut Agent, ctx: &ReplCtx) {
    let pal = ctx.pal;
    let fork = head == "fork";

    let Some(arg) = rest.first() else {
        let starts = agent.memory.turn_starts();
        if starts.is_empty() {
            println!("{}(noch keine Züge im Verlauf){}", pal.gray, pal.reset);
            return;
        }
        println!("{}Züge{}", pal.bold, pal.reset);
        for (n, idx) in starts.iter().enumerate() {
            let text =
                agentkit::one_line(agentkit::memory::content(&agent.memory.messages[*idx]), 70);
            println!("  {}{:>3}{}  {text}", pal.cyan, n + 1, pal.reset);
        }
        println!("{}/{head} <n> geht vor Zug n zurück{}", pal.gray, pal.reset);
        return;
    };

    let Ok(turn) = arg.parse::<usize>() else {
        println!("{}Nutzung: /{head} [<zug-nummer>]{}", pal.yellow, pal.reset);
        return;
    };

    // Erst prüfen, dann schreiben: der Ast wird nur gesichert, wenn der Schnitt
    // danach auch wirklich gelingt — sonst bliebe eine Datei zu einem Rewind
    // liegen, der nie stattgefunden hat.
    let starts = agent.memory.turn_starts().len();
    if turn == 0 || turn > starts {
        println!(
            "{}Zug {turn} gibt es nicht — /{head} listet die Züge auf.{}",
            pal.yellow, pal.reset
        );
        return;
    }
    if let Some(grund) = rewind_blockiert(agent) {
        println!("{}{grund}{}", pal.yellow, pal.reset);
        return;
    }

    if fork {
        let path = match rest.get(1) {
            Some(p) => p.to_string(),
            None => freier_ast_pfad(turn),
        };
        if let Err(e) = agent.memory.save(&path) {
            println!(
                "{}Sichern fehlgeschlagen, nichts geändert: {e}{}",
                pal.red, pal.reset
            );
            return;
        }
        println!(
            "{}✓ Bisheriger Ast gesichert: {path}{}",
            pal.green, pal.reset
        );
    }

    match agent.rewind_to_turn(turn) {
        RewindOutcome::Done(removed) => {
            println!(
                "{}✓ Zurück vor Zug {turn}{} ({removed} Nachrichten verworfen, {} übrig)",
                pal.green,
                pal.reset,
                agent.memory.messages.len()
            );
            // Die gekürzte Fassung sofort in die Session schreiben, sonst
            // stünde beim nächsten Start wieder der alte Verlauf da.
            if let Some(path) = ctx.session {
                save_session(agent, path);
            }
        }
        // Beides oben schon abgefangen; hier nur der Vollständigkeit halber.
        RewindOutcome::NoSuchTurn | RewindOutcome::ContextManaged => {
            println!("{}Rewind nicht ausgeführt.{}", pal.yellow, pal.reset)
        }
    }
}

/// Grund, warum ein Rewind gerade nicht geht — sonst `None`.
///
/// Mit `--ctx` rendert ctxman die Provider-Messages und `memory` ist nur ein
/// Spiegel: ein Schnitt im Spiegel nähme dem Modell nichts weg, würde aber eine
/// mitlaufende `--session`-Datei dauerhaft vom tatsächlichen Kontext abtrennen.
/// Deshalb wird abgelehnt statt halb ausgeführt — mit einem Weg, der wirklich
/// funktioniert.
fn rewind_blockiert(agent: &Agent) -> Option<String> {
    match agent.rewind_check() {
        RewindOutcome::ContextManaged => Some(
            "Mit --ctx verwaltet ctxman den Kontext; ein Rewind würde nur den Spiegel kürzen, \
             nicht das, was das Modell sieht. Stattdessen: `/export <datei> --json` sichert den \
             Verlauf, dann agentkit mit dieser Datei als --session und einem FRISCHEN \
             --ctx-Verzeichnis neu starten."
                .to_string(),
        ),
        _ => None,
    }
}

/// Ein noch freier Dateiname für den gesicherten Ast von `/fork`. Ohne die
/// Kollisionsprüfung überschriebe ein zweiter Fork am selben Zug den ersten —
/// bei einem Befehl, dessen ganzer Zweck das Bewahren des alten Astes ist.
fn freier_ast_pfad(turn: usize) -> String {
    let kandidat = format!("agentkit-ast-zug{turn}.json");
    if !std::path::Path::new(&kandidat).exists() {
        return kandidat;
    }
    for n in 2..100 {
        let kandidat = format!("agentkit-ast-zug{turn}-{n}.json");
        if !std::path::Path::new(&kandidat).exists() {
            return kandidat;
        }
    }
    kandidat
}

/// `/mcp` — MCP-Server auflisten bzw. für den Agenten ein-/ausschalten, sowie
/// (`tools`) die Tools eines Servers auflisten bzw. einzeln umschalten.
/// `/mcp` (Liste) · `/mcp on|off <name>` · `/mcp tools <name>` ·
/// `/mcp on|off <name> <tool>`.
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
                    None => agentkit::tool_count_label(s.active_tool_count(), s.tool_count()),
                };
                println!("  {}{}{} {} — {}", col, mark, pal.reset, s.name(), info);
            }
            println!(
                "{}  /mcp on <name>  ·  /mcp off <name>  ·  /mcp tools <name>  ·  \
                 /mcp on <name> <tool>  ·  /mcp off <name> <tool>{}",
                pal.gray, pal.reset
            );
        }
        ["tools", name] => match hub.find(name) {
            None => println!("{}✖ unbekannter MCP-Server '{name}'{}", pal.red, pal.reset),
            Some(s) if !s.is_connected() => println!(
                "{}({name} ist nicht verbunden — keine Tools verfügbar){}",
                pal.gray, pal.reset
            ),
            Some(s) => {
                println!("{}Tools von '{name}':{}", pal.bold, pal.reset);
                for t in s.tool_names() {
                    let (mark, col) = if s.is_tool_enabled(t) {
                        ("●", pal.green)
                    } else {
                        ("○", pal.gray)
                    };
                    println!("  {}{}{} {}", col, mark, pal.reset, t);
                }
            }
        },
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
        [action, name, tool]
            if matches!(
                action.to_lowercase().as_str(),
                "on" | "off" | "enable" | "disable"
            ) =>
        {
            let on = matches!(action.to_lowercase().as_str(), "on" | "enable");
            match hub.set_tool_enabled(name, tool, on) {
                Ok(()) => {
                    hub.rewire(agent, mcp_base);
                    let state = if on { "aktiv" } else { "aus" };
                    println!(
                        "{}✓ MCP-Tool '{name}/{tool}' {state}.{}",
                        pal.green, pal.reset
                    );
                }
                Err(e) => println!("{}✖ {e}{}", pal.red, pal.reset),
            }
        }
        _ => println!(
            "{}Nutzung: /mcp [on|off <name>] [tools <name>] [on|off <name> <tool>]{}",
            pal.yellow, pal.reset
        ),
    }
}

/// Die Slash-Befehle des REPL: Name + Wirkung. Eine Liste statt eines
/// format!-Strings mit Dutzenden Positionsargumenten — ein neuer Befehl ist
/// eine Zeile, kein Abzählen von `{}`-Platzhaltern.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "diese Hilfe"),
    ("/clear", "Bildschirm leeren"),
    (
        "/reset",
        "Unterhaltung vergessen (neues Kurzzeitgedächtnis)",
    ),
    ("/plan", "aktuellen Plan zeigen"),
    ("/tools", "registrierte Tools auflisten"),
    ("/skills", "verfügbare Skills auflisten"),
    (
        "/agents",
        "verfügbare Sub-Agent-Rollen (task-Tool) auflisten",
    ),
    (
        "/export",
        "Verlauf zeigen; /export <datei> [--json] schreibt ihn",
    ),
    (
        "/undo",
        "letzte Datei-Änderung zurücknehmen (/undo alle | liste)",
    ),
    ("/init", "Projekt-Instruktionen (AGENTS.md) anlegen"),
    ("/model", "das aktive Modell zeigen"),
    (
        "/permissions",
        "Freigabe-Regeln zeigen; reset setzt sie zurück, allow <programm> trägt eine dauerhafte Freigabe in config.json ein",
    ),
    ("/context", "Kontext-Belegung zeigen (auch /ctx)"),
    (
        "/context alles",
        "die Nachrichten selbst; /context <n> eine davon vollständig",
    ),
    (
        "/compact",
        "Kontext jetzt verdichten; /compact <hinweis> lenkt die Zusammenfassung",
    ),
    (
        "/sessions",
        "gespeicherte Sitzungen dieses Projekts auflisten",
    ),
    (
        "/rewind",
        "Züge auflisten; /rewind <n> geht vor Zug n zurück",
    ),
    (
        "/fork",
        "wie /rewind, sichert den bisherigen Ast vorher als Datei",
    ),
    (
        "/mcp",
        "MCP-Server auflisten/umschalten (/mcp on|off <name>); Tools eines Servers \
         auflisten (/mcp tools <name>) oder einzeln umschalten (/mcp on|off <name> <tool>)",
    ),
    ("/exit", "beenden (auch /quit, Ctrl-D)"),
];

fn help_text(p: Pal) -> String {
    let width = COMMANDS.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut out = format!("{}Befehle{}\n", p.bold, p.reset);
    for (cmd, what) in COMMANDS {
        out.push_str(&format!("  {}{cmd:<width$}{}  {what}\n", p.cyan, p.reset));
    }
    out.push_str(
        "\nSonst: einfach eine Aufgabe eintippen. Ctrl-C bricht die laufende Aufgabe ab.\n\
         Tab vervollständigt Befehle (/se\u{2192}/sessions) und Dateien nach @ (@src/m\u{2192}…).\n\
         Editor: \u{2191}/\u{2193} History, Ctrl-R Suche, Ctrl-A/E/W/U wie readline; mehrzeilig\n\
         mit `\\` am Zeilenende oder in einem offenen ```-Block (History-Datei:\n\
         `history` im Konfigurationsverzeichnis, siehe `agentkit config path`).",
    );
    out
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
            run_strategy: args.run_strategy,
            force_demo: args.demo,
            workspace: args.workspace.clone(),
            skills: args.skills.clone(),
            agents: args.agents.clone(),
            agents_only: args.agents_only,
            protect_paths: args.protect_paths.clone(),
            allow_read: args.allow_read.clone(),
            sub_rules: args.sub_rules.clone(),
            memory: args.memory.clone(),
            subagents: !args.no_subagents,
            max_steps: args.max_steps,
            ask_approval: !args.yes,
            mcp_config: args.mcp_config.clone(),
            mcp_enable: args.mcp_enable.clone(),
            no_mcp: args.no_mcp,
            system: args.system.clone(),
            tool_system: agentkit_app::tool_system(!args.no_swarm, graph_active(args)),
            ctx: args.ctx.clone(),
            ctx_budget: args.ctx_budget,
            ctx_policy: args.ctx_policy.clone(),
            ctx_compaction_model: args.ctx_compaction_model.clone(),
            extra_tools: frontend_tools(args).build(),
            session: args.session.clone(),
            project_instructions: args.project_instructions,
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

// --------------------------------------------------------------------- work

/// `agentkit work <unterkommando> …` — reicht nur Abhängigkeiten durch
/// (`WorkCliDeps`); die gesamte Logik liegt in `agentkit_work::cli::dispatch`
/// (Schritt 7 des Plans: dieses Crate bleibt ein dünnes Wiring-Crate). Ohne
/// Feature `work` fehlt die Runtime im Build — dieselbe Machart wie `--graph`
/// ohne Feature `graph` (Warnung statt Absturz dort, hier Exit statt Warnung,
/// weil `work` kein Zusatz-Flag, sondern das ganze Verb ist): eine deutsche
/// Meldung auf stderr, Exit 1. Der Release-Smoke-Test unterscheidet genau
/// danach, ob ein Build die Arbeits-Runtime enthält.
#[cfg(not(feature = "work"))]
fn run_work_cmd(_rest: &[String]) -> std::io::Result<()> {
    eprintln!(
        "[FEHLER] Dieses Build enthält die Arbeits-Runtime nicht (ohne Feature `work` \
         gebaut — cargo build --features work)."
    );
    std::process::exit(ExitCode::GeneralError.code());
}

#[cfg(feature = "work")]
fn run_work_cmd(rest: &[String]) -> std::io::Result<()> {
    // Derselbe Stop-Knopf wie der REPL-/One-shot-Pfad: `new_cancel()` anlegen,
    // in CURRENT_CANCEL ablegen (der Handler liest die Zelle erst beim
    // Signal, nicht beim Einrichten) und denselben Ctrl-C-Handler aktivieren.
    // Der Work-Runner prüft das Flag kooperativ zwischen Schritten und
    // schreibt dann einen Checkpoint, statt den Prozess hart zu beenden.
    install_ctrlc_handler();
    let cancel = new_cancel();
    *CURRENT_CANCEL.lock().unwrap() = Some(cancel.clone());

    // Die drei globalen Frontend-Flags (`--no-swarm`, `--graph DIR`,
    // `--graph-readonly`) kennt `agentkit_work::cli` nicht — sie würden dort
    // als unbekannte Option abgewiesen. Deshalb hier herausziehen, BEVOR der
    // Rest weitergereicht wird. Warum das im Binary passiert und nicht im
    // Work-Crate: die Abhängigkeitsrichtung ist einbahnig (`agentkit_work`
    // kennt `agentkit`, aber nicht `agentkit-graph` — CLAUDE.md), nur dieses
    // Crate kennt beide Bibliotheken und kann `FrontendTools` bauen.
    let flags = extract_frontend_flags(&normalize_args(rest));
    let work_argv = flags.rest;
    let (extra_tools, graph_gateway) = work_frontend_tools(
        flags.no_swarm,
        flags.graph_dir.as_deref(),
        flags.graph_readonly,
        flags.graph_scope.as_deref(),
        &work_argv,
    );
    // Ein Work-Lauf hat keinen `EventBus`, an dem der Trace sonst hängt — der
    // Runner reicht die Ereignisse als `WorkProgress::Agent` durch. Der
    // Schreiber wandert deshalb in die Deps (siehe `WorkCliDeps::trace`).
    let trace = flags.trace_dir.as_deref().and_then(open_trace);

    // Farben/Freigabe wie im übrigen CLI: `confirm_shell` fragt interaktiv
    // nach, `-y`/`--yes` in der Work-Argumentliste überschreibt das lokal in
    // `agentkit_work::cli::cmd_run` mit "immer erlauben" (siehe dort) — hier
    // wird nur der Rückfrage-Callback für den Fall OHNE `-y` gebaut.
    let color =
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() && enable_vt();
    let pal = if color { Pal::color() } else { Pal::plain() };
    let perms = Arc::new(Mutex::new(Permissions::aus_umgebung(false)));
    let approve: ApproveFn = Arc::new(move |cmd: &str| confirm_shell(cmd, pal, false, &perms));

    let llm_builder = |provider: &str, demo: bool| build_llm(provider, demo, None).0;
    let deps = agentkit_work::cli::WorkCliDeps {
        llm: &llm_builder,
        approve,
        extra_tools,
        cancel,
        graph: graph_gateway,
        build_executor: Some(work_build_executor(flags.no_swarm)),
        trace: trace.map(|sink| sink.writer),
        // Dieselbe Zusammensetzung wie bei einem normalen Lauf: der Prompt des
        // Aufrufers plus die Fragmente der aktiven Frontend-Tools. Ohne das
        // wüsste ein Item-Agent nichts von `swarm` und den Graph-Tools, obwohl
        // er sie in der Hand hält.
        system_extra: agentkit_app::system_with_extras(
            flags.system.as_deref(),
            !flags.no_swarm,
            flags.graph_dir.is_some(),
        ),
        agent_setup: work_agent_setup(flags.ctx.clone(), flags.ctx_budget),
        protect_paths: flags.protect_paths.clone(),
        // Kein `--allow-read` für `agentkit work` in dieser Aufgabe (YAGNI) —
        // das Feld muss nur mitziehen, damit `WorkCliDeps` kompiliert.
        allow_read: Vec::new(),
    };
    let code = agentkit_work::cli::dispatch(&work_argv, deps);
    std::process::exit(code.code());
}

/// Baut die Naht, über die `agentkit_work::cli::cmd_run` seinen
/// Einzelagenten-Executor an `agentkit_app::DispatchingExecutor` reicht
/// (Phase 6, §13 des `agentkit-work`-Konzepts): `agentkit_work` kennt
/// `agentkit_swarm`/`ExecutorKind` zwar schon als eigenes Feld am `WorkItem`,
/// aber welcher Executor daraus tatsächlich gebaut wird, ist eine
/// Frontend-Entscheidung — deshalb bekommt `WorkCliDeps` nur diese eine
/// Closure statt einer neuen Dependency. `no_swarm` (`--no-swarm`) ist der
/// einzige Grund, warum in diesem Binary kein Schwarm verfügbar wäre —
/// `agentkit-swarm` selbst ist eine Pflicht-Dependency von `agentkit_app`.
#[cfg(feature = "work")]
/// Kontext-Management für die Item-Agenten eines Work-Laufs (`--ctx DIR`).
///
/// Jeder Versuch bekommt ein EIGENES Unterverzeichnis `<DIR>/<item>-<versuch>`.
/// Das ist keine Ordnungsliebe: ctxman legt seinen Zustand je Verzeichnis ab
/// und setzt beim Öffnen einen vorhandenen Kontext fort. Ein gemeinsames
/// Verzeichnis hieße, dass Versuch 2 den Gesprächsverlauf von Versuch 1
/// weiterführt — genau das, was die Work-Runtime bewusst nicht tut.
///
/// `None` ohne `--ctx` (und ohne Feature `ctxman`): dann läuft ein Work-Lauf
/// exakt wie zuvor.
#[cfg(feature = "work")]
fn work_agent_setup(
    ctx: Option<String>,
    budget: Option<u32>,
) -> Option<agentkit_work::executor::AgentSetup> {
    #[cfg(feature = "ctxman")]
    {
        let dir = ctx?;
        // Derselbe Default wie `ManagedContextConfig::default`.
        let budget = budget.unwrap_or(100_000);
        Some(Arc::new(
            move |agent: &mut Agent,
                  mcp_base: &mut ToolRegistry,
                  schluessel: &str,
                  llm: Arc<dyn Llm>| {
                let pfad = std::path::Path::new(&dir).join(schluessel);
                let mut cfg = agentkit::ManagedContextConfig::new(pfad);
                cfg.budget_tokens = budget;
                if let Err(e) = agentkit::attach_managed_context(agent, mcp_base, cfg, llm) {
                    // Kein Abbruch: ein Work-Lauf ohne Kontext-Management ist
                    // brauchbar, ein abgebrochener Versuch nicht.
                    eprintln!("[WARN] --ctx für {schluessel}: {e} — Versuch läuft ohne ctxman.");
                }
            },
        ))
    }
    #[cfg(not(feature = "ctxman"))]
    {
        if ctx.is_some() {
            eprintln!("[WARN] --ctx ignoriert — ohne Feature `ctxman` gebaut.");
        }
        let _ = budget;
        None
    }
}

#[cfg(feature = "work")]
fn work_build_executor(no_swarm: bool) -> agentkit_work::cli::ExecutorBuilder {
    Box::new(move |single: agentkit_work::CodingAgentExecutor| {
        let swarm = if no_swarm {
            None
        } else {
            Some(agentkit_app::SwarmWorkExecutor {
                llm: single.llm.clone(),
                approve: single.approve.clone(),
                cancel: single.cancel.clone(),
                dry_run: single.dry_run,
                shell_timeout: single.shell_timeout,
                // Dieselbe Entscheidung wie beim Einzelagenten — sonst haette
                // ein Schwarm-Item die Leitplanken des Projekts und ein
                // normales Item nicht (oder umgekehrt).
                project_instructions: single.project_instructions,
            })
        };
        Box::new(agentkit_app::DispatchingExecutor { single, swarm })
            as Box<dyn agentkit_work::AgentExecutor>
    })
}

// ---------------------------------------------------------------------- viz

/// Hilfetext des `viz`-Verbs — auch ohne Feature verfügbar, damit
/// `agentkit viz --help` überall dasselbe erklärt.
const VIZ_HELP: &str = "\
agentkit viz — den Ereignisstrom eines Laufs im Browser ansehen (localhost)

AUFRUF:
  agentkit viz [OPTIONEN]

OPTIONEN:
  -w, --workspace DIR   Arbeitsverzeichnis (Default: .); bestimmt die Defaults
                        von --trace und --work
  --trace DIR           Wurzel der Trace-Dateien (Default:
                        <workspace>/.agentkit/trace). Gesucht wird REKURSIV:
                        jede `trace-*.jsonl` darunter ist eine Sitzung, und ihr
                        Pfad relativ zu DIR ist ihr Name. Damit zeigt ein
                        Betrachter über einer Benchmark-Ergebniswurzel jeden
                        Task als eigene Sitzung
  --trace-file FILE     eine BESTIMMTE Trace-Datei statt der jüngsten
  --work DIR            Wurzel der Work-Projekte für den Work-Reiter
                        (Default: <workspace>/.agentkit/work). Wie --graph
                        meist überflüssig: liegt neben dem Trace-Verzeichnis
                        der Sitzung ein `work`, gewinnt das
  --graph DIR           Verzeichnis des Wissensgraphen für den Graph-Reiter.
                        Meist überflüssig: liegt neben dem Trace-Verzeichnis
                        der Sitzung ein `graph`, nimmt der Betrachter das von
                        selbst (`.agentkit/trace` → `.agentkit/graph`). Diese
                        Angabe greift nur, wenn es dort keinen gibt
  --port N              Port (Default: 7878; 0 = freien Port wählen lassen)
  --open                die Adresse gleich im Browser öffnen
  -h, --help            diese Hilfe

Den Trace erzeugt der Lauf selbst: `agentkit --trace .agentkit/trace \"…\"`.

ACHTUNG: Der Betrachter liefert den Trace unredigiert aus — Dateiinhalte,
Shell-Ausgaben, Modellantworten, also möglicherweise Geheimnisse. Er bindet
deshalb ausschließlich an 127.0.0.1, und jede Anfrage braucht das Token aus
der beim Start ausgegebenen URL. Er ist rein lesend.";

/// `agentkit viz …` ohne Feature `viz` — dieselbe Machart wie `work`: eine
/// deutsche Meldung auf stderr und Exit 1, damit der Release-Smoke-Test
/// unterscheiden kann, ob ein Build den Betrachter enthält.
#[cfg(not(feature = "viz"))]
fn run_viz_cmd(rest: &[String]) -> std::io::Result<()> {
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        println!("{VIZ_HELP}");
        return Ok(());
    }
    eprintln!(
        "[FEHLER] Dieses Build enthält den Betrachter nicht (ohne Feature `viz` \
         gebaut — cargo build --features viz)."
    );
    std::process::exit(ExitCode::GeneralError.code());
}

#[cfg(feature = "viz")]
fn run_viz_cmd(rest: &[String]) -> std::io::Result<()> {
    use agentkit_viz::{default_trace_dir, default_work_root, VizConfig, VizServer};

    let argv = normalize_args(rest);
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{VIZ_HELP}");
        return Ok(());
    }
    let wert = |flag: &str| find_flag_value(&argv, flag);
    let workspace = wert("-w")
        .or_else(|| wert("--workspace"))
        .unwrap_or_else(|| ".".to_string());
    let port: u16 = wert("--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_VIZ_PORT);

    let trace_dir = wert("--trace")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_trace_dir(&workspace));
    let cfg = VizConfig {
        // Der Default greift nur, wenn auch der Trace der voreingestellte ist.
        // Zeigt `--trace` auf einen fremden Baum (Benchmark-Ergebnisse), bekäme
        // eine Sitzung sonst die Work-Projekte des Startverzeichnisses
        // untergeschoben — siehe `work_default_passt`.
        work_root: match wert("--work") {
            Some(p) => Some(PathBuf::from(p)),
            None => agentkit_viz::server::work_default_passt(&trace_dir, &workspace)
                .then(|| default_work_root(&workspace)),
        },
        trace_dir,
        trace_file: wert("--trace-file").map(PathBuf::from),
        // KEIN Default: anders als Trace und Work hat der Graph keinen im
        // Agenten festgelegten Ort — `agentkit --graph DIR` nimmt jedes
        // Verzeichnis. Einen zu erfinden hieße, bei falschem Ort einen LEEREN
        // Graphen zu zeigen statt zu sagen, dass keiner gewählt wurde.
        graph_dir: wert("--graph").map(PathBuf::from),
        port,
    };
    let zeige = |p: &Option<PathBuf>| {
        p.as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };
    eprintln!(
        "» Trace-Verzeichnis: {}\n» Work-Verzeichnis:  {}\n» Graph-Verzeichnis: {}",
        cfg.trace_dir.display(),
        zeige(&cfg.work_root),
        zeige(&cfg.graph_dir)
    );

    let server = match VizServer::bind(cfg) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("[FEHLER] {e}");
            std::process::exit(ExitCode::GeneralError.code());
        }
    };
    let url = server.url();
    println!("{url}");
    eprintln!(
        "» agentkit viz läuft. Die Adresse enthält ein Token — der Betrachter zeigt \
         Datei- und Shell-Inhalte des Laufs unredigiert. Beenden mit Ctrl-C."
    );
    if argv.iter().any(|a| a == "--open") {
        agentkit_viz::open_browser(&url);
    }
    server.run();
    Ok(())
}

/// Default-Port des Betrachters. Bewusst fest und nicht „irgendein freier":
/// die Adresse soll zwischen zwei Starts dieselbe bleiben, damit ein offener
/// Browser-Tab nach einem Neustart weiterfunktioniert (nur das Token wechselt).
#[cfg(feature = "viz")]
const DEFAULT_VIZ_PORT: u16 = 7878;

// ---------------------------------------------------------------- --upgrade

/// `agentkit --upgrade` ohne Feature `upgrade` — dieselbe Machart wie `viz`/
/// `work` ohne ihr Feature: deutsche Meldung auf stderr, Exit 1.
#[cfg(not(feature = "upgrade"))]
fn run_upgrade_cmd(_gewuenscht: Option<&str>) -> std::io::Result<()> {
    eprintln!(
        "[FEHLER] Dieses Build enthält den Selbst-Update nicht (ohne Feature `upgrade` \
         gebaut — cargo build --features upgrade)."
    );
    std::process::exit(ExitCode::GeneralError.code());
}

/// `agentkit --upgrade [VERSION]` — lädt das passende Release-Asset und
/// ersetzt die laufende Binary. Der eigene Pfad wird über `current_exe` plus
/// `canonicalize` bestimmt (löst Symlinks auf, damit z. B. ein `~/.cargo/bin`-
/// Symlink nicht versehentlich woanders landet als das eigentliche Ziel).
#[cfg(feature = "upgrade")]
fn run_upgrade_cmd(gewuenscht: Option<&str>) -> std::io::Result<()> {
    let eigener_pfad = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[FEHLER] Eigener Pfad nicht bestimmbar: {e}");
            std::process::exit(ExitCode::GeneralError.code());
        }
    };
    let mit_tui = cfg!(feature = "tui");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let netz = agentkit_app::upgrade::UreqNetz;
    match agentkit_app::upgrade::fuehre_upgrade_aus(
        gewuenscht,
        &netz,
        &eigener_pfad,
        mit_tui,
        os,
        arch,
    ) {
        Ok(meldung) => {
            eprintln!("{meldung}");
            Ok(())
        }
        Err(e) => {
            eprintln!("[FEHLER] {e}");
            std::process::exit(ExitCode::GeneralError.code());
        }
    }
}

/// Die Flags eines `work`-Aufrufs, die das FRONTEND kennt und
/// `agentkit_work::cli` nicht (dort wären sie unbekannte Optionen).
#[cfg(feature = "work")]
struct WorkFrontendFlags {
    /// Alles Übrige — geht unverändert an `agentkit_work::cli::dispatch`.
    rest: Vec<String>,
    no_swarm: bool,
    graph_dir: Option<String>,
    graph_readonly: bool,
    trace_dir: Option<String>,
    /// Zusätzlicher System-Prompt der Item-Agenten (`--system`/`--system-file`).
    system: Option<String>,
    /// Kontext-Management für die Item-Agenten (`--ctx DIR`).
    ctx: Option<String>,
    ctx_budget: Option<u32>,
    /// Arbeits-Scope des Graphen (`--graph-scope ID`) — siehe `graph_run_id`.
    graph_scope: Option<String>,
    /// Schreibgeschützte Pfade (`--protect-paths`) für JEDEN Item-Agenten.
    /// Muss hier durch: Ein Work-Lauf baut je Versuch einen frischen Agenten,
    /// und eine Sperre, die nur der Einzelagenten-Pfad kennt, gälte dort nicht.
    protect_paths: Vec<String>,
}

/// Zieht die Frontend-Flags aus `argv` heraus. `argv` muss bereits durch
/// [`normalize_args`] gelaufen sein (`--graph=DIR` -> zwei Tokens), sonst
/// würde `--graph=DIR` nicht erkannt.
#[cfg(feature = "work")]
fn extract_frontend_flags(argv: &[String]) -> WorkFrontendFlags {
    let mut flags = WorkFrontendFlags {
        rest: Vec::with_capacity(argv.len()),
        no_swarm: false,
        graph_dir: None,
        graph_readonly: false,
        trace_dir: None,
        system: None,
        ctx: None,
        ctx_budget: None,
        graph_scope: None,
        protect_paths: Vec::new(),
    };
    let mut it = argv.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-swarm" => flags.no_swarm = true,
            "--graph" => flags.graph_dir = it.next(),
            "--graph-readonly" => flags.graph_readonly = true,
            "--graph-scope" => flags.graph_scope = it.next(),
            "--protect-paths" => {
                flags.protect_paths = it
                    .next()
                    .map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "--trace" => flags.trace_dir = it.next(),
            // Wie im übrigen CLI: `--system-file` sticht `--system`, wenn
            // beides angegeben ist (letzter gewinnt, weil er später zuweist).
            "--ctx" => flags.ctx = it.next(),
            "--ctx-budget" => flags.ctx_budget = it.next().and_then(|v| v.parse().ok()),
            "--system" => flags.system = it.next(),
            "--system-file" => match it.next().map(std::fs::read_to_string) {
                Some(Ok(s)) => flags.system = Some(s),
                Some(Err(e)) => eprintln!("[WARN] --system-file nicht lesbar: {e}"),
                None => eprintln!("[WARN] --system-file ohne Pfad"),
            },
            _ => flags.rest.push(a),
        }
    }
    flags
}

/// Baut die [`agentkit_app::FrontendTools`] für einen `work`-Lauf — dieselben
/// Fähigkeiten wie ein normaler Agentenlauf (Schwarm an, Graph optional über
/// `--graph DIR`), nur direkt aus den drei Flags statt aus `Args`, weil die
/// Work-CLI keine eigene `Args`-Instanz hat — plus den [`GraphGateway`]-Adapter
/// für `WorkCliDeps::graph` (Phase 4 des `agentkit-work`-Konzepts).
///
/// Der Work-Agent bekommt aus den zurückgegebenen `ExtraTools` NUR die
/// LESENDEN Graph-Tools (`graph_search`/`graph_neighbors`/`graph_evidence`),
/// nie `graph_remember`/`graph_promote` — unabhängig von `--graph-readonly`.
/// Begründung: aus einem Work Item heraus soll es GENAU EINEN Weg geben,
/// Wissen zu schreiben, und der muss Provenance tragen (`work_claim`, über
/// den zweiten Rückgabewert). Ein zweiter, provenienzloser Schreibweg über
/// `graph_remember` wäre schlimmer als gar keiner. Erzwungen wird das über
/// denselben Mechanismus, mit dem `register_graph_tools` seine Schreib-Tools
/// selbst gated (`GraphAccess::can_write`/`can_promote`): eine
/// [`agentkit_app::GraphAccess::read_only`] mit UNVERÄNDERTER Sicht, statt
/// Tools nachträglich aus der Registry zu entfernen.
///
/// [`GraphGateway`]: agentkit_work::GraphGateway
#[cfg(feature = "work")]
#[cfg_attr(not(feature = "graph"), allow(unused_variables))]
fn work_frontend_tools(
    no_swarm: bool,
    graph_dir: Option<&str>,
    graph_readonly: bool,
    graph_scope: Option<&str>,
    work_argv: &[String],
) -> (
    Option<agentkit::ExtraTools>,
    Option<Arc<dyn agentkit_work::GraphGateway>>,
) {
    #[allow(unused_mut)]
    #[cfg_attr(not(feature = "graph"), allow(clippy::needless_update))]
    let mut extras = agentkit_app::FrontendTools {
        swarm: !no_swarm,
        ..Default::default()
    };
    #[cfg(feature = "graph")]
    let mut graph_gateway: Option<Arc<dyn agentkit_work::GraphGateway>> = None;
    #[cfg(feature = "graph")]
    if let Some(dir) = graph_dir {
        // Workspace-Identität des Graphen: "." (Prozess-Arbeitsverzeichnis) —
        // derselbe Default wie `agentkit_work::cli`s eigenes `-w`/`--workspace`
        // (siehe `workspace_of` dort). Ein Vorhaben mit eigenem `-w DIR` läuft
        // in der Praxis ohnehin aus diesem Verzeichnis heraus; eine exakte
        // Übernahme des Work-eigenen `-w`-Werts würde eine zweite Kopie des
        // Work-Argument-Scans erfordern, für ein Feld, das nur die Graph-
        // Fähigkeit betrifft (nicht den Work-Lauf selbst) — nicht mehr Aufwand
        // wert, solange kein Fall bekannt ist, der es braucht (YAGNI).
        match agentkit_app::open_graph(
            dir,
            ".",
            &work_graph_run_id(work_argv, graph_scope),
            graph_readonly,
        ) {
            Ok(setup) => {
                // Der Adapter bekommt den ECHTEN Zugriff (schreibfähig, außer
                // bei `--graph-readonly`) — er ist der einzige Weg, der
                // Provenance trägt. Die direkt registrierten Tools (unten)
                // bekommen dieselbe Sicht, aber IMMER nur lesend.
                let read_only_view = setup.access.view.clone();
                graph_gateway = Some(Arc::new(agentkit_app::WorkGraphAdapter {
                    store: setup.store.clone(),
                    access: setup.access,
                }) as Arc<dyn agentkit_work::GraphGateway>);
                extras.graph = Some(agentkit_app::GraphSetup {
                    store: setup.store,
                    access: agentkit_app::GraphAccess::read_only("agentkit-work", read_only_view),
                });
            }
            Err(e) => {
                eprintln!("[FEHLER] --graph: {e}");
                std::process::exit(ExitCode::GeneralError.code());
            }
        }
    }
    #[cfg(not(feature = "graph"))]
    let graph_gateway: Option<Arc<dyn agentkit_work::GraphGateway>> = None;
    #[cfg(not(feature = "graph"))]
    if graph_dir.is_some() {
        eprintln!(
            "[WARN] --graph ignoriert — Binary ohne Feature `graph` gebaut \
             (cargo build --features graph)."
        );
    }
    (extras.build(), graph_gateway)
}

/// Scope des vorläufigen Arbeitswissens für einen `work`-Lauf: die Projekt-ID,
/// falls sie sich aus den Argumenten erkennen lässt (zweites Token, wenn es
/// keine Option ist — trifft auf `run`/`resume`/`status`/`items`/`events` zu,
/// deren Aufruf `work <unterkommando> <projekt-id> …` lautet), sonst das Wort
/// "work" (z. B. bei `work create`/`work list`, wo noch kein Projekt feststeht).
/// Dasselbe Prinzip wie `graph_run_id` beim normalen Lauf: ein stabiler Scope,
/// den ein wiederholter Aufruf für dasselbe Vorhaben wiederfindet.
#[cfg(all(feature = "work", feature = "graph"))]
/// `scope` (`--graph-scope`) sticht die Projekt-ID: Wer mehrere Work-Läufe
/// bewusst dasselbe Wissen teilen lassen will, kann das sonst nicht sagen —
/// jede Projekt-ID ist neu, und der Graph zerfiele in ein Projekt je Insel.
#[cfg(all(feature = "work", feature = "graph"))]
fn work_graph_run_id(work_argv: &[String], scope: Option<&str>) -> String {
    if let Some(s) = scope.map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    work_argv
        .get(1)
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "work".to_string())
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
    opts="-w --workspace -s --strategy --skills --agents --memory --session -c --continue --resume --model --notify \
--provider --demo \
--max-steps --plan --plain --react --no-subagents --no-swarm --no-project-instructions \
-y --yes --steps --no-color -p --print \
--tui --repl --format --dry-run --verify --shell-timeout --max-context --json-retries \
--ctx --ctx-budget --ctx-policy --ctx-compaction-model --graph --graph-readonly --trace --token-limit --hooks \
--mcp-config --mcp --no-mcp \
--system --system-file --profile --upgrade -h --help -V --version \
--tools --check --schema --each -j --jobs --patch --cache --timeout -f --file -o --output -q --quiet"
    # Erstes Wort: auch die Verben `completions`/`read-pdf`/`config`/`work` anbieten.
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "completions read-pdf config work viz mcp-serve acp run $opts" -- "$cur") )
        return 0
    fi
    case "$prev" in
        completions) COMPREPLY=( $(compgen -W "bash zsh fish powershell" -- "$cur") ); return 0;;
        read-pdf) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
        config) COMPREPLY=( $(compgen -W "show path init" -- "$cur") ); return 0;;
        work) COMPREPLY=( $(compgen -W "create list run resume status items events watch budget pause retry approve reject" -- "$cur") ); return 0;;
        viz) COMPREPLY=( $(compgen -W "--trace --trace-file --work --graph --port --open" -- "$cur") ); return 0;;
        -s|--strategy) COMPREPLY=( $(compgen -W "react plan plain plan_execute" -- "$cur") ); return 0;;
        --provider) COMPREPLY=( $(compgen -W "auto azure openai anthropic demo" -- "$cur") ); return 0;;
        --format) COMPREPLY=( $(compgen -W "text json stream-json" -- "$cur") ); return 0;;
        --tools) COMPREPLY=( $(compgen -W "none read_only read_file,grep,glob_files" -- "$cur") ); return 0;;
        run) COMPREPLY=( $(compgen -W "$(agentkit run 2>/dev/null)" -- "$cur") ); return 0;;
        --cache) COMPREPLY=( $(compgen -d -- "$cur") ); return 0;;
        --schema|-f|--file|-o|--output) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
        -w|--workspace|--skills|--agents|--ctx|--graph|--trace|--trace-file|--work) COMPREPLY=( $(compgen -d -- "$cur") ); return 0;;
        --memory|--session|--mcp-config|--system-file|--profile|--ctx-policy) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
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
    # Wort direkt nach `work` -> dessen Unterkommandos anbieten (analog zum
    # bash-`case "$prev"`); alles andere fällt auf die normale Options-/
    # Datei-Vervollständigung unten durch.
    local prev="${words[CURRENT-1]}"
    if [[ "$prev" == "work" ]]; then
        _values 'work-unterkommando' create list run resume status items events watch budget pause retry approve reject
        return
    fi
    if [[ "$prev" == "viz" ]]; then
        _values 'viz-option' --trace --trace-file --work --graph --port --open
        return
    fi
    opts=(
        '1:verb:(completions read-pdf config work viz mcp-serve acp run)'
        '-w[Arbeitsverzeichnis]:dir:_files -/'
        '--workspace[Arbeitsverzeichnis]:dir:_files -/'
        '-s[Strategie]:strategy:(react plan plain)'
        '--strategy[Strategie]:strategy:(react plan plain plan_execute)'
        '--skills[Skills-Verzeichnis]:dir:_files -/'
        '--agents[Custom-Rollen-Verzeichnis]:dir:_files -/'
        '--memory[Langzeitgedächtnis (JSONL)]:file:_files'
        '--session[Session-Datei (Resume)]:file:_files'
        '--model[Modell überschreiben]:name:'
        '--notify[Glocke/Desktop-Meldung bei langen Läufen]'
        '(-c --continue)'{-c,--continue}'[jüngste Sitzung dieses Projekts fortsetzen]'
        '--resume[Sitzung aus der Liste auswählen]'
        '--provider[LLM-Anbieter]:provider:(auto azure openai anthropic demo)'
        '--demo[Demo-Modus erzwingen]'
        '--max-steps[Max. Loop-Schritte]:n:'
        '--plan[Plan-Strategie]'
        '--plain[Plain-Strategie]'
        '--react[ReAct-Strategie]'
        '--no-subagents[task-Tool deaktivieren]'
        '--no-swarm[swarm-Tool deaktivieren]'
        '--no-project-instructions[AGENTS.md nicht laden]'
        '-y[Shell ohne Rückfrage]'
        '--yes[Shell ohne Rückfrage]'
        '--steps[Schritt-Grenzen anzeigen]'
        '--no-color[Farbe aus]'
        '-p[Nur finale Antwort]'
        '--print[Nur finale Antwort]'
        '--tui[Terminal-UI]'
        '--repl[Interaktive Session]'
        '--format[Ausgabeformat]:format:(text json stream-json)'
        '--tools[Werkzeuge (none oder Liste)]:tools:(none read_only)'
        '--check[Ja/Nein-Prüfung als Exit-Code]'
        '--schema[Antwort nach JSON-Schema]:file:_files'
        '--each[Jede stdin-Zeile ein Auftrag]'
        '(-j --jobs)'{-j,--jobs}'[Parallele Aufträge mit --each]:n:'
        '--patch[Unified Diff statt Schreiben]'
        '--cache[Ergebnis-Cache]:dir:_files -/'
        '--timeout[Laufzeit-Obergrenze]:dauer:'
        '*'{-f,--file}'[Datei als Kontext]:file:_files'
        '(-o --output)'{-o,--output}'[Resultat in Datei]:file:_files'
        '(-q --quiet)'{-q,--quiet}'[stderr stumm]'
        '--dry-run[Schreibvorgänge blockieren]'
        '--verify[Vor dem Abschluss selbst verifizieren]'
        '--shell-timeout[Timeout für run_shell (Sekunden)]:n:'
        '--ctx[Kontext-Management-Verzeichnis]:dir:_files -/'
        '--ctx-budget[Kontext-Budget (Tokens)]:n:'
        '--ctx-policy[Kontext-Policy (JSON)]:file:_files'
        '--ctx-compaction-model[Modell für die Verdichtung]:name:'
        '--graph[Wissensgraph-Verzeichnis]:dir:_files -/'
        '--graph-readonly[Graph nur lesen]'
        '--trace[Ereignisstrom als NDJSON mitschreiben]:dir:_files -/'
        '--token-limit[Abbruch ab N gemessenen Tokens]:n:'
        '--hooks[Hook-Datei (JSON)]:file:_files'
        '--max-context[Kontext-Limit (Tokens)]:n:'
        '--json-retries[JSON-Versuche]:n:'
        '--mcp-config[MCP-Config]:file:_files'
        '--mcp[MCP-Server-Allowlist]:name:'
        '--no-mcp[MCP aus]'
        '--system[Zusatz-System-Prompt]:text:'
        '--system-file[System-Prompt-Datei]:file:_files'
        '--profile[Config-Bündel (JSON)]:file:_files'
        '--upgrade[Selbst-Update]::version:'
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
complete -c agentkit -n '__fish_use_subcommand' -a work -d 'Arbeits-Runtime (Feature `work`)'
complete -c agentkit -n '__fish_use_subcommand' -a viz -d 'Trace im Browser ansehen (Feature `viz`)'
complete -c agentkit -n '__fish_use_subcommand' -a mcp-serve -d 'agentkit als MCP-Server (stdio)'
complete -c agentkit -n '__fish_use_subcommand' -a acp -d 'agentkit als ACP-Agent für Editoren'
complete -c agentkit -n '__fish_use_subcommand' -a run -d 'Eigenen Befehl ausführen'
complete -c agentkit -n '__fish_seen_subcommand_from run' -a '(agentkit run 2>/dev/null)'
complete -c agentkit -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell'
complete -c agentkit -n '__fish_seen_subcommand_from config' -a 'show path init'
complete -c agentkit -n '__fish_seen_subcommand_from work' -a 'create list run resume status items events watch budget pause retry approve reject'
complete -c agentkit -n '__fish_seen_subcommand_from viz' -a '--trace --trace-file --work --graph --port --open'
complete -c agentkit -s w -l workspace -r -d 'Arbeitsverzeichnis'
complete -c agentkit -s s -l strategy -x -a 'react plan plain' -d 'Strategie'
complete -c agentkit -l skills -r -d 'Skills-Verzeichnis'
complete -c agentkit -l agents -r -d 'Custom-Rollen-Verzeichnis'
complete -c agentkit -l memory -r -d 'Langzeitgedächtnis (JSONL)'
complete -c agentkit -l session -r -d 'Session-Datei (Resume)'
complete -c agentkit -s c -l continue -d 'Jüngste Sitzung fortsetzen'
complete -c agentkit -l resume -d 'Sitzung aus der Liste auswählen'
complete -c agentkit -l model -x -d 'Modell überschreiben'
complete -c agentkit -l notify -d 'Meldung bei langen Läufen'
complete -c agentkit -l provider -x -a 'auto azure openai anthropic demo' -d 'LLM-Anbieter'
complete -c agentkit -l demo -d 'Demo-Modus erzwingen'
complete -c agentkit -l max-steps -x -d 'Max. Loop-Schritte'
complete -c agentkit -l plan -d 'Plan-Strategie'
complete -c agentkit -l plain -d 'Plain-Strategie'
complete -c agentkit -l react -d 'ReAct-Strategie'
complete -c agentkit -l no-subagents -d 'task-Tool deaktivieren'
complete -c agentkit -l no-swarm -d 'swarm-Tool deaktivieren'
complete -c agentkit -l no-project-instructions -d 'AGENTS.md nicht laden'
complete -c agentkit -s y -l yes -d 'Shell ohne Rückfrage'
complete -c agentkit -l steps -d 'Schritt-Grenzen anzeigen'
complete -c agentkit -l no-color -d 'Farbe aus'
complete -c agentkit -s p -l print -d 'Nur finale Antwort'
complete -c agentkit -l tui -d 'Terminal-UI'
complete -c agentkit -l repl -d 'Interaktive Session'
complete -c agentkit -l format -x -a 'text json stream-json' -d 'Ausgabeformat'
complete -c agentkit -l tools -x -a 'none read_only' -d 'Werkzeuge (none oder Liste)'
complete -c agentkit -l check -d 'Ja/Nein-Prüfung als Exit-Code'
complete -c agentkit -l schema -r -d 'Antwort nach JSON-Schema'
complete -c agentkit -l each -d 'Jede stdin-Zeile ein Auftrag'
complete -c agentkit -s j -l jobs -x -d 'Parallele Aufträge mit --each'
complete -c agentkit -l patch -d 'Unified Diff statt Schreiben'
complete -c agentkit -l cache -r -d 'Ergebnis-Cache'
complete -c agentkit -l timeout -x -d 'Laufzeit-Obergrenze'
complete -c agentkit -s f -l file -r -d 'Datei als Kontext'
complete -c agentkit -s o -l output -r -d 'Resultat in Datei'
complete -c agentkit -s q -l quiet -d 'stderr stumm'
complete -c agentkit -l dry-run -d 'Schreibvorgänge blockieren'
complete -c agentkit -l verify -d 'Vor dem Abschluss selbst verifizieren'
complete -c agentkit -l shell-timeout -x -d 'Timeout für run_shell (Sekunden)'
complete -c agentkit -l ctx -r -d 'Kontext-Management-Verzeichnis'
complete -c agentkit -l ctx-budget -x -d 'Kontext-Budget (Tokens)'
complete -c agentkit -l ctx-policy -r -d 'Kontext-Policy (JSON)'
complete -c agentkit -l ctx-compaction-model -x -d 'Modell für die Verdichtung'
complete -c agentkit -l graph -r -d 'Wissensgraph-Verzeichnis'
complete -c agentkit -l graph-readonly -d 'Graph nur lesen'
complete -c agentkit -l trace -r -d 'Ereignisstrom als NDJSON mitschreiben'
complete -c agentkit -l token-limit -x -d 'Abbruch ab N gemessenen Tokens'
complete -c agentkit -l hooks -r -d 'Hook-Datei (JSON)'
complete -c agentkit -l max-context -x -d 'Kontext-Limit (Tokens)'
complete -c agentkit -l json-retries -x -d 'JSON-Versuche'
complete -c agentkit -l mcp-config -r -d 'MCP-Config'
complete -c agentkit -l mcp -x -d 'MCP-Server-Allowlist'
complete -c agentkit -l no-mcp -d 'MCP aus'
complete -c agentkit -l system -x -d 'Zusatz-System-Prompt'
complete -c agentkit -l system-file -r -d 'System-Prompt-Datei'
complete -c agentkit -l profile -r -d 'Config-Bündel (JSON)'
complete -c agentkit -l upgrade -x -d 'Selbst-Update (optional: Zielversion)'
complete -c agentkit -s h -l help -d 'Hilfe'
complete -c agentkit -s V -l version -d 'Version'
"#;

const COMPLETIONS_PWSH: &str = r#"# PowerShell-Vervollständigung für agentkit.
# Einbinden:  agentkit completions powershell | Out-String | Invoke-Expression
# Dauerhaft:  agentkit completions powershell >> $PROFILE
Register-ArgumentCompleter -Native -CommandName agentkit -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $opts = @(
        'completions','read-pdf','config','work','viz','mcp-serve','acp','run','-w','--workspace','-s','--strategy','--skills','--agents','--memory','--session','-c','--continue','--resume','--model','--notify',
        '--provider','--demo','--max-steps','--plan','--plain','--react','--no-subagents','--no-swarm','--no-project-instructions',
        '-y','--yes','--steps','--no-color','-p','--print','--tui','--repl','--format',
        '--dry-run','--verify','--shell-timeout','--max-context','--json-retries',
        '--ctx','--ctx-budget','--ctx-policy','--ctx-compaction-model','--graph','--graph-readonly','--trace','--token-limit','--hooks',
        '--mcp-config','--mcp','--no-mcp',
        '--system','--system-file','--profile','--upgrade','-h','--help','-V','--version',
        '--tools','--check','--schema','--each','-j','--jobs','--patch','--cache','--timeout',
        '-f','--file','-o','--output','-q','--quiet'
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
        'work'        { @('create','list','run','resume','status','items','events','watch','budget','pause','retry','approve','reject') }
        'viz'         { @('--trace','--trace-file','--work','--graph','--port','--open') }
        '-s'          { @('react','plan','plain') }
        '--strategy'  { @('react','plan','plain','plan_execute') }
        '--provider'  { @('auto','azure','openai','anthropic','demo') }
        '--format'    { @('text','json','stream-json') }
        '--tools'     { @('none','read_only') }
        default       { $opts }
    }
    $values | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}
"#;

/// Der Hilfetext selbst — als Funktion statt eines direkt gedruckten String-Literals,
/// damit ein Test prüfen kann, dass neue Abschnitte (z. B. `work`) wirklich drin
/// stehen, ohne stdout mitschneiden zu müssen. Rein mechanisches Herausziehen aus
/// `print_help`, keine Verhaltensänderung.
fn cli_help_text() -> String {
    format!(
        "agentkit {VERSION} — Claude-Code-artiges CLI/TUI für den agentkit-Agenten\n\n\
         AUFRUF:\n  agentkit [OPTIONEN] [AUFTRAG …]\n\n\
         BETRIEBSARTEN:\n  \
           agentkit \"Frage\"        One-shot: Auftrag ausführen, Antwort streamen\n  \
           agentkit                 interaktive Session (REPL)\n  \
           agentkit --tui           interaktives Terminal-UI (nur mit Feature `tui`)\n  \
           agentkit config          Konfiguration prüfen (show|path|init) — ~/.agentkit/config.json\n  \
           agentkit completions SH  Shell-Completion ausgeben (bash|zsh|fish|powershell)\n  \
           agentkit read-pdf FILE   PDF-Text extrahieren auf stdout (kein LLM; Feature `pdf`)\n  \
           agentkit work SUB        Arbeits-Runtime (Feature `work`): Vorhaben in Work Items\n  \
                                    zerlegen und abarbeiten — überlebt den Prozess. SUB ist eins\n  \
                                    von create|list|run|resume|status|items|events|watch|budget|\n  \
                                    pause|retry|approve|reject. Details: `agentkit work --help`\n  \
           agentkit viz             Ereignisstrom eines Laufs im Browser ansehen (Feature `viz`):\n  \
                                    Agenten, Verlauf, Kontext, Schwarm-Verkehr, Graph und Work.\n  \
                                    Braucht einen mit `--trace DIR` geschriebenen Trace.\n  \
                                    Details: `agentkit viz --help`\n  \
           agentkit mcp-serve       agentkit als MCP-Server auf stdio (für Claude Code, Cursor, …):\n  \
                                    Tool `agentkit` delegiert einen Auftrag; mit --expose-tools\n  \
                                    zusätzlich die Werkzeuge selbst. Optionen wie unten (-w, -y, …)\n  \
           agentkit acp             agentkit als Agent für Editoren mit Agent Client Protocol\n  \
                                    (z. B. Zed); Shell-Freigaben fragt der Editor\n  \
           agentkit run NAME        eigener Befehl aus ~/.agentkit/commands/NAME.md (Frontmatter =\n  \
                                    Einstellungen wie --profile, Text = System-Prompt). Als Symlink\n  \
                                    (`ln -s agentkit NAME`) geht auch `cat log | NAME`\n\n\
         UNIX-PIPE:\n  \
           stdin  = Kontext (per Pipe), wird an die Query angehängt\n  \
           stdout = nur das finale Resultat (bei Pipe/--format json/--print)\n  \
           stderr = Status, Tool-Spur, ReAct-Gedanken, Fehler\n  \
           Exit:  0 Erfolg · 1 Laufzeit · 2 API/Netz · 3 Kontext/Prompt · 4 Format · 124 Timeout\n\n\
         UNIX-WERKZEUG:\n  \
           --tools none|LISTE    none = reiner Modell-Aufruf ohne Werkzeuge (schnell, billig,\n  \
                                 ohne Rückfrage); LISTE = nur diese Werkzeuge, z. B.\n  \
                                 read_file,grep oder read_only\n  \
           --check               Ja/Nein-Prüfung: Exit 0 = ja, 1 = nein; Begründung auf stderr\n  \
           --schema FILE         Antwort als JSON nach JSON-Schema; wo möglich erzwingt der\n  \
                                 Anbieter die Struktur, sonst prüft agentkit und wiederholt\n  \
           --each                jede stdin-Zeile ein eigener Auftrag ({{}} im Prompt = die Zeile),\n  \
                                 Ergebnisse als JSONL in Eingabe-Reihenfolge\n  \
           -j, --jobs N          mit --each: N Aufträge gleichzeitig (Default: 1)\n  \
           --patch               auf einer Kopie arbeiten, Unified Diff auf stdout (git apply)\n  \
           --cache DIR           Ergebnis bei gleichem Auftrag/Modell/Format wiederverwenden\n  \
           --timeout DAUER       Obergrenze der Laufzeit (90, 30s, 5m, 1h) -> sonst Exit 124\n  \
           -f, --file DATEI      Datei mit Namen als Kontext (mehrfach möglich)\n  \
           -o, --output DATEI    Resultat in DATEI statt auf stdout\n  \
           -q, --quiet           stderr komplett stumm; Shell nur per -y/allow-Liste\n\n\
         OPTIONEN:\n  \
           -w, --workspace DIR   Sandbox-/Arbeitsverzeichnis (Default: .)\n  \
           -s, --strategy S      react | plan | plain | plan_execute (Default: react)\n  \
                                 plan_execute: erst planen, dann je Schritt ein\n  \
                                 ReAct-Durchlauf mit kurzer Erledigt-Prüfung;\n  \
                                 erweist sich der Plan als nicht zielführend,\n  \
                                 wird der Rest umgeplant (begrenzt, s. Profil)\n  \
           --react/--plan/--plain  Kurzform für -s\n  \
           --skills DIR          Skills-Verzeichnis aktivieren (SKILL.md-Ordner);\n  \
                                 mehrfach angebbar, wird intern mit `;` verkettet\n  \
           --agents DIR          Custom-Sub-Agenten aus *.md laden (subagent_type)\n  \
           --memory FILE         Langzeitgedächtnis (JSONL) für remember/recall\n  \
           --session FILE        Verlauf laden/speichern — Resume über Prozessgrenzen\n  \
           --model NAME          Modell überschreiben (statt OPENAI_MODEL o. Ä.)\n  \
           --notify              Glocke/Desktop-Meldung bei langen Läufen\n  \
           -c, --continue        jüngste Sitzung dieses Projekts fortsetzen\n  \
           --resume              Sitzung aus der Liste auswählen\n  \
           --ctx DIR             ctxman-Kontext-Management aktivieren (Feature `ctxman`):\n  \
                                 Watermarks/GC, expand_context_ref, Snapshot-Resume in DIR\n  \
           --ctx-budget N        Kontext-Budget B in Tokens für --ctx (Default: 100000)\n  \
           --ctx-policy FILE     partielles Policy-Overlay (JSON) für --ctx: Watermarks,\n  \
                                 kinds-TTLs, tokenizer (heuristic|o200k|cl100k), max_share, …\n  \
           --ctx-compaction-model NAME  separates (günstiges) LLM nur für Compaction/\n  \
                                 Fact-Extraction (Azure-Deployment- bzw. OpenAI-Modellname)\n  \
           --graph DIR           Wissensgraph in DIR aktivieren (Feature `graph`):\n  \
                                 graph_search/-neighbors/-evidence/-remember/-promote,\n  \
                                 dauerhaftes Wissen je Workspace, Arbeitsstand je Session\n  \
           --graph-readonly      Graph nur lesen (kein graph_remember/graph_promote)\n  \
           --allow-read DIR      zusätzliche NUR-LESBARE Sandbox-Wurzel (mehrfach möglich);\n  \
                                 KEIN -w — die Schreib-Sandbox bleibt eng, nur read_file/\n  \
                                 glob_files/grep dürfen dorthin\n  \
           --trace DIR           kompletten Ereignisstrom als NDJSON nach DIR mitschreiben\n  \
                                 (z. B. .agentkit/trace) — Datengrundlage für `agentkit viz`.\n  \
                                 ACHTUNG: enthält Dateiinhalte, Shell-Ausgaben und Modell-\n  \
                                 antworten unredigiert, also möglicherweise Geheimnisse\n  \
           --provider P          auto | azure | openai | anthropic | demo (Default: auto)\n  \
           --demo                Demo-Modus erzwingen (netzfrei)\n  \
           --max-steps N         Max. Loop-Schritte (Default: 600)\n  \
           --token-limit N       Auftrag abbrechen, sobald die gemessenen Tokens (ein + aus,\n  \
                                 alle Agenten zusammen) N übersteigen (Exit 1)\n  \
           --hooks FILE          Hook-Datei (JSON): Shell-Kommandos vor/nach Coding-Tools,\n  \
                                 Exit 2 blockiert bzw. meldet zurück. Zusätzlich wird\n  \
                                 ~/.agentkit/hooks.json immer geladen\n  \
           --verify              vor der finalen Antwort einen ausgeführten Check verlangen\n  \
           --shell-timeout N     Timeout je run_shell-Befehl in Sekunden (Default: 120)\n  \
           --no-subagents        das 'task'-Tool deaktivieren\n  \
           --no-swarm            das 'swarm'-Tool (dynamische Agenten-Schwärme) deaktivieren\n  \
           --no-project-instructions  AGENTS.md nicht laden (weder Prompt noch Leitplanken)\n  \
           -y, --yes             Shell-Befehle ohne Rückfrage ausführen\n  \
           --steps               Schritt-Grenzen anzeigen\n  \
           --no-color            Farbausgabe aus\n  \
           -p, --print           One-shot: nur finale Antwort ausgeben\n  \
           --format T            text | json | stream-json (json: erzwingt + validiert\n  \
                                 strukturierten Output; stream-json: jedes Ereignis als\n  \
                                 JSON-Zeile auf stdout, zum Schluss ein result-Datensatz)\n  \
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
           --upgrade [X.Y.Z]     Selbst-Update (Feature `upgrade`): ohne Angabe die neueste\n  \
                                 Release-Version, mit Angabe (auch `vX.Y.Z`) genau diese —\n  \
                                 auch ein Downgrade. Ersetzt die laufende Binary\n  \
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
    )
}

fn print_help() {
    println!("{}", cli_help_text());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Legt einen kleinen Workspace an und gibt seinen Pfad zurück.
    fn tmp_ws(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentkit_comp_{}_{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        std::fs::write(dir.join("src/main.rs"), "x").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
        std::fs::write(dir.join(".versteckt"), "x").unwrap();
        dir
    }

    /// `--skills` ist mehrfach angebbar und verkettet dann mit `;`.
    #[test]
    fn mehrfaches_skills_flag_wird_verkettet() {
        let a = Args::parse(&v(&["--skills", "/a", "--skills", "/b", "frage"]));
        assert_eq!(a.skills.as_deref(), Some("/a;/b"));
    }

    /// Gegenüber einem Profil verhält sich `--skills` wie jedes andere Flag:
    /// es ERSETZT den Profilwert, es hängt nicht an. Sonst ließe sich ein im
    /// Profil gesetztes Skills-Verzeichnis über die Kommandozeile nicht mehr
    /// abwählen — entgegen der zugesicherten Reihenfolge (Flag schlägt Profil).
    #[test]
    fn skills_flag_ersetzt_profilwert_statt_anzuhaengen() {
        let dir = std::env::temp_dir().join(format!("agentkit_prof_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profil = dir.join("p.json");
        std::fs::write(&profil, r#"{"skills": "/aus/dem/profil"}"#).unwrap();

        let nur_profil = Args::parse(&v(&["--profile", profil.to_str().unwrap(), "frage"]));
        assert_eq!(nur_profil.skills.as_deref(), Some("/aus/dem/profil"));

        let mit_flag = Args::parse(&v(&[
            "--profile",
            profil.to_str().unwrap(),
            "--skills",
            "/vom/flag",
            "frage",
        ]));
        assert_eq!(mit_flag.skills.as_deref(), Some("/vom/flag"));

        // Zwei Flags verketten weiterhin miteinander — aber nicht mit dem Profil.
        let zwei_flags = Args::parse(&v(&[
            "--profile",
            profil.to_str().unwrap(),
            "--skills",
            "/eins",
            "--skills",
            "/zwei",
            "frage",
        ]));
        assert_eq!(zwei_flags.skills.as_deref(), Some("/eins;/zwei"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ein Trace entsteht NUR auf ausdrückliche Anforderung — er enthält
    /// unredigiert alles, was der Agent gelesen und geschrieben hat. Ohne
    /// `--trace` bleibt das Feld leer, und `main` legt gar keinen Schreiber an.
    #[test]
    fn ohne_trace_flag_wird_kein_trace_angefordert() {
        assert!(Args::parse(&v(&["Was ist 17 + 25?"])).trace.is_none());
        assert!(Args::parse(&v(&["--steps", "--graph", "g"]))
            .trace
            .is_none());
        assert_eq!(
            Args::parse(&v(&["--trace", ".agentkit/trace", "Auftrag"])).trace,
            Some(".agentkit/trace".to_string())
        );
        // `--flag=wert` greift (GNU/POSIX, siehe `normalize_args`).
        assert_eq!(
            Args::parse(&v(&["--trace=/tmp/t"])).trace,
            Some("/tmp/t".to_string())
        );
    }

    /// Die Frontend-Flags eines `work`-Aufrufs werden herausgezogen, BEVOR der
    /// Rest an `agentkit_work::cli` geht — dort wären sie unbekannte Optionen
    /// und der Aufruf würde mit Exit 1 abgewiesen.
    #[test]
    #[cfg(feature = "work")]
    fn work_frontend_flags_werden_herausgezogen() {
        let flags = extract_frontend_flags(&normalize_args(&v(&[
            "run",
            "demo",
            "--no-swarm",
            "--graph",
            "g",
            "--graph-readonly",
            "--trace=.agentkit/trace",
            "--steps",
        ])));
        assert_eq!(flags.rest, v(&["run", "demo", "--steps"]));
        assert!(flags.no_swarm);
        assert_eq!(flags.graph_dir.as_deref(), Some("g"));
        assert!(flags.graph_readonly);
        assert_eq!(flags.trace_dir.as_deref(), Some(".agentkit/trace"));

        // Ohne die Flags bleibt alles stehen — und kein Trace entsteht.
        let ohne = extract_frontend_flags(&normalize_args(&v(&["run", "demo"])));
        assert_eq!(ohne.rest, v(&["run", "demo"]));
        assert!(ohne.trace_dir.is_none());
    }

    #[test]
    fn permissions_merkt_sich_das_programm() {
        let mut p = Permissions::default();
        assert!(p.fragt_nach("cargo test"), "frisch wird gefragt");

        assert_eq!(p.erlaube_dauerhaft("cargo test --all"), "cargo");
        // Dasselbe Programm mit anderen Argumenten fragt nicht mehr …
        assert!(!p.fragt_nach("cargo build"));
        assert!(!p.fragt_nach("  cargo   fmt  "), "Leerraum ist egal");
        // … ein anderes schon.
        assert!(p.fragt_nach("git push"));
    }

    #[test]
    fn permissions_alles_uebergeht_die_frage() {
        let p = Permissions {
            alles: true,
            ..Default::default()
        };
        assert!(!p.fragt_nach("rm -rf /"), "-y fragt grundsätzlich nicht");
    }

    #[test]
    fn permissions_programm_ist_das_erste_wort() {
        assert_eq!(Permissions::programm("  ls -la  "), "ls");
        assert_eq!(Permissions::programm(""), "");
        assert_eq!(Permissions::programm("git"), "git");
    }

    /// Eine aus `AGENTKIT_ALLOW` befüllte `Permissions` lässt die gelisteten
    /// Programme ohne Rückfrage laufen. (Das Zerlegen der Liste selbst prüft
    /// `agent_framework_rs/tests/integration.rs` — dort liegt die Funktion.)
    #[test]
    fn permissions_mit_allow_liste_fragt_nicht_mehr_nach() {
        let p = Permissions {
            erlaubt: agentkit::config::allow_aus_liste("docker, git"),
            alles: false,
        };
        assert!(!p.fragt_nach("docker ps"));
        assert!(!p.fragt_nach("git push"));
        assert!(p.fragt_nach("curl https://example.com"));
    }

    /// Der Markdown-Strom gibt Zeilen erst frei, wenn ihr `\n` da ist — sonst
    /// ließe sich eine Zeile nicht auszeichnen. Der Rest bleibt gepuffert.
    #[test]
    fn markdown_stream_gibt_ganze_zeilen_frei() {
        let mut md = MarkdownStream::new(Pal::plain());
        assert_eq!(md.push("Hallo"), "", "angefangene Zeile bleibt im Puffer");
        assert_eq!(md.push(" Welt\nzweite"), "Hallo Welt\n");
        assert_eq!(md.flush(), "zweite");
        assert_eq!(md.flush(), "", "Puffer ist danach leer");
    }

    #[test]
    fn markdown_stream_zeichnet_zeilen_aus() {
        let p = Pal::color();
        let mut md = MarkdownStream::new(p);
        // Überschrift: Rauten weg, fett.
        let out = md.push("## Titel\n");
        assert!(out.contains(p.bold) && out.contains("Titel"), "{out:?}");
        assert!(!out.contains('#'));
        // Aufzählung: Marker wird zum Punkt, Einzug bleibt.
        let out = md.push("  - erster\n");
        assert!(out.starts_with("  "), "{out:?}");
        assert!(out.contains('•') && out.contains("erster"));
    }

    #[test]
    fn markdown_stream_faerbt_code_fences() {
        let p = Pal::color();
        let mut md = MarkdownStream::new(p);
        md.push("```rust\n");
        // Im Fence: keine Inline-Auszeichnung, dafür der Balken.
        let out = md.push("let x = **kein_fett**;\n");
        assert!(out.contains('▏'), "{out:?}");
        assert!(out.contains("**kein_fett**"), "im Code nicht auszeichnen");
        md.push("```\n");
        // Nach dem Fence wieder normal.
        let out = md.push("**fett**\n");
        assert!(out.contains(p.bold) && !out.contains("**"), "{out:?}");
    }

    /// Ein einzelnes Sternchen oder Backtick im Fließtext darf nicht den Rest
    /// der Zeile einfärben.
    #[test]
    fn markdown_stream_laesst_unpaarige_marker_stehen() {
        let mut md = MarkdownStream::new(Pal::plain());
        assert_eq!(md.push("2 ** 8 ist viel\n"), "2 ** 8 ist viel\n");
        assert_eq!(md.push("ein ` Backtick\n"), "ein ` Backtick\n");
    }

    #[test]
    fn completion_schlaegt_slash_befehle_vor() {
        let ws = tmp_ws("slash");
        let (start, treffer) = complete_at("/se", 3, &ws);
        assert_eq!(start, 0);
        assert_eq!(treffer, vec!["/sessions ".to_string()]);

        // Mehrere Treffer bei gemeinsamem Präfix.
        let (_, treffer) = complete_at("/", 1, &ws);
        assert!(treffer.len() > 5, "{treffer:?}");
        assert!(treffer.contains(&"/help ".to_string()));

        // Nach dem ersten Leerzeichen sind es Argumente, kein Befehl mehr.
        assert_eq!(complete_at("/rewind 2", 9, &ws).1, Vec::<String>::new());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn completion_schlaegt_workspace_pfade_vor() {
        let ws = tmp_ws("pfad");
        // Verzeichnisse bekommen ein `/`, damit man weitertabben kann.
        let (start, treffer) = complete_at("lies @sr", 8, &ws);
        assert_eq!(start, 6, "ersetzt wird ab hinter dem @");
        assert_eq!(treffer, vec!["src/".to_string()]);

        // Eine Ebene tiefer, Präfix greift.
        let (start, treffer) = complete_at("lies @src/m", 11, &ws);
        assert_eq!(start, 6);
        assert_eq!(treffer, vec!["src/main.rs".to_string()]);

        // Versteckte Dateien nur auf ausdrücklichen Wunsch.
        assert!(!complete_at("@", 1, &ws)
            .1
            .contains(&".versteckt".to_string()));
        assert!(complete_at("@.", 2, &ws)
            .1
            .contains(&".versteckt".to_string()));

        // Unbekanntes Verzeichnis -> keine Vorschläge, kein Panik.
        assert!(complete_at("@gibtsnicht/x", 13, &ws).1.is_empty());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn completion_bleibt_bei_normalem_text_stumm() {
        let ws = tmp_ws("stumm");
        assert!(complete_at("erklär mir den code", 19, &ws).1.is_empty());
        // `@` mit Leerzeichen dahinter ist keine Pfadangabe mehr.
        assert!(complete_at("mail@ firma", 11, &ws).1.is_empty());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn input_incomplete_erkennt_fence_und_backslash() {
        assert!(input_incomplete("zeig mir ```rust"));
        assert!(input_incomplete("weiter geht's \\"));
        assert!(!input_incomplete("```rust\nfn main() {}\n```"));
        assert!(!input_incomplete("normale eingabe"));
        assert!(!input_incomplete(""));
    }

    #[test]
    fn unix_werkzeug_flags_werden_gelesen() {
        let a = Args::parse(&v(&[
            "--tools",
            "read_file,grep",
            "--check",
            "--each",
            "-j",
            "4",
            "--patch",
            "--cache",
            "/tmp/c",
            "--timeout",
            "5m",
            "-f",
            "a.txt",
            "--file=b.txt",
            "-o",
            "out.json",
            "-q",
            "--format",
            "stream-json",
            "frage",
        ]));
        assert_eq!(a.tools.as_deref(), Some("read_file,grep"));
        assert!(a.check && a.each && a.patch && a.quiet);
        assert_eq!(a.jobs, 4);
        assert_eq!(a.cache.as_deref(), Some("/tmp/c"));
        assert_eq!(a.timeout, Some(std::time::Duration::from_secs(300)));
        assert_eq!(a.files, v(&["a.txt", "b.txt"]));
        assert_eq!(a.output.as_deref(), Some("out.json"));
        assert!(a.stream_json);
        assert_eq!(
            a.format,
            OutputFormat::Text,
            "stream-json ist kein JSON-Modus"
        );
        assert_eq!(a.prompt, "frage");
        // `-j8` ohne Leerzeichen wie bei make.
        assert_eq!(Args::parse(&v(&["-j8", "x"])).jobs, 8);
        assert_eq!(
            Args::parse(&v(&["-j", "0", "x"])).jobs,
            1,
            "mindestens einer"
        );
    }

    /// `--tools none` ist ein reiner Modell-Aufruf — ein ReAct-/Plan-Preamble
    /// würde von Werkzeugen reden, die es nicht gibt.
    #[test]
    fn tools_none_schaltet_auf_plain() {
        let a = Args::parse(&v(&["--tools", "none", "-s", "plan_execute", "x"]));
        assert!(a.tools_none());
        assert_eq!(a.strategy, Strategy::Plain);
        assert!(matches!(
            a.run_strategy,
            RunStrategy::Direct(Strategy::Plain)
        ));
        assert!(!Args::parse(&v(&["--tools", "read_file", "x"])).tools_none());
        assert!(!Args::parse(&v(&["x"])).tools_none());
    }

    #[test]
    fn parse_duration_versteht_einheiten() {
        use std::time::Duration;
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("bald"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn widerspruechliche_pipe_flags_werden_abgelehnt() {
        assert!(
            check_pipe_flags(&Args::parse(&v(&["--check", "--schema", "s.json", "x"]))).is_err()
        );
        assert!(check_pipe_flags(&Args::parse(&v(&["--patch", "--each", "x"]))).is_err());
        assert!(
            check_pipe_flags(&Args::parse(&v(&["--patch", "--format", "stream-json"]))).is_err()
        );
        assert!(check_pipe_flags(&Args::parse(&v(&["--each", "-j", "3", "x"]))).is_ok());
    }

    /// Eine Befehlsdatei wird zum Profil: Frontmatter = Einstellungen (typisiert),
    /// Text = System-Prompt, relative Pfade gelten ab der Befehlsdatei.
    #[test]
    fn befehlsdatei_wird_zum_profil() {
        let text = "---\ntools: none\ncheck: true\nmax_steps: 5\nmcp: a, b\n\
                    schema: s.json\nmodel: gpt-5\n---\nFasse zusammen.\n";
        let p = command_profile(text, Path::new("/cfg/commands"));
        assert_eq!(p["tools"], "none");
        assert_eq!(p["check"], true);
        assert_eq!(p["max_steps"], 5);
        assert_eq!(p["mcp"], serde_json::json!(["a", "b"]));
        assert_eq!(p["model"], "gpt-5");
        assert_eq!(
            PathBuf::from(p["schema"].as_str().unwrap()),
            Path::new("/cfg/commands").join("s.json")
        );
        assert_eq!(p["system"], "Fasse zusammen.");

        // Flags schlagen den Befehl, der Befehl liefert den Rest.
        let a = Args::parse_with(&v(&["--max-steps", "9", "text"]), Some(&p));
        assert!(a.tools_none() && a.check);
        assert_eq!(a.max_steps, 9);
        assert_eq!(a.system.as_deref(), Some("Fasse zusammen."));
        assert_eq!(a.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn befehlsnamen_bleiben_im_verzeichnis() {
        assert!(valid_command_name("summarize"));
        assert!(valid_command_name("pr-review_2"));
        assert!(!valid_command_name("../geheim"));
        assert!(!valid_command_name("a/b"));
        assert!(!valid_command_name(""));
    }

    /// Das gemeinsame Token-Budget meldet das Überschreiten genau einmal.
    #[test]
    fn token_budget_meldet_ueberschreiten_einmal() {
        let b = TokenBudget::new(Some(25));
        assert!(!b.book(15));
        assert!(!b.exhausted());
        assert!(b.book(15), "30 > 25 — jetzt");
        assert!(b.exhausted());
        assert!(!b.book(15), "nicht noch einmal");
        let ohne = TokenBudget::new(None);
        assert!(!ohne.book(1_000_000) && !ohne.exhausted());
    }

    #[test]
    fn job_result_als_json_wert() {
        let mut r = finish_job(
            &Args::parse(&v(&["x"])),
            "Antwort".into(),
            Usage::default(),
            false,
            false,
        );
        assert_eq!(r.value(false), serde_json::json!("Antwort"));
        r.output = r#"{"a":1}"#.into();
        assert_eq!(r.value(true), serde_json::json!({"a": 1}));
        assert!(JobResult::failed(ExitCode::ApiError, Usage::default())
            .value(false)
            .is_null());
    }

    /// `--check`: das Urteil bestimmt den Exit-Code, auch bei einem Cache-Treffer.
    #[test]
    fn check_urteil_wird_exit_code() {
        let a = Args::parse(&v(&["--check", "x"]));
        let ja = finish_job(
            &a,
            r#"{"ergebnis":true,"begruendung":"passt"}"#.into(),
            Usage::default(),
            true,
            false,
        );
        assert_eq!(ja.code, ExitCode::Success);
        assert_eq!(ja.verdict, Some((true, "passt".to_string())));
        let nein = finish_job(
            &a,
            r#"{"ergebnis":false,"begruendung":"nein"}"#.into(),
            Usage::default(),
            false,
            false,
        );
        assert_eq!(nein.code, ExitCode::GeneralError);
        assert!(
            nein.value(true).is_object(),
            "auch ein Nein hat ein Resultat"
        );
    }

    /// Hilfe und alle vier Completions kennen die Unix-Werkzeug-Optionen.
    #[test]
    fn hilfe_und_completions_kennen_unix_optionen() {
        let hilfe = cli_help_text();
        for opt in [
            "--tools",
            "--check",
            "--schema",
            "--each",
            "--jobs",
            "--patch",
            "--cache",
            "--timeout",
            "--file",
            "--output",
            "--quiet",
            "stream-json",
            "agentkit run",
        ] {
            assert!(hilfe.contains(opt), "Hilfe kennt {opt} nicht");
        }
        for (name, script) in [
            ("bash", COMPLETIONS_BASH),
            ("zsh", COMPLETIONS_ZSH),
            ("fish", COMPLETIONS_FISH),
            ("powershell", COMPLETIONS_PWSH),
        ] {
            for opt in [
                "--tools",
                "--check",
                "--schema",
                "--each",
                "--patch",
                "--cache",
                "--timeout",
                "stream-json",
                "run",
            ] {
                // fish schreibt Lang-Optionen als `-l tools`.
                let wort = opt.trim_start_matches('-');
                assert!(script.contains(wort), "{name}-Completion kennt {opt} nicht");
            }
        }
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

    /// Ohne `--continue`/`--resume` wählt `chosen_session` nichts aus. Das ist
    /// die Zusage des TUI-Pfades: ein kurzer Blick ins TUI legt keine
    /// Sitzungsdatei an (der REPL nutzt dafür `resolve_session`).
    #[test]
    fn chosen_session_ohne_flag_waehlt_nichts() {
        let a = Args::parse(&v(&["--tui"]));
        assert!(chosen_session(&a, Pal::plain()).is_none());
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

    // ---------------------------------------------------------- work (Schritt 7/8)

    /// Der Hilfetext muss den `work`-Abschnitt tragen — sonst weiß niemand ohne
    /// `--features work`-Build, dass es das Verb gibt.
    #[test]
    fn cli_help_text_enthaelt_work_abschnitt() {
        let text = cli_help_text();
        assert!(text.contains("agentkit work"));
        assert!(text.contains("agentkit work --help"));
    }

    /// Das `viz`-Verb muss in der Hilfe stehen und seinen eigenen Hilfetext
    /// haben — er ist auch OHNE Feature `viz` einkompiliert, damit
    /// `agentkit viz --help` überall dasselbe erklärt.
    #[test]
    fn hilfe_erklaert_das_viz_verb() {
        let text = cli_help_text();
        assert!(text.contains("agentkit viz"));
        assert!(text.contains("agentkit viz --help"));
        assert!(VIZ_HELP.contains("--trace DIR"));
        assert!(VIZ_HELP.contains("--port N"));
        // Die Warnung gehört in den Hilfetext, nicht nur ins README: der Trace
        // wird unredigiert ausgeliefert.
        assert!(VIZ_HELP.contains("127.0.0.1"));
        assert!(VIZ_HELP.contains("Token"));
    }

    /// Alle vier Shell-Completion-Skripte müssen auch das Verb `viz` kennen.
    #[test]
    fn alle_completions_kennen_das_verb_viz() {
        for (name, script) in [
            ("bash", COMPLETIONS_BASH),
            ("zsh", COMPLETIONS_ZSH),
            ("fish", COMPLETIONS_FISH),
            ("powershell", COMPLETIONS_PWSH),
        ] {
            assert!(
                script.contains("viz"),
                "{name}-Completion kennt 'viz' nicht"
            );
            assert!(
                script.contains("--trace-file"),
                "{name}-Completion kennt '--trace-file' nicht"
            );
        }
    }

    /// Alle vier Shell-Completion-Skripte müssen das Verb `work` kennen — sonst
    /// tippt niemand `agentkit work …` per Tab fertig, obwohl es das Verb gibt.
    #[test]
    fn alle_completions_kennen_das_verb_work() {
        for (name, script) in [
            ("bash", COMPLETIONS_BASH),
            ("zsh", COMPLETIONS_ZSH),
            ("fish", COMPLETIONS_FISH),
            ("powershell", COMPLETIONS_PWSH),
        ] {
            assert!(
                script.contains("work"),
                "{name}-Completion kennt 'work' nicht"
            );
        }
    }

    /// Phase 8 (Observability): alle vier Completion-Generatoren müssen auch
    /// das neue `work watch`-Unterkommando anbieten, sonst tippt niemand es
    /// per Tab fertig, obwohl es existiert.
    #[test]
    fn alle_completions_kennen_das_unterkommando_watch() {
        for (name, script) in [
            ("bash", COMPLETIONS_BASH),
            ("zsh", COMPLETIONS_ZSH),
            ("fish", COMPLETIONS_FISH),
            ("powershell", COMPLETIONS_PWSH),
        ] {
            assert!(
                script.contains("watch"),
                "{name}-Completion kennt das Unterkommando 'watch' nicht"
            );
        }
    }

    /// Der Hilfetext muss `--upgrade` erwähnen — sonst weiß niemand ohne
    /// Blick in den Quellcode, dass es den Selbst-Update gibt.
    #[test]
    fn cli_help_text_enthaelt_upgrade_option() {
        assert!(cli_help_text().contains("--upgrade"));
    }

    /// Alle vier Shell-Completion-Skripte müssen `--upgrade` kennen — sonst
    /// tippt niemand es per Tab fertig, obwohl die Option existiert.
    #[test]
    fn alle_completions_kennen_die_option_upgrade() {
        for (name, script) in [
            ("bash", COMPLETIONS_BASH),
            ("zsh", COMPLETIONS_ZSH),
            ("fish", COMPLETIONS_FISH),
            ("powershell", COMPLETIONS_PWSH),
        ] {
            assert!(
                script.contains("upgrade"),
                "{name}-Completion kennt 'upgrade' nicht"
            );
        }
    }

    // Das Verhalten OHNE Feature `work` (Exit 1, deutsche Meldung auf stderr)
    // lässt sich nicht als Unit-Test fassen: `run_work_cmd` ruft dort
    // `std::process::exit` auf, das den Testprozess selbst beenden würde.
    // Geprüft wird es deshalb per Hand (siehe Auftrag-Bericht) und über den
    // Release-Smoke-Test, der genau diesen Unterschied zwischen den
    // Feature-Sets abfragt.

    /// `agentkit_work::cli::dispatch` mit `["--help"]` muss Exit 0 liefern und
    /// den Work-Hilfetext (u. a. `work create`) ausgeben — über `dispatch_with_io`
    /// mit Puffern, damit nichts auf das echte stdout/stderr des Testprozesses
    /// geht (dieselbe Testbarkeits-Naht wie `agentkit_work`s eigene CLI-Tests).
    #[cfg(feature = "work")]
    #[test]
    fn agentkit_work_help_zeigt_unterkommandos() {
        use agentkit::testing::FakeLlm;

        let llm_builder =
            |_provider: &str, _demo: bool| -> Arc<dyn Llm> { Arc::new(FakeLlm::new(vec![])) };
        let deps = agentkit_work::cli::WorkCliDeps {
            llm: &llm_builder,
            approve: Arc::new(|_: &str| true),
            extra_tools: None,
            protect_paths: Vec::new(),
            allow_read: Vec::new(),
            cancel: new_cancel(),
            graph: None,
            build_executor: None,
            trace: None,
            system_extra: None,
            agent_setup: None,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = agentkit_work::cli::dispatch_with_io(&v(&["--help"]), deps, &mut out, &mut err);
        assert_eq!(code.code(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("agentkit work <unterkommando>"));
        assert!(text.contains("create"));
    }

    /// Wie `graph_tools_landen_neben_dem_swarm_tool` (`agentkit_app::lib`-Tests),
    /// aber für den Work-Agenten: er bekommt die LESENDEN Graph-Tools, NICHT
    /// `graph_remember`/`graph_promote` — auch wenn `--graph` (ohne
    /// `--graph-readonly`) einen schreibfähigen Zugriff geöffnet hat. Der
    /// einzige Schreibweg aus einem Work Item heraus ist `work_claim` (mit
    /// Provenance), nicht die generischen Graph-Tools (siehe
    /// `work_frontend_tools`-Doku).
    #[cfg(all(feature = "work", feature = "graph"))]
    #[test]
    fn work_agent_bekommt_nur_lesende_graph_tools() {
        use agentkit::testing::FakeLlm;
        use agentkit::{ExtraToolCtx, RunHandle};

        let graph_dir =
            std::env::temp_dir().join(format!("agentkit_bin_work_graph_{}", std::process::id()));
        std::fs::create_dir_all(&graph_dir).unwrap();

        let (extra_tools, graph_gateway) = work_frontend_tools(
            false,
            Some(graph_dir.to_str().unwrap()),
            false,
            None,
            &v(&["run", "demo"]),
        );
        let extra_tools = extra_tools.expect("swarm allein liefert schon Tools");
        assert!(
            graph_gateway.is_some(),
            "der Gateway-Adapter muss trotz nur-lesender Tools gebaut werden"
        );

        let run = RunHandle::new();
        let llm: Arc<dyn Llm> = Arc::new(FakeLlm::new(vec![]));
        let mcp = Arc::new(McpHub::empty());
        let ws_dir =
            std::env::temp_dir().join(format!("agentkit_bin_work_graph_ws_{}", std::process::id()));
        let coding = CodingTools::new(ws_dir.to_str().unwrap(), false);

        let mut reg = ToolRegistry::new();
        extra_tools(
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

        assert!(reg.has("graph_search"));
        assert!(reg.has("graph_neighbors"));
        assert!(reg.has("graph_evidence"));
        assert!(!reg.has("graph_remember"));
        assert!(!reg.has("graph_promote"));

        std::fs::remove_dir_all(&graph_dir).ok();
        std::fs::remove_dir_all(&ws_dir).ok();
    }

    /// Ein transienter Modellfehler in einem Schwarm-Mitglied oder Sub-Agenten
    /// darf den Lauf des Orchestrators nicht als API-Fehler abstempeln — sonst
    /// verwirft `classify_outcome` ein gültiges Ergebnis (Exit 2, leeres stdout).
    #[test]
    fn nur_orchestrator_fehler_zaehlen_als_harter_fehler() {
        let fehler = || EventData::Error {
            name: None,
            error: "HTTP 429 (Rate-Limit), Retry-After: 30s".to_string(),
        };

        // Orchestrator (leere source) -> harter Fehler.
        assert!(ist_harter_fehler(&AgentEvent::new(
            agentkit::ERROR,
            fehler()
        )));

        // Schwarm-Mitglied bzw. Sub-Agent -> NICHT.
        for quelle in ["architektur", "explorer:Suche die Tests"] {
            let ev = AgentEvent::with_meta(agentkit::ERROR, fehler(), 1, quelle.to_string());
            assert!(!ist_harter_fehler(&ev), "source '{quelle}' kippte den Lauf");
        }

        // Tool-Fehler sind weich, auch beim Orchestrator.
        let weich = AgentEvent::new(
            agentkit::ERROR,
            EventData::Error {
                name: Some("read_file".to_string()),
                error: "nicht gefunden".to_string(),
            },
        );
        assert!(!ist_harter_fehler(&weich));
    }
}
