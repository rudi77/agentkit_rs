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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::coding::{ApproveFn, CodingTools};
use agentkit::demo::demo_tools;
use agentkit::{
    build_coding_agent, build_task, classify_outcome, config_path, config_status,
    count_tokens_text, extract_json, init_user_config, load_dotenv, load_user_config, new_cancel,
    read_stdin_context, render_steps, strategy_from_str, Agent, AgentEvent, AgentRole,
    CodingAgentConfig, EventBus, EventData, ExitCode, Llm, McpHub, OutputFormat, Plan,
    RewindOutcome, ShortTermMemory, Skills, Strategy, ToolRegistry, DONE, JSON_SYSTEM,
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

    // `work` hat sein eigenes `-h`/`--help` (im Work-Argument-Scan, mit dem
    // Work-Hilfetext) — der globale Scan hier würde sonst `agentkit work
    // --help` abfangen, BEVOR der Verb-Dispatch weiter unten überhaupt läuft.
    let is_work_verb = argv.first().map(String::as_str) == Some("work");
    if !is_work_verb && (has("-h") || has("--help")) {
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

    // `agentkit work <unterkommando>` — die persistente Arbeits-Runtime
    // (agentkit-work, Feature `work`). Eigenes Verb, kein Auftrag; braucht die
    // geladene Umgebung wie `config` (Provider-Default), und muss VOR
    // `Args::parse` laufen, weil die Work-CLI eine eigene, unabhängige
    // Argument-Grammatik hat (siehe `agentkit_work::cli`) statt der von `Args`.
    if argv.first().map(String::as_str) == Some("work") {
        return run_work_cmd(&argv[1..]);
    }

    let mut args = Args::parse(&argv);

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
        return launch_tui(&args);
    }

    // Stop-Knopf: Ctrl-C bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).
    install_ctrlc_handler();

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
    } = build_agent(&args, pal, hub);
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
    };
    // `stdin_is_tty` kommt von oben: die Entscheidung „Skript oder Mensch"
    // gehört zum stdin-Kontrakt und wird nur EINMAL getroffen.
    repl(&mut agent, &mut renderer, &ctx, stdin_is_tty);
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
    /// Selbstverifikation vor der finalen Antwort (`--verify`).
    verify: bool,
    /// Timeout (Sekunden) für run_shell (`--shell-timeout`, Default 120).
    shell_timeout: u64,
    no_subagents: bool,
    /// Das `swarm`-Tool abschalten (`--no-swarm`).
    no_swarm: bool,
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
            verify: false,
            shell_timeout: 120,
            no_subagents: false,
            no_swarm: false,
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
                "--verify" => a.verify = true,
                "--shell-timeout" => a.shell_timeout = take().parse().unwrap_or(120),
                "--plan" => a.strategy = Strategy::Plan,
                "--plain" => a.strategy = Strategy::Plain,
                "--react" => a.strategy = Strategy::React,
                "--demo" => a.demo = true,
                "--no-subagents" => a.no_subagents = true,
                "--no-swarm" => a.no_swarm = true,
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
    if let Some(x) = b("no_swarm") {
        a.no_swarm = x;
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
    let vorlage = "# Projekt-Instruktionen für agentkit\n\n\
         Diese Datei wird bei jedem Start in diesem Verzeichnis an den System-Prompt\n\
         angehängt. Halte sie kurz — sie kostet in jedem Zug Kontext.\n\n\
         ## Was ist das hier?\n\n\
         (Ein bis zwei Sätze: Zweck des Projekts, Sprache, Aufbau.)\n\n\
         ## Bauen und Testen\n\n\
         (Die Befehle, die wirklich laufen — z. B. `cargo test`, `npm test`.)\n\n\
         ## Konventionen\n\n\
         (Was der Agent beachten muss: Stil, Sprache der Kommentare, verbotene Pfade.)\n";
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
/// Bewusst **sitzungsweit** und nicht in der Config: eine gespeicherte
/// Allowlist wäre eine stehende Erlaubnis, die beim nächsten Start niemand
/// mehr auf dem Schirm hat. Wer dauerhaft alles erlauben will, hat `-y`.
///
/// Geregelt wird nach dem **ersten Wort** des Befehls (`cargo`, `git`, `ls`).
/// Feiner wäre trügerisch: `cargo test` und `cargo publish` unterscheiden sich
/// nicht an der Länge des Präfixes, sondern in dem, was sie tun — dafür ist die
/// Einzelfrage der ehrlichere Weg.
#[derive(Default)]
struct Permissions {
    /// Erste Wörter, die in dieser Sitzung nicht mehr nachfragen.
    erlaubt: std::collections::BTreeSet<String>,
    /// `-y`: alles ohne Rückfrage.
    alles: bool,
}

impl Permissions {
    /// Das erste Wort eines Befehls — der Schlüssel der Regel.
    fn programm(command: &str) -> &str {
        command.split_whitespace().next().unwrap_or("")
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

/// `/permissions` — die Regeln dieser Sitzung zeigen bzw. zurücksetzen.
fn handle_permissions(rest: &[&str], perms: &Mutex<Permissions>, pal: Pal) {
    let mut p = perms.lock().unwrap();
    if matches!(rest.first(), Some(&"reset") | Some(&"zurücksetzen")) {
        p.erlaubt.clear();
        p.alles = false;
        println!(
            "{}✓ Freigabe-Regeln zurückgesetzt — es wird wieder jedes Mal gefragt.{}",
            pal.green, pal.reset
        );
        return;
    }
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
            println!("  {}{prog}{}", pal.cyan, pal.reset);
        }
    }
    println!(
        "{}/permissions reset setzt die Regeln zurück.{}",
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
fn confirm_shell(command: &str, pal: Pal, notify_on: bool, perms: &Mutex<Permissions>) -> bool {
    // Schon erlaubt (per `-y` oder „immer") -> gar nicht erst fragen.
    if !perms.lock().unwrap().fragt_nach(command) {
        return true;
    }
    // Eine wartende Freigabe blockiert den Agenten — wer nebenher etwas
    // anderes tut, soll das mitbekommen.
    notify("agentkit: Freigabe nötig", notify_on);
    let prog = Permissions::programm(command);
    eprintln!(
        "\n{}⚠  Shell-Befehl ausführen?{}\n  {}{command}{}",
        pal.yellow, pal.reset, pal.bold, pal.reset
    );
    eprint!(
        "{}  [j]a / [N]ein / [i]mmer ({prog}) › {}",
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
        _ => false,
    }
}

// --------------------------------------------------------------------- Setup

/// Wählt den LLM und gibt `(llm, label)` zurück.
/// Bildet `--model NAME` auf die Umgebungsvariable ab, aus der `build_llm` das
/// Modell ohnehin liest — je nach Anbieter `AZURE_OPENAI_DEPLOYMENT` oder
/// `OPENAI_MODEL`. Kein zweites Modell-Konzept: genau so verfährt schon
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
        _ => {
            std::env::set_var("AZURE_OPENAI_DEPLOYMENT", name);
            std::env::set_var("OPENAI_MODEL", name);
        }
    }
}

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
    apply_model_override(args);
    let (llm, label) = build_llm(&args.provider, args.demo);
    eprintln!("{}» Modell: {label}{}", pal.gray, pal.reset);
    // Sichtbar machen, dass der System-Prompt aus dem Projekt ergänzt wurde —
    // eine still wirkende Datei wäre ein Rätsel bei unerwartetem Verhalten.
    if agentkit::load_project_instructions(&args.workspace).is_some() {
        eprintln!(
            "{}» Projekt-Instruktionen geladen: {}{}",
            pal.gray,
            agentkit::PROJECT_INSTRUCTIONS,
            pal.reset
        );
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
            perms: Arc::new(Mutex::new(Permissions {
                alles: args.yes,
                ..Default::default()
            })),
            coding: None,
        };
    }

    // Freigabe-Policy steckt im Callback: bei `--yes` immer erlauben, sonst nachfragen.
    let yes = args.yes;
    let notify_on = args.notify;
    // Die Regeln leben in EINEM geteilten Objekt: der Approve-Callback fragt
    // sie, `/permissions` zeigt und ändert sie.
    let perms = Arc::new(Mutex::new(Permissions {
        alles: yes,
        ..Default::default()
    }));
    let perms_cb = perms.clone();
    let approve: ApproveFn =
        Arc::new(move |cmd: &str| confirm_shell(cmd, pal, notify_on, &perms_cb));

    // Frontend-eigene Fähigkeiten: das `swarm`-Tool aus agentkit-swarm und die
    // Graph-Tools aus agentkit-graph, plus die Prompt-Zusätze, die dem Modell
    // erklären, wann sie sich lohnen.
    let extras = frontend_tools(args);
    let system = agentkit_app::system_with_extras(
        args.system.as_deref(),
        !args.no_swarm,
        graph_active(args),
    );
    let cfg = CodingAgentConfig {
        workspace: &args.workspace,
        strategy: args.strategy,
        max_steps: args.max_steps,
        skills: args.skills.as_deref(),
        agents: args.agents.as_deref(),
        memory: args.memory.as_deref(),
        subagents: !args.no_subagents,
        system: system.as_deref(),
        verify: args.verify,
        shell_timeout: args.shell_timeout,
        dry_run: args.dry_run,
        extra_tools: extras.build(),
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
    args.session
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()))
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
        // Resume: gespeicherten Verlauf laden (auch je JSON-Retry — derselbe Stand).
        if let Some(path) = args.session.as_deref() {
            load_session(&mut agent, path);
        }
        // Die Sperre selbst setzt `build_coding_agent` (für Haupt-Agent, Sub-Agenten
        // und Schwarm-Mitglieder gleichermaßen) — hier bleibt nur die Meldung.
        if args.dry_run {
            eprintln!("[INFO] Dry-Run aktiv — zerstörerische Schreibvorgänge werden blockiert.");
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
            // One-shot bleibt roh: der Unix-Filter-Kontrakt sagt zu, dass
            // stdout die unverfälschte Antwort trägt.
            md: None,
        };
        let (agent, final_, hard_error) = run_task(agent, &task, &mut renderer);
        // Verlauf sichern, BEVOR der Exit-Code fällt — auch ein Fehl-Lauf ist Verlauf.
        if let Some(path) = args.session.as_deref() {
            save_session(&agent, path);
        }

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
        if !self.aktiv() {
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
        if self.sichtbar {
            eprint!("\r{}\r", " ".repeat(20));
            let _ = std::io::stderr().flush();
            self.sichtbar = false;
        }
    }
}

fn run_task(agent: Agent, task: &str, renderer: &mut Renderer) -> (Agent, String, bool) {
    let bus = EventBus::new();
    let q = bus.subscribe();
    let cancel = new_cancel();
    *CURRENT_CANCEL.lock().unwrap() = Some(cancel.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    let task_owned = task.to_string();
    let cancel_worker = cancel.clone();
    let mut agent = agent;
    std::thread::spawn(move || {
        let final_ = agent.run_on_bus(&task_owned, &bus, -1, Some(&cancel_worker), "");
        let _ = tx.send((agent, final_));
    });

    // Nur das Root-DONE (leere `source`) beendet die Anzeige; Sub-Agent-DONEs nicht.
    //
    // `recv_timeout` statt `recv`, damit in der Wartezeit ein Spinner laufen
    // kann — bewusst OHNE zweiten Thread: der würde sich mit der Ausgabe des
    // Renderers um dieselbe Zeile streiten. Der Spinner läuft nur bis zum
    // ersten Ereignis; danach zeigt der Trace selbst den Fortschritt.
    let mut hard_error = false;
    let mut spinner = Spinner::new(renderer.pal);
    loop {
        let ev = match q.recv_timeout(Spinner::INTERVAL) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                spinner.tick();
                continue;
            }
            Err(_) => break,
        };
        spinner.clear();
        if ev.etype == DONE && ev.source.is_empty() {
            break;
        }
        if let EventData::Error { name: None, .. } = &ev.data {
            hard_error = true;
        }
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
    *CURRENT_CANCEL.lock().unwrap() = None;
    // Zähler zurücksetzen: ein einzelnes Ctrl-C während des Laufs soll nach
    // Lauf-Ende nicht als "erstes von zwei" weiterzählen und den nächsten
    // Ctrl-C am Prompt sofort beenden lassen.
    INT_COUNT.store(0, Ordering::SeqCst);
    (agent, final_, hard_error)
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
    let (back, _final, _hard) = run_task(taken, user, renderer);
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
    println!(
        "{}  ↳ Kontext {} Tokens (+{}) · {dauer} s{}",
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
        "context" | "ctx" => handle_context(agent, pal),
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
fn handle_context(agent: &Agent, pal: Pal) {
    /// Breite des Belegungsbalkens in Zeichen.
    const BAR: usize = 40;

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
    ("/init", "Projekt-Instruktionen (AGENTKIT.md) anlegen"),
    ("/model", "das aktive Modell zeigen"),
    (
        "/permissions",
        "Freigabe-Regeln zeigen; /permissions reset setzt sie zurück",
    ),
    ("/context", "Kontext-Belegung zeigen (auch /ctx)"),
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
        "MCP-Server auflisten / umschalten (/mcp on|off <name>)",
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
            system: agentkit_app::system_with_extras(
                args.system.as_deref(),
                !args.no_swarm,
                graph_active(args),
            ),
            ctx: args.ctx.clone(),
            ctx_budget: args.ctx_budget,
            ctx_policy: args.ctx_policy.clone(),
            ctx_compaction_model: args.ctx_compaction_model.clone(),
            extra_tools: frontend_tools(args).build(),
            session: args.session.clone(),
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
    let (work_argv, no_swarm, graph_dir, graph_readonly) =
        extract_frontend_flags(&normalize_args(rest));
    let (extra_tools, graph_gateway) =
        work_frontend_tools(no_swarm, graph_dir.as_deref(), graph_readonly, &work_argv);

    // Farben/Freigabe wie im übrigen CLI: `confirm_shell` fragt interaktiv
    // nach, `-y`/`--yes` in der Work-Argumentliste überschreibt das lokal in
    // `agentkit_work::cli::cmd_run` mit "immer erlauben" (siehe dort) — hier
    // wird nur der Rückfrage-Callback für den Fall OHNE `-y` gebaut.
    let color =
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() && enable_vt();
    let pal = if color { Pal::color() } else { Pal::plain() };
    let perms = Arc::new(Mutex::new(Permissions::default()));
    let approve: ApproveFn = Arc::new(move |cmd: &str| confirm_shell(cmd, pal, false, &perms));

    let llm_builder = |provider: &str, demo: bool| build_llm(provider, demo).0;
    let deps = agentkit_work::cli::WorkCliDeps {
        llm: &llm_builder,
        approve,
        extra_tools,
        cancel,
        graph: graph_gateway,
    };
    let code = agentkit_work::cli::dispatch(&work_argv, deps);
    std::process::exit(code.code());
}

/// Zieht `--no-swarm`, `--graph DIR` und `--graph-readonly` aus `argv` heraus
/// und gibt den Rest (für `agentkit_work::cli::dispatch`) plus die drei Werte
/// zurück. `argv` muss bereits durch [`normalize_args`] gelaufen sein
/// (`--graph=DIR` -> zwei Tokens), sonst würde `--graph=DIR` nicht erkannt.
#[cfg(feature = "work")]
fn extract_frontend_flags(argv: &[String]) -> (Vec<String>, bool, Option<String>, bool) {
    let mut rest = Vec::with_capacity(argv.len());
    let mut no_swarm = false;
    let mut graph_dir = None;
    let mut graph_readonly = false;
    let mut it = argv.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-swarm" => no_swarm = true,
            "--graph" => graph_dir = it.next(),
            "--graph-readonly" => graph_readonly = true,
            _ => rest.push(a),
        }
    }
    (rest, no_swarm, graph_dir, graph_readonly)
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
        match agentkit_app::open_graph(dir, ".", &work_graph_run_id(work_argv), graph_readonly) {
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
fn work_graph_run_id(work_argv: &[String]) -> String {
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
--max-steps --plan --plain --react --no-subagents --no-swarm -y --yes --steps --no-color -p --print \
--tui --repl --format --dry-run --verify --shell-timeout --max-context --json-retries \
--ctx --ctx-budget --ctx-policy --ctx-compaction-model --graph --graph-readonly \
--mcp-config --mcp --no-mcp \
--system --system-file --profile -h --help -V --version"
    # Erstes Wort: auch die Verben `completions`/`read-pdf`/`config`/`work` anbieten.
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "completions read-pdf config work $opts" -- "$cur") )
        return 0
    fi
    case "$prev" in
        completions) COMPREPLY=( $(compgen -W "bash zsh fish powershell" -- "$cur") ); return 0;;
        read-pdf) COMPREPLY=( $(compgen -f -- "$cur") ); return 0;;
        config) COMPREPLY=( $(compgen -W "show path init" -- "$cur") ); return 0;;
        work) COMPREPLY=( $(compgen -W "create list run resume status items events budget pause retry approve reject" -- "$cur") ); return 0;;
        -s|--strategy) COMPREPLY=( $(compgen -W "react plan plain" -- "$cur") ); return 0;;
        --provider) COMPREPLY=( $(compgen -W "auto azure openai demo" -- "$cur") ); return 0;;
        --format) COMPREPLY=( $(compgen -W "text json" -- "$cur") ); return 0;;
        -w|--workspace|--skills|--agents|--ctx|--graph) COMPREPLY=( $(compgen -d -- "$cur") ); return 0;;
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
        _values 'work-unterkommando' create list run resume status items events budget pause retry approve reject
        return
    fi
    opts=(
        '1:verb:(completions read-pdf config work)'
        '-w[Arbeitsverzeichnis]:dir:_files -/'
        '--workspace[Arbeitsverzeichnis]:dir:_files -/'
        '-s[Strategie]:strategy:(react plan plain)'
        '--strategy[Strategie]:strategy:(react plan plain)'
        '--skills[Skills-Verzeichnis]:dir:_files -/'
        '--agents[Custom-Rollen-Verzeichnis]:dir:_files -/'
        '--memory[Langzeitgedächtnis (JSONL)]:file:_files'
        '--session[Session-Datei (Resume)]:file:_files'
        '--model[Modell überschreiben]:name:'
        '--notify[Glocke/Desktop-Meldung bei langen Läufen]'
        '(-c --continue)'{-c,--continue}'[jüngste Sitzung dieses Projekts fortsetzen]'
        '--resume[Sitzung aus der Liste auswählen]'
        '--provider[LLM-Anbieter]:provider:(auto azure openai demo)'
        '--demo[Demo-Modus erzwingen]'
        '--max-steps[Max. Loop-Schritte]:n:'
        '--plan[Plan-Strategie]'
        '--plain[Plain-Strategie]'
        '--react[ReAct-Strategie]'
        '--no-subagents[task-Tool deaktivieren]'
        '--no-swarm[swarm-Tool deaktivieren]'
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
        '--verify[Vor dem Abschluss selbst verifizieren]'
        '--shell-timeout[Timeout für run_shell (Sekunden)]:n:'
        '--ctx[Kontext-Management-Verzeichnis]:dir:_files -/'
        '--ctx-budget[Kontext-Budget (Tokens)]:n:'
        '--ctx-policy[Kontext-Policy (JSON)]:file:_files'
        '--ctx-compaction-model[Modell für die Verdichtung]:name:'
        '--graph[Wissensgraph-Verzeichnis]:dir:_files -/'
        '--graph-readonly[Graph nur lesen]'
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
complete -c agentkit -n '__fish_use_subcommand' -a work -d 'Arbeits-Runtime (Feature `work`)'
complete -c agentkit -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish powershell'
complete -c agentkit -n '__fish_seen_subcommand_from config' -a 'show path init'
complete -c agentkit -n '__fish_seen_subcommand_from work' -a 'create list run resume status items events budget pause retry approve reject'
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
complete -c agentkit -l provider -x -a 'auto azure openai demo' -d 'LLM-Anbieter'
complete -c agentkit -l demo -d 'Demo-Modus erzwingen'
complete -c agentkit -l max-steps -x -d 'Max. Loop-Schritte'
complete -c agentkit -l plan -d 'Plan-Strategie'
complete -c agentkit -l plain -d 'Plain-Strategie'
complete -c agentkit -l react -d 'ReAct-Strategie'
complete -c agentkit -l no-subagents -d 'task-Tool deaktivieren'
complete -c agentkit -l no-swarm -d 'swarm-Tool deaktivieren'
complete -c agentkit -s y -l yes -d 'Shell ohne Rückfrage'
complete -c agentkit -l steps -d 'Schritt-Grenzen anzeigen'
complete -c agentkit -l no-color -d 'Farbe aus'
complete -c agentkit -s p -l print -d 'Nur finale Antwort'
complete -c agentkit -l tui -d 'Terminal-UI'
complete -c agentkit -l repl -d 'Interaktive Session'
complete -c agentkit -l format -x -a 'text json' -d 'Ausgabeformat'
complete -c agentkit -l dry-run -d 'Schreibvorgänge blockieren'
complete -c agentkit -l verify -d 'Vor dem Abschluss selbst verifizieren'
complete -c agentkit -l shell-timeout -x -d 'Timeout für run_shell (Sekunden)'
complete -c agentkit -l ctx -r -d 'Kontext-Management-Verzeichnis'
complete -c agentkit -l ctx-budget -x -d 'Kontext-Budget (Tokens)'
complete -c agentkit -l ctx-policy -r -d 'Kontext-Policy (JSON)'
complete -c agentkit -l ctx-compaction-model -x -d 'Modell für die Verdichtung'
complete -c agentkit -l graph -r -d 'Wissensgraph-Verzeichnis'
complete -c agentkit -l graph-readonly -d 'Graph nur lesen'
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
        'completions','read-pdf','config','work','-w','--workspace','-s','--strategy','--skills','--agents','--memory','--session','-c','--continue','--resume','--model','--notify',
        '--provider','--demo','--max-steps','--plan','--plain','--react','--no-subagents','--no-swarm',
        '-y','--yes','--steps','--no-color','-p','--print','--tui','--repl','--format',
        '--dry-run','--verify','--shell-timeout','--max-context','--json-retries',
        '--ctx','--ctx-budget','--ctx-policy','--ctx-compaction-model','--graph','--graph-readonly',
        '--mcp-config','--mcp','--no-mcp',
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
        'work'        { @('create','list','run','resume','status','items','events','budget','pause','retry','approve','reject') }
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
                                    von create|list|run|resume|status|items|events|budget|pause|\n  \
                                    retry|approve|reject. Details: `agentkit work --help`\n\n\
         UNIX-PIPE:\n  \
           stdin  = Kontext (per Pipe), wird an die Query angehängt\n  \
           stdout = nur das finale Resultat (bei Pipe/--format json/--print)\n  \
           stderr = Status, Tool-Spur, ReAct-Gedanken, Fehler\n  \
           Exit:  0 Erfolg · 1 Laufzeit · 2 API/Netz · 3 Kontext/Prompt · 4 Format\n\n\
         OPTIONEN:\n  \
           -w, --workspace DIR   Sandbox-/Arbeitsverzeichnis (Default: .)\n  \
           -s, --strategy S      react | plan | plain (Default: react)\n  \
           --react/--plan/--plain  Kurzform für -s\n  \
           --skills DIR          Skills-Verzeichnis aktivieren (SKILL.md-Ordner)\n  \
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
           --provider P          auto | azure | openai | demo (Default: auto)\n  \
           --demo                Demo-Modus erzwingen (netzfrei)\n  \
           --max-steps N         Max. Loop-Schritte (Default: 160)\n  \
           --verify              vor der finalen Antwort einen ausgeführten Check verlangen\n  \
           --shell-timeout N     Timeout je run_shell-Befehl in Sekunden (Default: 120)\n  \
           --no-subagents        das 'task'-Tool deaktivieren\n  \
           --no-swarm            das 'swarm'-Tool (dynamische Agenten-Schwärme) deaktivieren\n  \
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
            cancel: new_cancel(),
            graph: None,
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
}
