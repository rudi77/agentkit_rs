//! agentkit TUI — ein interaktives Terminal-UI für den Agenten.
//!
//! Liegt als Library-Modul vor (Feature `tui`), damit sowohl das `tui`-Binary als
//! auch die Haupt-Executable `agentkit` es starten können. Der Agent läuft in einem
//! Worker-Thread und publiziert [`AgentEvent`]s auf einen [`EventBus`]; das UI ist
//! genau ein weiterer Consumer dieses Stroms und rendert die Events live (Schritte,
//! Tool-Calls, gestreamte Tokens). `Esc` setzt den kooperativen Stop-Knopf.
//!
//! Mit echtem LLM ist es der volle Coding-Agent — Sandbox-Tools (inkl. glob/grep),
//! Skills, Plan und das `task`-Tool für Sub-Agenten. Da `ratatui` das Terminal belegt,
//! läuft die `run_shell`-Freigabe nicht über stdin, sondern über einen In-TUI-Dialog.
//! Mit **Ctrl-Tab** (oder Shift-Tab) schaltet man zwischen *Nachfragen* und
//! *Auto-Freigabe* um — wie der Permission-Mode in der Claude-Code-CLI.
//!
//! Bewusst schlank gehalten: nur `ratatui` als zusätzliche Abhängigkeit (crossterm
//! kommt re-exportiert via `ratatui::crossterm`). Kein async-Runtime.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::app::{fmt_count, fmt_pct, fmt_tokens};
use crate::coding::ApproveFn;
use crate::demo::{build_llm, demo_tools};
use crate::events::{AgentEvent, EventData};
use crate::memory::one_line;
use crate::{
    build_coding_agent, context_report, new_cancel, render_steps, Agent, Cancel, CodingAgentConfig,
    ContextReport, EventBus, McpHub, Strategy, ToolRegistry,
};

/// Konfiguration fürs TUI (vom CLI bzw. `tui`-Binary befüllt).
pub struct TuiConfig {
    pub strategy: Strategy,
    pub force_demo: bool,
    pub workspace: String,
    pub skills: Option<String>,
    pub agents: Option<String>,
    pub memory: Option<String>,
    pub subagents: bool,
    pub max_steps: usize,
    /// Anfangsmodus der Shell-Freigabe: `true` = nachfragen, `false` = auto.
    pub ask_approval: bool,
    /// Pfad zur `.mcp.json` (sonst Auto-Discovery im Workspace/CWD).
    pub mcp_config: Option<String>,
    /// Allowlist initial aktiver MCP-Server (leer = alle nicht-`disabled`).
    pub mcp_enable: Vec<String>,
    /// MCP komplett aus.
    pub no_mcp: bool,
    /// Agenten-spezifischer Zusatz-System-Prompt (aus `--system`/`--system-file`/`--profile`).
    pub system: Option<String>,
    /// ctxman-Zustandsverzeichnis (`--ctx DIR`, nur mit Feature `ctxman`): aktiviert
    /// das volle Kontext-Management (Watermark-GC, Externalisierung, Snapshot-Resume);
    /// `/context` zeigt dann die echte Segment-Statistik statt der Zeichen/4-Heuristik.
    pub ctx: Option<String>,
    /// Kontext-Budget B in Tokens für `--ctx` (Default: 100 000).
    pub ctx_budget: u32,
    /// Partielles Policy-Overlay als JSON-Datei (`--ctx-policy FILE`).
    pub ctx_policy: Option<String>,
    /// Separates Compaction-LLM (`--ctx-compaction-model NAME`).
    pub ctx_compaction_model: Option<String>,
    /// Zusätzliche Tools des Frontends (siehe [`crate::ExtraTools`]) — im
    /// Demo-Modus wirkungslos, weil dort gar kein Coding-Agent gebaut wird.
    pub extra_tools: Option<crate::ExtraTools>,
    /// Sitzungsdatei (`--session`): Verlauf wird daraus geladen und nach jedem
    /// Zug dorthin geschrieben — bis hierher verlor das TUI beim Schließen alles.
    pub session: Option<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            strategy: Strategy::React,
            force_demo: false,
            workspace: ".".to_string(),
            skills: None,
            agents: None,
            memory: None,
            subagents: true,
            max_steps: 160,
            ask_approval: true,
            mcp_config: None,
            mcp_enable: Vec::new(),
            no_mcp: false,
            system: None,
            ctx: None,
            ctx_budget: 100_000,
            ctx_policy: None,
            ctx_compaction_model: None,
            extra_tools: None,
            session: None,
        }
    }
}

/// Bilder des Warte-Spinners (Braille-Punkte, eine Zelle breit).
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Wie oft der Spinner weiterrückt. Ein Vielfaches des 50-ms-Polls, damit die
/// Animation nicht flimmert und die Neuzeichnungen im Rahmen bleiben.
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

/// Eine wartende Shell-Freigabe: der Befehl + der Antwortkanal zum Worker.
type ApprovalReq = (String, Sender<bool>);

/// Startet das TUI: baut LLM + Agent, initialisiert das Terminal und rendert die
/// App, bis der Nutzer beendet. Stellt das Terminal in jedem Fall wieder her.
pub fn run(cfg: TuiConfig) -> std::io::Result<()> {
    // true = nachfragen, false = auto-freigeben (per Ctrl-Tab umschaltbar).
    let approval_mode = Arc::new(AtomicBool::new(cfg.ask_approval));
    let (req_tx, req_rx) = mpsc::channel::<ApprovalReq>();

    // MCP interaktiv: ALLE Server vorverbinden (connect_all), damit das F2-Panel sie
    // ohne Reconnect zu- und abschalten kann.
    let hub = build_mcp_hub(&cfg);
    let (mut agent, model_label, mcp_base, mut notes) =
        build_agent(&cfg, approval_mode.clone(), req_tx, hub.clone());
    if let Some(pfad) = &cfg.session {
        notes.push(load_session(&mut agent, pfad));
    }

    // Vor ratatui::init() eingehängt: dessen Panic-Hook läuft zuerst und ruft
    // danach diesen — so laufen alle Teardown-Pfade (regulär, Panic, SIGINT)
    // über restore_terminal() und hinterlassen keine ~[200~-Marker in der Shell.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));
    let terminal = ratatui::init();
    // Bracketed Paste: eingefügter Text kommt als EIN Paste-Event an statt als
    // einzelne Tastendrücke — eingebettete Zeilenumbrüche würden sonst wie Enter
    // wirken und jede Zeile sofort abschicken. Fehler ignorieren (nicht jedes
    // Terminal unterstützt den Modus).
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut app = App::new(agent, model_label, approval_mode, req_rx, hub, mcp_base);
    app.session = cfg.session.clone();
    for (msg, color) in notes {
        app.push(note_line(&msg, color));
    }
    let result = app.run(terminal);
    restore_terminal();
    result
}

/// Stellt das Terminal vollständig wieder her: Bracketed Paste aus, Raw-Mode und
/// Alternate-Screen zurück. Öffentlich, damit ein Signal-Handler (externes SIGINT
/// im `--tui`-Modus) das Terminal nicht kaputt hinterlässt.
pub fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

/// Baut den MCP-Hub aus der TUI-Config (leer bei `--no-mcp`/Demo oder fehlender Config).
fn build_mcp_hub(cfg: &TuiConfig) -> Arc<McpHub> {
    // MCP ist unabhängig vom LLM — auch im Demo-Modus nutzbar; nur --no-mcp schaltet ab.
    if cfg.no_mcp {
        return Arc::new(McpHub::empty());
    }
    let hub = McpHub::from_config(
        &cfg.workspace,
        cfg.mcp_config.as_deref(),
        &cfg.mcp_enable,
        true,
    )
    .unwrap_or_else(|_| McpHub::empty());
    Arc::new(hub)
}

/// Lädt eine `--session`-Datei in den frisch gebauten Agenten und gibt die
/// Hinweis-Zeile für den Verlauf zurück.
///
/// `adopt_history` statt direkter Zuweisung: es setzt den Spiegel UND (bei
/// frischem `--ctx`) den verwalteten Kontext — sonst begänne das Modell trotz
/// geladener Datei bei null.
fn load_session(agent: &mut Agent, pfad: &str) -> (String, Color) {
    let mut geladen = match crate::ShortTermMemory::load(pfad) {
        Ok(m) => m,
        Err(e) => return (format!("Sitzung nicht ladbar: {e}"), Color::Red),
    };
    if geladen.messages.is_empty() {
        return (
            format!("Sitzung leer, beginne neu: {pfad}"),
            Color::DarkGray,
        );
    }
    // Eine Datei ohne System-Prompt (etwa aus `/export --json`) bekommt den
    // frischen davorgesetzt — sonst liefe der Agent ganz ohne Instruktionen.
    if !geladen.messages.iter().any(|m| m["role"] == "system") {
        if let Some(sys) = agent
            .memory
            .messages
            .iter()
            .find(|m| m["role"] == "system")
            .cloned()
        {
            geladen.messages.insert(0, sys);
        }
    }
    let n = geladen.messages.len();
    agent.adopt_history(geladen);
    (
        format!("Sitzung geladen: {pfad} ({n} Nachrichten)"),
        Color::DarkGray,
    )
}

/// Baut den Agenten: voller Coding-Agent (echter LLM) oder schlanker Demo-Agent.
/// Gibt zusätzlich die MCP-freie Basis-Registry zurück (Grundlage fürs Umschalten)
/// sowie Hinweis-Zeilen für den Verlauf (z. B. ctxman aktiv / fehlgeschlagen).
fn build_agent(
    cfg: &TuiConfig,
    approval_mode: Arc<AtomicBool>,
    req_tx: Sender<ApprovalReq>,
    hub: Arc<McpHub>,
) -> (Agent, String, ToolRegistry, Vec<(String, Color)>) {
    let (llm, label) = build_llm(cfg.force_demo);

    // Demo-Modus: kleiner, netzfreier Werkzeugkasten — MCP-Tools werden dennoch
    // eingeklinkt, und auch ctxman ist nutzbar (arbeitet rein lokal).
    if label.starts_with("demo") {
        let mut agent = Agent::builder(llm.clone())
            .tools(demo_tools())
            .strategy(cfg.strategy)
            .max_steps(cfg.max_steps)
            .build();
        let mut mcp_base = hub.apply(&mut agent);
        let notes = attach_ctx_notes(&mut agent, &mut mcp_base, cfg, llm, &label);
        return (agent, label, mcp_base, notes);
    }

    // Approval-Callback: läuft im Worker-Thread. Bei Auto-Modus sofort `true`; sonst
    // eine Freigabe-Anfrage ans UI schicken und auf die Antwort blockieren.
    let approve: ApproveFn = {
        let mode = approval_mode;
        Arc::new(move |cmd: &str| {
            if !mode.load(Ordering::Relaxed) {
                return true; // Auto-Freigabe
            }
            let (resp_tx, resp_rx) = mpsc::channel();
            if req_tx.send((cmd.to_string(), resp_tx)).is_err() {
                return false;
            }
            resp_rx.recv().unwrap_or(false)
        })
    };

    // Human-in-the-Loop braucht kein Sonderwerkzeug: Der Agent beendet seinen Zug mit einer
    // Rückfrage, die nächste Eingabe des Menschen beantwortet sie (Gesprächsverlauf bleibt).

    let acfg = CodingAgentConfig {
        workspace: &cfg.workspace,
        strategy: cfg.strategy,
        max_steps: cfg.max_steps,
        skills: cfg.skills.as_deref(),
        agents: cfg.agents.as_deref(),
        memory: cfg.memory.as_deref(),
        subagents: cfg.subagents,
        system: cfg.system.as_deref(),
        // Interaktiv unerwünscht: Der Mensch sieht die Änderungen und fragt selbst nach.
        verify: false,
        shell_timeout: 120,
        // Interaktiv: kein --dry-run im TUI.
        dry_run: false,
        extra_tools: cfg.extra_tools.clone(),
        helper_ctx_budget: None,
    };
    let (mut agent, _plan, _skills, _roles, mut mcp_base, _coding) =
        build_coding_agent(llm.clone(), &acfg, approve, hub);
    let notes = attach_ctx_notes(&mut agent, &mut mcp_base, cfg, llm, &label);
    (agent, label, mcp_base, notes)
}

/// Klinkt ctxman ein, wenn `--ctx` gesetzt ist (gemeinsamer Pfad mit dem CLI via
/// [`crate::app::attach_managed_context`]). Die Rückgabe sind Hinweis-Zeilen für
/// den Verlauf — das TUI hat kein stderr für Startmeldungen.
#[cfg(feature = "ctxman")]
fn attach_ctx_notes(
    agent: &mut Agent,
    mcp_base: &mut ToolRegistry,
    cfg: &TuiConfig,
    llm: Arc<dyn crate::Llm>,
    model_label: &str,
) -> Vec<(String, Color)> {
    let Some(dir) = cfg.ctx.as_deref() else {
        return Vec::new();
    };
    let mut notes: Vec<(String, Color)> = Vec::new();
    let mut mc = crate::ManagedContextConfig::new(dir);
    mc.budget_tokens = cfg.ctx_budget;
    // Fakten-Promotion in die --memory-Datei lenken, damit `recall` sie später findet.
    if let Some(mem) = cfg.memory.as_deref() {
        mc.facts_path = Some(std::path::PathBuf::from(mem));
    }
    // Policy-Overlay: eine kaputte Datei aktiviert ctxman NICHT halbherzig mit
    // Default-Policy — der Nutzer hat explizit eine andere verlangt.
    if let Some(path) = cfg.ctx_policy.as_deref() {
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
        {
            Ok(overlay) => mc.policy_overlay = Some(overlay),
            Err(e) => {
                notes.push((
                    format!("--ctx-policy {path}: {e} — ctxman NICHT aktiviert."),
                    Color::Red,
                ));
                return notes;
            }
        }
    }
    // Separates Compaction-LLM; scheitert der Bau, übernimmt sichtbar das Agent-LLM.
    match cfg.ctx_compaction_model.as_deref() {
        Some(name) => match crate::app::compaction_llm_from_env(name) {
            Ok(cllm) => {
                mc.compaction_llm = Some(cllm);
                mc.compaction_model_label = Some(name.to_string());
            }
            Err(e) => notes.push((
                format!(
                    "--ctx-compaction-model {name}: {e} — Compaction läuft über das Agent-LLM."
                ),
                Color::Yellow,
            )),
        },
        None => mc.compaction_model_label = Some(model_label.to_string()),
    }
    match crate::app::attach_managed_context(agent, mcp_base, mc, llm) {
        Ok(info) => {
            notes.push((
                format!(
                    "ctxman: Kontext-Management aktiv ({dir}, Budget {}, Tokenizer {}) — \
                     /context zeigt die Segment-Statistik.",
                    cfg.ctx_budget, info.tokenizer
                ),
                Color::Magenta,
            ));
            if info.resumed {
                notes.push((
                    "ctxman: Session aus Snapshot fortgesetzt — die eingefrorene Policy gilt; \
                     --ctx-policy/--ctx-budget wirken erst auf eine neue Session."
                        .to_string(),
                    Color::Yellow,
                ));
            }
        }
        Err(e) => notes.push((format!("ctxman nicht aktiviert: {e}"), Color::Red)),
    }
    notes
}

/// Ohne Feature `ctxman` ist `--ctx` ein sichtbarer No-op (Hinweis statt stillem Ignorieren).
#[cfg(not(feature = "ctxman"))]
fn attach_ctx_notes(
    _agent: &mut Agent,
    _mcp_base: &mut ToolRegistry,
    cfg: &TuiConfig,
    _llm: Arc<dyn crate::Llm>,
    _model_label: &str,
) -> Vec<(String, Color)> {
    if cfg.ctx.is_some() {
        vec![(
            "--ctx ignoriert — Binary ohne Feature `ctxman` gebaut \
             (cargo build --features \"tui ctxman\")."
                .to_string(),
            Color::Yellow,
        )]
    } else {
        Vec::new()
    }
}

// ------------------------------------------------------------------------- App/UI

/// Laufende Hintergrund-Aufgabe: der Agent in einem Worker-Thread. Der `done`-Kanal
/// gibt den Agenten nach Abschluss zurück (Memory bleibt für den nächsten Turn erhalten).
struct Running {
    done: Receiver<Agent>,
    cancel: Cancel,
    /// Beginn des Laufs — für die verstrichene Zeit in der Titelzeile.
    started: std::time::Instant,
    /// Zuletzt gemeldeter Loop-Schritt (aus dem STEP-Event).
    step: usize,
}

struct App {
    /// `None`, solange der Agent in einem Worker-Thread arbeitet.
    agent: Option<Agent>,
    model_label: String,
    bus: EventBus,
    events: Receiver<AgentEvent>,
    running: Option<Running>,

    /// Umschaltbarer Freigabe-Modus (true = nachfragen) + Kanal für Anfragen.
    approval_mode: Arc<AtomicBool>,
    approval_rx: Receiver<ApprovalReq>,
    /// Aktuell offene Freigabe (Befehl + Antwortkanal zum Worker).
    pending: Option<ApprovalReq>,

    /// Eingabepuffer mit Cursor (mehrzeilig: `\n` trennt Zeilen; Alt/Shift-Enter
    /// fügt eine ein; die Anzeige bricht automatisch an der Feldbreite um).
    input: InputBuffer,
    /// Während ein Auftrag läuft eingetippte Aufträge (Type-ahead). Sie werden
    /// der Reihe nach abgearbeitet, sobald der Agent zurück ist — man kann
    /// also weiterdenken, statt auf den Prompt zu warten.
    queue: std::collections::VecDeque<String>,
    /// Zählt die Spinner-Bilder hoch (nur während ein Auftrag läuft).
    tick: usize,
    /// Sitzungsdatei (`--session`): nach jedem Zug gesichert.
    session: Option<String>,
    lines: Vec<Line<'static>>,
    /// Startindex des laufenden Assistant-Blocks in `lines` und der bislang
    /// gestreamte Rohtext. Der ganze Block wird bei jedem Token neu als Markdown
    /// gerendert — nur so lassen sich mehrzeilige Konstrukte (Tabellen, Code-Fences
    /// inkl. JSON-Highlighting) korrekt formatieren.
    assistant_start: Option<usize>,
    assistant_buf: String,

    /// Scroll-Offset in gerenderten Zeilen; `follow` heftet ans Ende (Auto-Scroll).
    scroll: usize,
    follow: bool,
    should_quit: bool,

    /// Geteilter MCP-Hub (auch fürs `task`-Tool) + MCP-freie Basis-Registry des
    /// Haupt-Agenten. `mcp_panel` blendet das Server-Panel ein, `mcp_sel` ist die
    /// Auswahl darin; `mcp_dirty` merkt einen Toggle, der den (gerade laufenden)
    /// Haupt-Agenten noch nicht neu verdrahtet hat.
    hub: Arc<McpHub>,
    mcp_base: ToolRegistry,
    mcp_panel: bool,
    mcp_sel: usize,
    mcp_dirty: bool,
}

impl App {
    fn new(
        agent: Agent,
        model_label: String,
        approval_mode: Arc<AtomicBool>,
        approval_rx: Receiver<ApprovalReq>,
        hub: Arc<McpHub>,
        mcp_base: ToolRegistry,
    ) -> Self {
        let bus = EventBus::new();
        let events = bus.subscribe();
        let mcp_note = if hub.is_empty() {
            None
        } else {
            let on = hub.servers.iter().filter(|s| s.is_enabled()).count();
            Some(format!(
                "{} MCP-Server geladen ({on} aktiv) — F2 öffnet das MCP-Panel.",
                hub.servers.len()
            ))
        };
        let mut app = App {
            agent: Some(agent),
            model_label,
            bus,
            events,
            running: None,
            approval_mode,
            approval_rx,
            pending: None,
            input: InputBuffer::default(),
            queue: std::collections::VecDeque::new(),
            tick: 0,
            session: None,
            lines: Vec::new(),
            assistant_start: None,
            assistant_buf: String::new(),
            scroll: 0,
            follow: true,
            should_quit: false,
            hub,
            mcp_base,
            mcp_panel: false,
            mcp_sel: 0,
            mcp_dirty: false,
        };
        app.push(note_line(
            "Willkommen beim agentkit-TUI. Stelle eine Frage und drücke Enter (Alt-Enter fügt \
             eine neue Zeile ein). Ctrl-Tab schaltet die Shell-Freigabe um. /context zeigt die \
             aktuelle Kontext-Belegung.",
            Color::DarkGray,
        ));
        if let Some(msg) = mcp_note {
            app.push(note_line(&msg, Color::Magenta));
        }
        app
    }

    fn run(mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        let mut dirty = true;
        let mut last_tick = std::time::Instant::now();
        while !self.should_quit {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                    {
                        self.on_key(key.code, key.modifiers);
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        self.on_paste(&text);
                        dirty = true;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }

            dirty |= self.drain_events();
            dirty |= self.drain_approvals();
            dirty |= self.reclaim_agent();

            // Spinner + Uhr laufen nur, solange etwas arbeitet — im Leerlauf
            // bleibt das TUI ruhig und zeichnet gar nicht neu.
            if self.running.is_some() && last_tick.elapsed() >= SPINNER_INTERVAL {
                self.tick = self.tick.wrapping_add(1);
                last_tick = std::time::Instant::now();
                dirty = true;
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- Eingabe

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Offene Freigabe hat Vorrang: nur j/n bzw. Esc.
        if self.pending.is_some() {
            match code {
                KeyCode::Char('j')
                | KeyCode::Char('J')
                | KeyCode::Char('y')
                | KeyCode::Char('Y') => self.answer_approval(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.answer_approval(false)
                }
                _ => {}
            }
            return;
        }

        // MCP-Panel offen: Tasten gehen ans Panel (Auswahl/Toggle/Schließen).
        if self.mcp_panel {
            self.on_mcp_key(code);
            return;
        }
        // F2 öffnet das MCP-Panel.
        if code == KeyCode::F(2) {
            self.mcp_panel = true;
            self.mcp_sel = self.mcp_sel.min(self.hub.servers.len().saturating_sub(1));
            return;
        }

        // Freigabe-Modus umschalten: Ctrl-Tab oder Shift-Tab (BackTab).
        if (mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Tab)
            || code == KeyCode::BackTab
        {
            let now_ask = !self.approval_mode.load(Ordering::Relaxed);
            self.approval_mode.store(now_ask, Ordering::Relaxed);
            let msg = if now_ask {
                "Shell-Freigabe: nachfragen (jeder Befehl wird bestätigt)."
            } else {
                "Shell-Freigabe: AUTO (Befehle laufen ohne Rückfrage)."
            };
            self.push(note_line(msg, Color::Yellow));
            return;
        }

        // Ab hier ist Tippen immer erlaubt — auch während ein Auftrag läuft:
        // die Eingabe wandert dann in die Warteschlange (Type-ahead).
        // Blockierende Zustände (offene Freigabe, MCP-Panel) sind oben schon
        // mit eigenen Rückkehrpunkten abgefangen.
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            // Home/End: Cursor in der Eingabe, solange dort Text steht; sonst
            // Transcript (Anfang/Ende) — beide Erwartungen ohne Extra-Belegung.
            KeyCode::End if !self.input.is_empty() => self.input.end_of_line(),
            KeyCode::Home if !self.input.is_empty() => self.input.home(),
            KeyCode::End => self.follow = true,
            KeyCode::Home => {
                self.scroll = 0;
                self.follow = false;
            }
            KeyCode::Esc => {
                if let Some(run) = &self.running {
                    // Zweites Esc: nicht länger auf den Worker warten (z. B.
                    // hängender HTTP-Read) — das TUI beendet sich sofort.
                    if run.cancel.load(Ordering::Relaxed) {
                        self.should_quit = true;
                    } else {
                        run.cancel.store(true, Ordering::Relaxed);
                        self.push(note_line(
                            "⏸ breche ab … (Esc erneut: TUI sofort beenden)",
                            Color::Yellow,
                        ));
                        // Wer abbricht, will nicht, dass gleich der nächste
                        // vorgemerkte Auftrag anläuft — die Warteschlange
                        // gehört mit verworfen.
                        if !self.queue.is_empty() {
                            self.push(note_line(
                                &format!("{} vorgemerkte Eingabe(n) verworfen.", self.queue.len()),
                                Color::Yellow,
                            ));
                            self.queue.clear();
                        }
                    }
                } else {
                    self.should_quit = true;
                }
            }
            // Alt/Shift-Enter fügt eine neue Zeile ein (mehrzeilige Eingabe), Enter sendet.
            KeyCode::Enter
                if mods.contains(KeyModifiers::ALT) || mods.contains(KeyModifiers::SHIFT) =>
            {
                self.input.insert('\n');
            }
            KeyCode::Enter => self.submit(),
            // Texteingabe/Cursor nur, solange keine Aufgabe läuft.
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            // Readline-Kürzel (kommen als Ctrl-modifizierte Buchstaben an).
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end_of_line(),
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('u') if ctrl => self.input.kill_line_start(),
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            _ => {}
        }
    }

    /// Eingefügter Text (Bracketed Paste) landet als Ganzes an der Cursorposition.
    /// Windows-Zeilenenden werden normalisiert, damit kein `\r` im Puffer landet.
    fn on_paste(&mut self, text: &str) {
        if self.pending.is_some() || self.mcp_panel {
            return;
        }
        self.input
            .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
    }

    fn answer_approval(&mut self, ok: bool) {
        if let Some((cmd, resp)) = self.pending.take() {
            let _ = resp.send(ok);
            let short: String = cmd.chars().take(60).collect();
            let (text, color) = if ok {
                (format!("✓ Freigabe erteilt: {short}"), Color::Green)
            } else {
                (format!("⨯ Freigabe abgelehnt: {short}"), Color::Red)
            };
            self.end_assistant();
            self.push(note_line(&text, color));
        }
    }

    // ----------------------------------------------------------- MCP-Panel

    /// Tastendruck im offenen MCP-Panel: Auswahl bewegen, Server umschalten, schließen.
    fn on_mcp_key(&mut self, code: KeyCode) {
        let n = self.hub.servers.len();
        match code {
            KeyCode::Up => self.mcp_sel = self.mcp_sel.saturating_sub(1),
            KeyCode::Down => {
                if n > 0 {
                    self.mcp_sel = (self.mcp_sel + 1).min(n - 1);
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_selected_mcp(),
            KeyCode::Esc | KeyCode::F(2) | KeyCode::Char('q') => self.mcp_panel = false,
            _ => {}
        }
    }

    /// Schaltet den gewählten Server um. Sub-Agenten greifen sofort (geteilter Hub);
    /// der Haupt-Agent wird neu verdrahtet, sobald er gerade nicht im Worker arbeitet
    /// (sonst gemerkt via `mcp_dirty` und beim Zurückholen nachgezogen).
    fn toggle_selected_mcp(&mut self) {
        let Some((name, new_on)) = self
            .hub
            .servers
            .get(self.mcp_sel)
            .map(|s| (s.name().to_string(), !s.is_enabled()))
        else {
            return;
        };
        match self.hub.set_enabled(&name, new_on) {
            Ok(_) => {
                if self.agent.is_some() {
                    self.rewire_main();
                } else {
                    self.mcp_dirty = true;
                }
                let state = if new_on { "aktiv" } else { "aus" };
                self.push(note_line(&format!("MCP '{name}': {state}"), Color::Yellow));
            }
            Err(e) => self.push(note_line(&format!("MCP: {e}"), Color::Red)),
        }
    }

    /// Verdrahtet den Haupt-Agenten mit den aktuell aktiven MCP-Server-Tools neu — nur
    /// wenn er gerade in Hand ist (sonst übernimmt `reclaim_agent` das via `mcp_dirty`).
    fn rewire_main(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            self.hub.rewire(agent, &self.mcp_base);
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        self.scroll = (self.scroll as i32 + delta).max(0) as usize;
        self.follow = false;
    }

    fn submit(&mut self) {
        let task = self.input.text().trim().to_string();
        if task.is_empty() {
            return;
        }
        self.input.clear();
        // Läuft noch etwas: einreihen statt verwerfen. reclaim_agent holt den
        // nächsten Eintrag, sobald der Agent zurück ist.
        if self.running.is_some() {
            self.push(note_line(
                &format!(
                    "⏳ vorgemerkt ({}): {}",
                    self.queue.len() + 1,
                    one_line(&task, 60)
                ),
                Color::DarkGray,
            ));
            self.queue.push_back(task);
            return;
        }
        self.start_task(task);
    }

    /// Startet einen Auftrag. Nimmt den Text als Argument statt ihn aus dem
    /// Eingabefeld zu ziehen: die Warteschlange darf nicht über das Feld
    /// laufen, sonst überschriebe ein nachrückender Auftrag das, was gerade
    /// getippt (aber noch nicht abgeschickt) wurde.
    fn start_task(&mut self, task: String) {
        self.end_assistant();
        self.push(user_line(&task));
        self.follow = true;

        // Lokale Slash-Befehle: beantwortet das TUI selbst, ohne den Agenten
        // (und damit einen Modell-Call) anzuwerfen.
        if task.starts_with('/') && self.handle_slash(&task) {
            return;
        }

        let Some(mut agent) = self.agent.take() else {
            // Nur erreichbar, nachdem ein Worker abgestürzt ist (er nimmt den
            // Agenten mit) — die Eingabe darf dann nicht kommentarlos versanden.
            self.push(note_line(
                "Kein Agent mehr vorhanden (abgestürzter Lauf) — bitte neu starten.",
                Color::Red,
            ));
            return;
        };
        let cancel = new_cancel();
        let bus = self.bus.clone();
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        thread::spawn(move || {
            agent.run_on_bus(&task, &bus, 0, Some(&cancel_thread), "");
            let _ = tx.send(agent);
        });
        self.running = Some(Running {
            done: rx,
            cancel,
            started: std::time::Instant::now(),
            step: 0,
        });
    }

    /// Startet den nächsten vorgemerkten Auftrag, falls einer wartet. Läuft
    /// nach jedem Lauf-Ende, damit die Warteschlange von selbst abfließt.
    fn start_queued(&mut self) {
        // Schleife, nicht ein einzelnes pop: ein lokal beantworteter Befehl
        // (`/context`) startet keinen Lauf, also käme `reclaim_agent` nie
        // wieder — alles dahinter bliebe für immer liegen. Terminiert, weil
        // jeder Durchlauf einen Eintrag entnimmt.
        while self.running.is_none() {
            let Some(task) = self.queue.pop_front() else {
                break;
            };
            self.start_task(task);
        }
    }

    /// Schreibt den Verlauf in die `--session`-Datei, falls eine gesetzt ist.
    /// Warnung statt Abbruch — ein Schreibfehler soll die Sitzung nicht beenden.
    fn save_session(&mut self) {
        let (Some(pfad), Some(agent)) = (self.session.clone(), self.agent.as_ref()) else {
            return;
        };
        if let Err(e) = agent.memory.save(&pfad) {
            self.push(note_line(
                &format!("Sitzung nicht speicherbar: {e}"),
                Color::Red,
            ));
        }
    }

    /// Lokale Slash-Befehle des TUI. `true` = erledigt, nicht ans Modell geben.
    ///
    /// Bewusst die kleine, im TUI sinnvolle Teilmenge: `/clear` (Bildschirm)
    /// und `/exit` haben hier eigene Tasten, `/mcp` ein Panel (F2). Der Rest
    /// braucht Zustand, den das TUI nicht führt.
    fn handle_slash(&mut self, task: &str) -> bool {
        let mut teile = task[1..].split_whitespace();
        let kopf = teile.next().unwrap_or("").to_lowercase();
        let rest: Vec<&str> = teile.collect();
        match kopf.as_str() {
            "context" | "ctx" => self.show_context(),
            "help" => self.push_lines(help_lines()),
            "tools" => {
                let namen = match self.agent.as_ref() {
                    Some(a) => {
                        let mut n = a.tools.names();
                        n.sort();
                        n.join(", ")
                    }
                    None => "(Agent arbeitet)".to_string(),
                };
                self.push(note_line(&format!("Werkzeuge: {namen}"), Color::Cyan));
            }
            "reset" => {
                if let Some(agent) = self.agent.as_mut() {
                    let sys = agent
                        .memory
                        .messages
                        .iter()
                        .find(|m| m["role"] == "system")
                        .and_then(|m| m["content"].as_str())
                        .map(str::to_string);
                    agent.memory = crate::ShortTermMemory::new(sys.as_deref());
                }
                self.save_session();
                self.push(note_line("Unterhaltung zurückgesetzt.", Color::Green));
            }
            "export" => self.handle_export(&rest),
            "compact" => self.handle_compact(&rest),
            _ => return false, // unbekannt: geht als normale Frage ans Modell
        }
        true
    }

    /// `/compact [hinweis]` im TUI — dieselbe Anzeige wie im REPL. Die Zahlen
    /// kommen aus `context_report`, nicht aus `memory`: mit `--ctx` ist `memory`
    /// nur der Spiegel und meldete stur „vorher == nachher".
    fn handle_compact(&mut self, rest: &[&str]) {
        let Some(agent) = self.agent.as_mut() else {
            self.push(note_line("Der Agent arbeitet gerade.", Color::Yellow));
            return;
        };
        let hinweis = rest.join(" ");
        // ctxmans Compaction kennt keinen Hinweis-Eingang. Das gehört gesagt —
        // sonst tippt man ihn und er verschwindet wortlos.
        let hinweis_verpufft = !hinweis.is_empty() && agent.context_managed();
        let vorher = context_report(agent).total;
        let bewegt = agent.compact_now(Some(hinweis.as_str()));
        let nachher = self.agent.as_ref().map(|a| context_report(a).total);
        if hinweis_verpufft {
            self.push(note_line(
                "Hinweis ohne Wirkung: mit --ctx verdichtet ctxman, dessen \
                 Compaction nimmt keinen Hinweis entgegen.",
                Color::Yellow,
            ));
        }
        let (text, farbe) = match (bewegt, nachher) {
            (true, Some(n)) => (
                format!("Kontext kompaktiert (~{vorher} → ~{n} Tokens)"),
                Color::Green,
            ),
            _ => (
                "Nichts zu kompaktieren — der Verlauf ist noch kurz.".to_string(),
                Color::DarkGray,
            ),
        };
        self.push(note_line(&text, farbe));
    }

    /// `/export` im TUI: ohne Datei die Kurzfassung in den Verlauf, mit Datei
    /// den vollen Verlauf bzw. (`--json`) die rohen Messages schreiben.
    fn handle_export(&mut self, rest: &[&str]) {
        let als_json = rest.contains(&"--json");
        let ziel = rest.iter().find(|a| !a.starts_with("--")).copied();
        let Some(agent) = self.agent.as_ref() else {
            self.push(note_line("Der Agent arbeitet gerade.", Color::Yellow));
            return;
        };
        let Some(pfad) = ziel else {
            if als_json {
                self.push(note_line(
                    "Für JSON braucht es eine Datei: /export <datei> --json",
                    Color::Yellow,
                ));
                return;
            }
            let md = agent.memory.to_markdown(false);
            self.push_lines(render_markdown_block(&md));
            self.push(note_line(
                "Gekürzte Ansicht — /export <datei> schreibt den vollen Verlauf.",
                Color::DarkGray,
            ));
            return;
        };
        let res = if als_json {
            agent.memory.save(pfad)
        } else {
            std::fs::write(pfad, agent.memory.to_markdown(true)).map_err(|e| e.to_string())
        };
        let (text, farbe) = match res {
            Ok(()) => (format!("Verlauf geschrieben: {pfad}"), Color::Green),
            Err(e) => (format!("Export fehlgeschlagen: {e}"), Color::Red),
        };
        self.push(note_line(&text, farbe));
    }

    /// Zeigt die aktuelle Kontext-Belegung (`/context`) als Block im Verlauf an.
    fn show_context(&mut self) {
        match self.agent.as_ref() {
            Some(agent) => {
                let report = context_report(agent);
                self.push_lines(context_lines(&report));
            }
            // Unerreichbar: start_task hat den Agenten in der Hand, wenn es
            // hierher kommt. Bleibt als Sicherheitsnetz stehen.
            None => self.push(note_line(
                "Der Agent arbeitet gerade — /context ist verfügbar, sobald der Lauf beendet ist.",
                Color::Yellow,
            )),
        }
    }

    // -------------------------------------------------------------- Events

    fn drain_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(ev) = self.events.try_recv() {
            self.apply_event(ev);
            any = true;
        }
        any
    }

    /// Holt höchstens EINE offene Freigabe-Anfrage herein (weitere warten im Kanal).
    fn drain_approvals(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        if let Ok((cmd, resp)) = self.approval_rx.try_recv() {
            self.end_assistant();
            self.push(note_line(
                &format!("⚠ Freigabe nötig — [j]a / [n]ein: {cmd}"),
                Color::Yellow,
            ));
            self.pending = Some((cmd, resp));
            self.follow = true;
            true
        } else {
            false
        }
    }

    fn reclaim_agent(&mut self) -> bool {
        let Some(running) = self.running.as_ref() else {
            return false;
        };
        match running.done.try_recv() {
            Ok(agent) => {
                self.agent = Some(agent);
                self.running = None;
                // Während des Laufs umgeschaltete MCP-Server jetzt am Haupt-Agenten nachziehen.
                if self.mcp_dirty {
                    self.rewire_main();
                    self.mcp_dirty = false;
                }
                self.save_session();
                self.start_queued();
                true
            }
            // Kanal zu, ohne dass der Agent zurückkam: der Worker ist gestorben
            // (Panik in einem Tool). Ohne diesen Zweig bliebe die Oberfläche für
            // immer im Zustand „arbeitet" und nähme keine Eingabe mehr an.
            Err(mpsc::TryRecvError::Disconnected) => {
                self.running = None;
                self.end_assistant();
                self.push(note_line(
                    "Der Agenten-Thread ist abgestürzt — die Sitzung lässt sich nicht \
                     fortsetzen. Mit Strg-C beenden und neu starten.",
                    Color::Red,
                ));
                // Vorgemerkte Aufträge kämen nie mehr dran — das gehört gesagt.
                if !self.queue.is_empty() {
                    self.push(note_line(
                        &format!("{} vorgemerkte Eingabe(n) verworfen.", self.queue.len()),
                        Color::Red,
                    ));
                    self.queue.clear();
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev.data {
            EventData::Step { step } => {
                if ev.source.is_empty() {
                    if let Some(run) = self.running.as_mut() {
                        run.step = step;
                    }
                }
                self.end_assistant();
                self.push(step_line(step));
            }
            EventData::ToolCall { name, args } => {
                self.end_assistant();
                self.push_lines(toolcall_lines(&name, &args, &ev.source));
            }
            EventData::ToolResult { name, result } => {
                self.end_assistant();
                self.push_lines(toolresult_lines(&name, &result));
            }
            EventData::TextDelta(t) => {
                // Sub-Agenten nicht Token-für-Token streamen (würde verschränkt unleserlich).
                if ev.source.is_empty() {
                    self.stream_text(&t);
                }
            }
            EventData::Final(t) => {
                // Kam der Text schon als Deltas, steht er bereits; sonst hier nachtragen.
                if ev.source.is_empty() && self.assistant_start.is_none() && !t.is_empty() {
                    self.stream_text(&t);
                }
                self.end_assistant();
            }
            EventData::Plan(steps) => {
                self.end_assistant();
                self.push_lines(plan_lines(&render_steps(&steps, "\n")));
            }
            EventData::Error { name, error } => {
                self.end_assistant();
                self.push(error_line(name.as_deref(), &error));
            }
            EventData::Cancelled { where_ } => {
                self.end_assistant();
                self.push(note_line(&format!("⨯ abgebrochen ({where_})"), Color::Red));
            }
            EventData::Done | EventData::None => {}
        }
    }

    /// Hängt gestreamten Antwort-Text an und bricht an `\n` in neue Zeilen um — sonst
    /// landet die ganze (oft mehrzeilige, z. B. Code/Tree-)Antwort in EINER Zeile.
    /// Hängt gestreamten Text an den Puffer und rendert den Assistant-Block neu.
    fn stream_text(&mut self, t: &str) {
        if self.assistant_start.is_none() {
            self.assistant_start = Some(self.lines.len());
            self.assistant_buf.clear();
        }
        self.assistant_buf.push_str(t);
        self.rerender_assistant();
    }

    /// Rendert den gepufferten Assistant-Text komplett neu (Markdown inkl. Tabellen
    /// und Code-Fences) und ersetzt die bisherigen Block-Zeilen. Die erste Zeile
    /// trägt das 🤖-Präfix.
    fn rerender_assistant(&mut self) {
        let Some(start) = self.assistant_start else {
            return;
        };
        self.lines.truncate(start);
        let mut block = render_markdown_block(&self.assistant_buf);
        if let Some(first) = block.first_mut() {
            let mut spans = vec![Span::styled("🤖 ", fg(Color::Green))];
            spans.append(&mut first.spans);
            *first = Line::from(spans);
        }
        self.lines.extend(block);
    }

    /// Schließt die laufende Antwort ab (der Block ist bereits final gerendert).
    fn end_assistant(&mut self) {
        self.assistant_start = None;
        self.assistant_buf.clear();
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn push_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines.extend(lines);
    }

    // -------------------------------------------------------------- Render

    fn draw(&mut self, f: &mut Frame) {
        // EIN Layout pro Frame: Feldhöhe und Rendering nutzen dieselben umbrochenen
        // Zeilen (zweimal rechnen könnte bei abweichender Breite auseinanderlaufen).
        // Die volle Terminalbreite stimmt mit der Feldbreite überein, weil das
        // Layout unten nur vertikal teilt. Höhe gedeckelt, damit das Transcript
        // nicht verschwindet.
        let input_lay = self.input.layout(input_avail(f.area().width));
        let input_h = input_lay.rows.len().min(8) as u16 + 2; // + Rahmen oben/unten
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),       // Titel
                Constraint::Min(3),          // Transcript
                Constraint::Length(input_h), // Eingabe / Freigabe (mehrzeilig)
                Constraint::Length(1),       // Fußzeile
            ])
            .split(f.area());

        // --- Titelzeile (Modell + Status + Freigabe-Modus)
        let status = match &self.running {
            Some(run) => {
                let s = run.started.elapsed().as_secs();
                Span::styled(
                    format!(
                        " {} Schritt {} · {}:{:02} ",
                        SPINNER[self.tick % SPINNER.len()],
                        run.step.max(1),
                        s / 60,
                        s % 60
                    ),
                    fg(Color::Black).bg(Color::Yellow),
                )
            }
            None => Span::styled(" bereit ", fg(Color::Black).bg(Color::Green)),
        };
        let ask = self.approval_mode.load(Ordering::Relaxed);
        let (mode_txt, mode_col) = if ask {
            (" Freigabe: nachfragen ", Color::Cyan)
        } else {
            (" Freigabe: AUTO ", Color::Red)
        };
        let mut title_spans = vec![
            Span::styled(" agentkit TUI ", bold(Color::White).bg(Color::Blue)),
            Span::raw(" · "),
            Span::styled(self.model_label.clone(), fg(Color::Cyan)),
            Span::raw(" · "),
            status,
            Span::raw(" "),
            Span::styled(mode_txt, fg(Color::Black).bg(mode_col)),
        ];
        if !self.hub.is_empty() {
            let on = self.hub.servers.iter().filter(|s| s.is_enabled()).count();
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                format!(" MCP {on}/{} ", self.hub.servers.len()),
                fg(Color::Black).bg(Color::Magenta),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

        // --- MCP-Panel hat (wenn offen) Vorrang vor dem Transcript-Bereich.
        if self.mcp_panel {
            self.draw_mcp_panel(f, chunks[1]);
            self.draw_input(f, chunks[2], input_lay);
            self.draw_footer(f, chunks[3]);
            return;
        }

        // --- Transcript (scrollbar, mit Zeilenumbruch)
        let inner_w = chunks[1].width.saturating_sub(2);
        let inner_h = chunks[1].height.saturating_sub(2) as usize;
        // Den Zeilenbedarf zählt ratatui selbst (`line_count`): das Rendering bricht
        // per Word-Wrap und Unicode-Breite (Emoji = 2 Spalten) um — eine
        // Zeichen/Breite-Näherung schätzt zu wenige Zeilen, und Auto-Scroll bliebe
        // oberhalb des Verlaufs-Endes hängen (Ende wäre nie sichtbar).
        let transcript = Paragraph::new(Text::from(self.lines.clone())).wrap(Wrap { trim: false });
        let max_scroll = transcript.line_count(inner_w).saturating_sub(inner_h);
        self.scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        if self.scroll >= max_scroll {
            self.follow = true;
        }

        let transcript = transcript.scroll((self.scroll as u16, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Verlauf ")
                .border_style(fg(Color::DarkGray)),
        );
        f.render_widget(transcript, chunks[1]);

        // --- Eingabe- oder Freigabe-Zeile
        self.draw_input(f, chunks[2], input_lay);

        // --- Fußzeile
        self.draw_footer(f, chunks[3]);
    }

    fn draw_footer(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let footer = if self.mcp_panel {
            Line::from(vec![
                Span::styled("↑↓", key_style()),
                Span::raw(" wählen  "),
                Span::styled("Space", key_style()),
                Span::raw(" an/aus  "),
                Span::styled("F2/Esc", key_style()),
                Span::raw(" schließen"),
            ])
        } else {
            Line::from(vec![
                Span::styled("Enter", key_style()),
                Span::raw(if self.running.is_some() {
                    " vormerken  "
                } else {
                    " senden  "
                }),
                Span::styled("Alt-Enter", key_style()),
                Span::raw(" neue Zeile  "),
                Span::styled("Esc", key_style()),
                Span::raw(" abbrechen  "),
                Span::styled("Ctrl-Tab", key_style()),
                Span::raw(" Freigabe  "),
                Span::styled("F2", key_style()),
                Span::raw(" MCP  "),
                Span::styled("↑↓/PgUp/PgDn", key_style()),
                Span::raw(" scrollen"),
            ])
        };
        f.render_widget(Paragraph::new(footer.style(fg(Color::DarkGray))), area);
    }

    /// Zeichnet das MCP-Server-Panel (Liste mit Auswahl + Status) in `area`.
    fn draw_mcp_panel(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.hub.is_empty() {
            lines.push(note_line(
                "Keine MCP-Server. Lege eine .mcp.json an oder starte mit --mcp-config <datei>.",
                Color::DarkGray,
            ));
        }
        for (i, s) in self.hub.servers.iter().enumerate() {
            let (mark, col) = if s.is_enabled() {
                ("[x]", Color::Green)
            } else if s.is_connected() {
                ("[ ]", Color::Gray)
            } else {
                ("[!]", Color::Red)
            };
            let detail = match &s.error {
                Some(e) => format!("nicht verbunden: {}", one_line(e, 80)),
                None => format!("{} Tools · mcp__{}__*", s.tool_count(), s.name()),
            };
            let selected = i == self.mcp_sel;
            let pointer = if selected { "› " } else { "  " };
            let name_style = if selected { bold(col) } else { fg(col) };
            lines.push(Line::from(vec![
                Span::styled(pointer, fg(Color::Cyan)),
                Span::styled(format!("{mark} {}  ", s.name()), name_style),
                Span::styled(detail, fg(Color::DarkGray)),
            ]));
        }
        let panel = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" MCP-Server (für den Agenten ein-/ausschalten) ")
                    .border_style(fg(Color::Magenta)),
            );
        f.render_widget(panel, area);
    }

    fn draw_input(&self, f: &mut Frame, area: ratatui::layout::Rect, lay: InputLayout) {
        // Offene Freigabe -> Bestätigungs-Prompt statt Eingabe.
        if let Some((cmd, _)) = &self.pending {
            let prompt = Paragraph::new(Line::from(vec![
                Span::styled("⚠ Shell ausführen? ", bold(Color::Yellow)),
                Span::styled(one_line(cmd, 120), fg(Color::White)),
                Span::styled("   [j]a / [n]ein", fg(Color::DarkGray)),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Freigabe ")
                    .border_style(fg(Color::Yellow)),
            );
            f.render_widget(prompt, area);
            return;
        }

        // Mehrzeilige Eingabe, automatisch an der Feldbreite umbrochen. Die erste
        // Anzeigezeile trägt den Prompt "› ", alle weiteren zwei Spalten Einzug.
        // Rückfragen des Agenten beantwortest du hier ganz normal als nächste
        // Nachricht (kein Sonderdialog mehr).
        // Mehr Zeilen als das (gedeckelte) Feld zeigt: so scrollen, dass die
        // Cursorzeile sichtbar bleibt.
        let InputLayout {
            rows,
            cursor_row,
            cursor_col,
        } = lay;
        // Während eines Laufs wandert die Eingabe in die Warteschlange — der
        // Titel sagt das, sonst wirkte das Tippen folgenlos.
        let (titel, rahmen) = match (self.running.is_some(), self.queue.len()) {
            (false, _) => (
                " Eingabe (Alt-Enter: neue Zeile) ".to_string(),
                Color::Green,
            ),
            (true, 0) => (
                " Eingabe (läuft — Enter merkt vor, Esc bricht ab) ".to_string(),
                Color::DarkGray,
            ),
            (true, n) => (
                format!(" Eingabe (läuft — {n} vorgemerkt, Esc bricht ab) "),
                Color::DarkGray,
            ),
        };
        let visible = usize::from(area.height).saturating_sub(2).max(1);
        let offset = cursor_row.saturating_sub(visible - 1);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
        for (i, row) in rows.into_iter().enumerate() {
            let prefix = if i == 0 { "› " } else { "  " };
            let pstyle = if i == 0 {
                bold(Color::Green)
            } else {
                fg(Color::Green)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, pstyle),
                Span::styled(row, fg(Color::White)),
            ]));
        }
        let input = Paragraph::new(Text::from(lines))
            .scroll((offset as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(titel)
                    .border_style(fg(rahmen)),
            );
        f.render_widget(input, area);

        let cx = (area.x + 1 + (INPUT_PREFIX_W + cursor_col) as u16)
            .min(area.x + area.width.saturating_sub(2));
        let cy =
            (area.y + 1 + (cursor_row - offset) as u16).min(area.y + area.height.saturating_sub(2));
        f.set_cursor_position((cx, cy));
    }
}

// ---------------------------------------------------------------- Eingabepuffer

/// Breite des Prompts "› " bzw. des Einzugs der Folgezeilen im Eingabefeld.
/// Einzige Quelle der Feldgeometrie: `input_avail` und die Cursor-x-Position
/// leiten sich hieraus ab — Höhe, Umbruch und Cursor bleiben so im Gleichschritt.
const INPUT_PREFIX_W: usize = 2;

/// Sichtbreite des Eingabetexts bei Terminalbreite `width`: Rahmen (2) + Prompt.
fn input_avail(width: u16) -> usize {
    usize::from(width).saturating_sub(2 + INPUT_PREFIX_W)
}

/// Mehrzeiliger Eingabepuffer mit Cursor (Zeichen-Index `0..=len`). Die Anzeige
/// bricht hart an der Feldbreite um — zeichenbasiert statt an Wortgrenzen, damit
/// die Cursorposition exakt aus dem Index berechenbar bleibt (ratatuis Word-Wrap
/// verrät nicht, wo ein Zeichen gelandet ist). Breitzeichen (Emoji) verschieben
/// die sichtbare Cursorspalte minimal; für ein Eingabefeld verschmerzbar.
#[derive(Default)]
struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
}

/// Ergebnis von [`InputBuffer::layout`]: Anzeigezeilen + Cursorposition darin.
struct InputLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

impl InputBuffer {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn insert_str(&mut self, s: &str) {
        // splice statt Einzel-inserts: verschiebt den Rest hinter dem Cursor
        // nur einmal (relevant bei großen Pastes mitten in den Text).
        let n = s.chars().count();
        self.chars.splice(self.cursor..self.cursor, s.chars());
        self.cursor += n;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Anfang der aktuellen logischen Zeile (nach dem letzten `\n` vor dem Cursor).
    fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// Ende der aktuellen logischen Zeile (vor dem nächsten `\n`).
    fn end_of_line(&mut self) {
        self.cursor = self.chars[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.chars.len(), |i| self.cursor + i);
    }

    fn line_start(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
    }

    /// Ctrl-W: löscht rückwärts erst Leerraum, dann das Wort davor.
    fn delete_word_back(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Ctrl-U: löscht vom Zeilenanfang bis zum Cursor.
    fn kill_line_start(&mut self) {
        let start = self.line_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Bricht den Text hart an `avail` Spalten um. Eine logische Zeile, die die
    /// Breite exakt füllt, bekommt eine leere Folgezeile — dort muss der Cursor
    /// stehen können (Index == Zeilenlänge), sonst zeigte er ins Nichts.
    fn layout(&self, avail: usize) -> InputLayout {
        let avail = avail.max(1);
        let mut rows: Vec<String> = Vec::new();
        let (mut cursor_row, mut cursor_col) = (0, 0);
        let mut start = 0; // Zeichen-Index des aktuellen Zeilenanfangs
        for line in self.chars.split(|&c| c == '\n') {
            let len = line.len();
            if (start..=start + len).contains(&self.cursor) {
                let col = self.cursor - start;
                cursor_row = rows.len() + col / avail;
                cursor_col = col % avail;
            }
            for r in 0..=len / avail {
                let b = ((r + 1) * avail).min(len);
                rows.push(line[r * avail..b].iter().collect());
            }
            start += len + 1; // + das übersprungene `\n`
        }
        InputLayout {
            rows,
            cursor_row,
            cursor_col,
        }
    }
}

/// Die Slash-Befehle, die das TUI selbst beantwortet — `/help` zeigt sie.
/// Kürzer als im REPL: `/clear` und `/exit` haben hier Tasten, MCP ein Panel.
fn help_lines() -> Vec<Line<'static>> {
    const BEFEHLE: &[(&str, &str)] = &[
        ("/help", "diese Hilfe"),
        ("/context", "Kontext-Belegung zeigen (auch /ctx)"),
        ("/tools", "registrierte Werkzeuge auflisten"),
        ("/reset", "Unterhaltung vergessen"),
        ("/compact", "Kontext jetzt verdichten (/compact <hinweis>)"),
        (
            "/export",
            "Verlauf zeigen; /export <datei> [--json] schreibt ihn",
        ),
    ];
    let mut out = vec![Line::from(Span::styled("Befehle", bold(Color::White)))];
    for (cmd, was) in BEFEHLE {
        out.push(Line::from(vec![
            Span::styled(format!("  {cmd:<10}"), fg(Color::Cyan)),
            Span::styled((*was).to_string(), fg(Color::Gray)),
        ]));
    }
    out.push(note_line(
        "Tasten: F2 MCP · Ctrl-Tab Freigabe · Esc abbrechen · Ctrl-C beenden",
        Color::DarkGray,
    ));
    out
}

// ----------------------------------------------------------------- Zeilen-Helfer

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

// --------------------------------------------------------- /context-Anzeige

/// Farbpalette der Kontext-Abschnitte (zyklisch); freier Platz ist dunkelgrau.
const CTX_PALETTE: [Color; 8] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::LightRed,
    Color::LightCyan,
    Color::LightMagenta,
];

/// Raster-Maße der visuellen Belegungs-Anzeige (Zellen = `CTX_ROWS · CTX_COLS`).
const CTX_ROWS: usize = 4;
const CTX_COLS: usize = 48;

/// Rendert den [`ContextReport`] als Transcript-Block: Kopfzeile mit Summe und
/// Budget, darunter das farbige Belegungs-Raster (à la `/context` der
/// Claude-Code-CLI) und pro Abschnitt eine Legenden-Zeile mit Tokens, Anteil
/// und Anzahl der Einträge.
fn context_lines(r: &ContextReport) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Skala fürs Raster und die Prozente: das Budget — außer die Belegung hat es
    // bereits überschritten, dann die Belegung selbst (Raster bleibt voll).
    let scale = r.budget.max(r.total).max(1);

    out.push(Line::from(vec![
        Span::styled("⛁ Kontext ", bold(Color::White)),
        Span::styled(
            format!(
                "— {} von {} Tokens belegt ({})",
                fmt_tokens(r.total),
                fmt_tokens(r.budget),
                fmt_pct(r.total, r.budget),
            ),
            fg(Color::Gray),
        ),
        Span::styled(
            if r.managed {
                "  ·  Verwaltung: ctxman"
            } else {
                "  ·  Schätzung: Zeichen/4"
            },
            fg(Color::DarkGray),
        ),
    ]));
    out.push(Line::from(""));

    // Zellen pro Abschnitt (mindestens 1, sobald er Tokens hat); Rest = frei.
    let cells_total = CTX_ROWS * CTX_COLS;
    let mut flat: Vec<Option<Color>> = Vec::with_capacity(cells_total);
    for (i, seg) in r.segments.iter().enumerate() {
        if seg.tokens == 0 {
            continue;
        }
        let cells = ((seg.tokens * cells_total + scale / 2) / scale).max(1);
        let color = CTX_PALETTE[i % CTX_PALETTE.len()];
        flat.extend(std::iter::repeat(Some(color)).take(cells));
    }
    flat.truncate(cells_total);
    flat.resize(cells_total, None);
    for row in flat.chunks(CTX_COLS) {
        let mut spans = vec![Span::raw("   ")];
        for cell in row {
            spans.push(match cell {
                Some(c) => Span::styled("█", fg(*c)),
                None => Span::styled("░", fg(Color::DarkGray)),
            });
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(""));

    // Legende: pro Abschnitt Farbfeld, Name, Tokens, Anteil, Anzahl (+ Hinweis).
    let width = r
        .segments
        .iter()
        .map(|s| s.label.chars().count())
        .max()
        .unwrap_or(0)
        .max("frei".len());
    for (i, seg) in r.segments.iter().enumerate() {
        let color = CTX_PALETTE[i % CTX_PALETTE.len()];
        let mut spans = vec![
            Span::raw("   "),
            Span::styled("█ ", fg(color)),
            Span::styled(format!("{:<width$}", seg.label), fg(Color::White)),
            Span::styled(
                format!(
                    "  {:>9} Tokens  ({:>7})",
                    fmt_tokens(seg.tokens),
                    fmt_pct(seg.tokens, scale)
                ),
                fg(Color::Gray),
            ),
            Span::styled(format!("  · {}", fmt_count(seg.count)), fg(Color::DarkGray)),
        ];
        if let Some(note) = &seg.note {
            spans.push(Span::styled(format!("  · {note}"), fg(Color::DarkGray)));
        }
        out.push(Line::from(spans));
    }

    // Frei-Zeile — bzw. Warnung, wenn das Budget bereits überschritten ist.
    if r.total <= r.budget {
        let free = r.budget - r.total;
        out.push(Line::from(vec![
            Span::raw("   "),
            Span::styled("░ ", fg(Color::DarkGray)),
            Span::styled(format!("{:<width$}", "frei"), fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "  {:>9} Tokens  ({:>7})",
                    fmt_tokens(free),
                    fmt_pct(free, scale)
                ),
                fg(Color::DarkGray),
            ),
        ]));
    } else {
        out.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                format!(
                    "⚠ Budget um {} Tokens überschritten — Kompaktierung steht an.",
                    fmt_tokens(r.total - r.budget)
                ),
                fg(Color::Red),
            ),
        ]));
    }
    out
}

fn user_line(task: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("🧑 ", fg(Color::Cyan)),
        Span::styled(task.to_string(), bold(Color::Cyan)),
    ])
}

fn step_line(step: usize) -> Line<'static> {
    Line::styled(
        format!("── Schritt {step} ──"),
        fg(Color::DarkGray).add_modifier(Modifier::DIM),
    )
}

/// Kurze Tool-Argumente werden bis zu dieser Länge inline gezeigt, längere als
/// mehrzeiliger, eingerückter JSON-Block.
const INLINE_JSON_MAX: usize = 60;
/// Deckel für die Anzahl Zeilen, die ein einzelnes Tool-Ergebnis belegt.
const RESULT_MAX_LINES: usize = 30;

/// Tool-Call-Zeile(n); Sub-Agenten werden mit ihrem Rollen-Tag vorangestellt.
/// Kurze Argumente bleiben inline (`name({...})`), lange JSON-Objekte werden
/// über mehrere Zeilen hübsch eingerückt und farbig hervorgehoben.
fn toolcall_lines(name: &str, args: &Value, source: &str) -> Vec<Line<'static>> {
    let mut head: Vec<Span<'static>> = Vec::new();
    if !source.is_empty() {
        let label = source.split(':').next().unwrap_or(source);
        head.push(Span::styled(format!("[{label}] "), fg(Color::DarkGray)));
    }
    head.push(Span::styled("🔧 ", fg(Color::Yellow)));
    head.push(Span::styled(name.to_string(), bold(Color::Yellow)));

    let empty_args =
        matches!(args, Value::Null) || matches!(args, Value::Object(m) if m.is_empty());
    if empty_args {
        head.push(Span::styled("()", fg(Color::Yellow)));
        return vec![Line::from(head)];
    }

    let compact = highlight_json(args, false);
    let compact_len: usize = compact.first().map_or(0, |l| {
        l.spans.iter().map(|s| s.content.chars().count()).sum()
    });

    // Kurz genug: alles auf eine Zeile — name( …farbig… ).
    if compact_len <= INLINE_JSON_MAX {
        head.push(Span::styled("(", fg(Color::Yellow)));
        if let Some(first) = compact.into_iter().next() {
            head.extend(first.spans);
        }
        head.push(Span::styled(")", fg(Color::Yellow)));
        return vec![Line::from(head)];
    }

    // Lang: name(\n   {pretty}\n)
    head.push(Span::styled("(", fg(Color::Yellow)));
    let mut out = vec![Line::from(head)];
    out.extend(indent_lines(highlight_json(args, true), "   "));
    out.push(Line::from(Span::styled(")", fg(Color::Yellow))));
    out
}

/// Tool-Ergebnis-Zeile(n). Reines JSON wird prettified + gehighlightet, sonst
/// bleibt mehrzeiliger Text mehrzeilig (statt auf eine Zeile kollabiert).
fn toolresult_lines(name: &str, result: &str) -> Vec<Line<'static>> {
    let prefix = || {
        vec![
            Span::styled("   ↳ ", fg(Color::DarkGray)),
            Span::styled(format!("{name}: "), fg(Color::DarkGray)),
        ]
    };
    let trimmed = result.trim();

    // 1) Reines JSON -> prettify + Syntax-Highlighting.
    if looks_like_json(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            let mut body = highlight_json(&v, true).into_iter();
            let mut head = prefix();
            if let Some(first) = body.next() {
                head.extend(first.spans);
            }
            let mut out = vec![Line::from(head)];
            out.extend(indent_lines(body.collect(), "     "));
            return cap_lines(out);
        }
    }

    // 2) Mehrzeiliger Text -> Zeilen erhalten, unter dem Ergebnis eingerückt.
    if trimmed.contains('\n') {
        let mut out = Vec::new();
        for (i, raw) in trimmed.lines().enumerate() {
            let seg = Span::styled(one_line(raw, 200), fg(Color::Gray));
            if i == 0 {
                let mut head = prefix();
                head.push(seg);
                out.push(Line::from(head));
            } else {
                out.push(Line::from(vec![Span::raw("     "), seg]));
            }
        }
        return cap_lines(out);
    }

    // 3) Einzeiler.
    let mut head = prefix();
    head.push(Span::styled(one_line(trimmed, 200), fg(Color::Gray)));
    vec![Line::from(head)]
}

/// Deckelt eine Zeilenliste auf [`RESULT_MAX_LINES`] und hängt einen Hinweis an.
fn cap_lines(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > RESULT_MAX_LINES {
        let extra = lines.len() - RESULT_MAX_LINES;
        lines.truncate(RESULT_MAX_LINES);
        lines.push(Line::from(Span::styled(
            format!("     … ({extra} weitere Zeilen)"),
            fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
    }
    lines
}

/// Plan als mehrzeilige Liste mit farbigen Checkboxen (`[x]/[~]/[ ]`).
fn plan_lines(rendered: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, raw) in rendered.lines().enumerate() {
        let mut spans = vec![if i == 0 {
            Span::styled("📋 ", fg(Color::Magenta))
        } else {
            Span::raw("   ")
        }];
        spans.extend(style_plan_line(raw));
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "📋 (kein Plan)",
            fg(Color::Magenta),
        )));
    }
    out
}

/// Färbt die Checkbox einer Plan-Zeile je nach Status; der Rest bleibt magenta.
fn style_plan_line(raw: &str) -> Vec<Span<'static>> {
    let (mark, rest, col) = if let Some(r) = raw.strip_prefix("[x] ") {
        ("[x] ", r, Color::Green)
    } else if let Some(r) = raw.strip_prefix("[~] ") {
        ("[~] ", r, Color::Yellow)
    } else if let Some(r) = raw.strip_prefix("[ ] ") {
        ("[ ] ", r, Color::DarkGray)
    } else {
        return vec![Span::styled(raw.to_string(), fg(Color::Magenta))];
    };
    vec![
        Span::styled(mark.to_string(), bold(col)),
        Span::styled(rest.to_string(), fg(Color::Magenta)),
    ]
}

fn error_line(name: Option<&str>, error: &str) -> Line<'static> {
    let prefix = match name {
        Some(n) => format!("⚠ {n}: "),
        None => "⚠ ".to_string(),
    };
    Line::from(vec![
        Span::styled(prefix, bold(Color::Red)),
        Span::styled(one_line(error, 300), fg(Color::Red)),
    ])
}

fn note_line(text: &str, color: Color) -> Line<'static> {
    Line::styled(text.to_string(), fg(color).add_modifier(Modifier::ITALIC))
}

fn key_style() -> Style {
    bold(Color::Black).bg(Color::DarkGray)
}

// ----------------------------------------------------------------- Hilfsfunktionen

// --------------------------------------------------------- JSON-Highlighting

fn json_key_style() -> Style {
    fg(Color::Cyan)
}
fn json_str_style() -> Style {
    fg(Color::Green)
}
fn json_num_style() -> Style {
    fg(Color::Yellow)
}
fn json_lit_style() -> Style {
    fg(Color::Magenta) // true/false/null
}
fn json_punct_style() -> Style {
    fg(Color::DarkGray)
}

/// Sammelt gestylte Spans zu Zeilen. `pretty` schaltet Zeilenumbrüche und
/// Einrückung ein; kompakt bleibt alles auf einer Zeile.
struct JsonFmt {
    pretty: bool,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
}

impl JsonFmt {
    fn new(pretty: bool) -> Self {
        JsonFmt {
            pretty,
            lines: Vec::new(),
            cur: Vec::new(),
        }
    }

    fn span(&mut self, text: impl Into<String>, style: Style) {
        self.cur.push(Span::styled(text.into(), style));
    }

    /// Zeilenumbruch (nur `pretty`): schließt die aktuelle Zeile und rückt die
    /// nächste um `depth` Ebenen (je 2 Leerzeichen) ein.
    fn newline(&mut self, depth: usize) {
        if !self.pretty {
            return;
        }
        let done = std::mem::take(&mut self.cur);
        self.lines.push(Line::from(done));
        if depth > 0 {
            self.cur.push(Span::raw("  ".repeat(depth)));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.cur.is_empty() || self.lines.is_empty() {
            let last = std::mem::take(&mut self.cur);
            self.lines.push(Line::from(last));
        }
        self.lines
    }
}

/// Highlightet einen JSON-Wert: `pretty=false` → eine kompakte farbige Zeile,
/// `pretty=true` → mehrere eingerückte Zeilen.
fn highlight_json(v: &Value, pretty: bool) -> Vec<Line<'static>> {
    let mut f = JsonFmt::new(pretty);
    emit_json(v, &mut f, 0);
    f.finish()
}

fn emit_json(v: &Value, f: &mut JsonFmt, depth: usize) {
    match v {
        Value::Object(map) => {
            if map.is_empty() {
                f.span("{}", json_punct_style());
                return;
            }
            f.span("{", json_punct_style());
            let n = map.len();
            for (i, (k, val)) in map.iter().enumerate() {
                f.newline(depth + 1);
                f.span(format!("\"{}\"", escape_json_str(k)), json_key_style());
                f.span(if f.pretty { ": " } else { ":" }, json_punct_style());
                emit_json(val, f, depth + 1);
                if i + 1 < n {
                    f.span(",", json_punct_style());
                }
            }
            f.newline(depth);
            f.span("}", json_punct_style());
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                f.span("[]", json_punct_style());
                return;
            }
            f.span("[", json_punct_style());
            let n = arr.len();
            for (i, val) in arr.iter().enumerate() {
                f.newline(depth + 1);
                emit_json(val, f, depth + 1);
                if i + 1 < n {
                    f.span(",", json_punct_style());
                }
            }
            f.newline(depth);
            f.span("]", json_punct_style());
        }
        Value::String(s) => f.span(format!("\"{}\"", escape_json_str(s)), json_str_style()),
        Value::Number(num) => f.span(num.to_string(), json_num_style()),
        Value::Bool(b) => f.span(b.to_string(), json_lit_style()),
        Value::Null => f.span("null", json_lit_style()),
    }
}

/// Escaped den Inhalt eines JSON-Strings (ohne die umschließenden Quotes).
fn escape_json_str(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""));
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or(s)
        .to_string()
}

/// Grober JSON-Test ohne Parsen: Beginn/Ende sehen nach Objekt/Array aus.
fn looks_like_json(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b'{') | Some(b'[')) && matches!(b.last(), Some(b'}') | Some(b']'))
}

/// Stellt jeder Zeile ein Padding voran (für eingerückte JSON-/Ergebnis-Blöcke).
fn indent_lines(lines: Vec<Line<'static>>, pad: &str) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            let mut spans = Vec::with_capacity(l.spans.len() + 1);
            spans.push(Span::raw(pad.to_string()));
            spans.append(&mut l.spans);
            Line::from(spans)
        })
        .collect()
}

// ------------------------------------------------------------ Markdown-Block

/// Rendert einen mehrzeiligen Markdown-Text: erkennt Code-Fences (```lang …```,
/// JSON wird gehighlightet) und Tabellen (`| … |` mit Trennzeile) als Blöcke,
/// alles andere Zeile für Zeile via [`style_markdown_spans`].
fn render_markdown_block(text: &str) -> Vec<Line<'static>> {
    let raw: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let line = raw[i];
        let trimmed = line.trim_start();

        // Code-Fence: ```lang … ``` (schließende Fence optional, falls noch streamend).
        if let Some(lang) = trimmed.strip_prefix("```") {
            let lang = lang.trim().to_string();
            let mut body: Vec<&str> = Vec::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < raw.len() {
                if raw[j].trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                body.push(raw[j]);
                j += 1;
            }
            out.extend(render_code_block(&lang, &body));
            i = if closed { j + 1 } else { j };
            continue;
        }

        // Tabelle: Kopfzeile mit '|' plus eine Trennzeile (|---|---|) darunter.
        if line.contains('|') && i + 1 < raw.len() && is_table_separator(raw[i + 1]) {
            let mut rows: Vec<&str> = vec![line];
            let mut j = i + 2; // Trennzeile überspringen
            while j < raw.len() && raw[j].contains('|') && !raw[j].trim().is_empty() {
                rows.push(raw[j]);
                j += 1;
            }
            out.extend(render_table(&rows));
            i = j;
            continue;
        }

        out.push(Line::from(style_markdown_spans(line)));
        i += 1;
    }
    if out.is_empty() {
        out.push(Line::from(Span::raw(String::new())));
    }
    out
}

/// Rendert einen Code-Block mit grauem Randbalken. `json` (oder ein Body, der
/// nach JSON aussieht) wird geparst und syntax-gehighlightet, sonst cyan.
fn render_code_block(lang: &str, body: &[&str]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if !lang.is_empty() {
        out.push(Line::from(Span::styled(
            format!(" {lang} "),
            fg(Color::Black).bg(Color::DarkGray),
        )));
    }
    let joined = body.join("\n");
    let is_json =
        lang.eq_ignore_ascii_case("json") || (lang.is_empty() && looks_like_json(joined.trim()));
    if is_json {
        if let Ok(v) = serde_json::from_str::<Value>(joined.trim()) {
            out.extend(bar_lines(highlight_json(&v, true)));
            return out;
        }
    }
    for l in body {
        out.push(Line::from(vec![
            Span::styled("▏ ", fg(Color::DarkGray)),
            Span::styled((*l).to_string(), fg(Color::Cyan)),
        ]));
    }
    out
}

/// Stellt jeder Zeile einen grauen Randbalken `▏ ` voran (Code-Block-Optik).
fn bar_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            let mut spans = vec![Span::styled("▏ ", fg(Color::DarkGray))];
            spans.append(&mut l.spans);
            Line::from(spans)
        })
        .collect()
}

/// Trennzeile einer Markdown-Tabelle, z. B. `|---|:--:|---|`.
fn is_table_separator(s: &str) -> bool {
    let t = s.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Zerlegt eine Tabellenzeile in Zellen (umschließende Pipes werden entfernt).
fn split_row(s: &str) -> Vec<String> {
    let t = s.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Rendert eine Markdown-Tabelle als ausgerichtete Box (Kopf fett cyan, Rahmen grau).
fn render_table(rows: &[&str]) -> Vec<Line<'static>> {
    let cells: Vec<Vec<String>> = rows.iter().map(|r| split_row(r)).collect();
    let cols = cells.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    // Spaltenbreiten = längster Zellinhalt (in Zeichen) je Spalte.
    let mut width = vec![0usize; cols];
    for row in &cells {
        for (c, cell) in row.iter().enumerate() {
            width[c] = width[c].max(cell.chars().count());
        }
    }

    let border = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (c, w) in width.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if c + 1 < cols { mid } else { right });
        }
        Line::from(Span::styled(s, fg(Color::DarkGray)))
    };

    let data_row = |row: &[String], header: bool| -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("│", fg(Color::DarkGray)));
        for (c, w) in width.iter().enumerate() {
            let raw = row.get(c).map(String::as_str).unwrap_or("");
            let padded = format!(" {raw:<w$} ", w = *w);
            let style = if header {
                bold(Color::Cyan)
            } else if c == 0 {
                fg(Color::White)
            } else {
                fg(Color::Gray)
            };
            spans.push(Span::styled(padded, style));
            spans.push(Span::styled("│", fg(Color::DarkGray)));
        }
        Line::from(spans)
    };

    let mut out = vec![border("┌", "┬", "┐")];
    if let Some(head) = cells.first() {
        out.push(data_row(head, true));
        out.push(border("├", "┼", "┤"));
    }
    for row in cells.iter().skip(1) {
        out.push(data_row(row, false));
    }
    out.push(border("└", "┴", "┘"));
    out
}

// ------------------------------------------------------------- Markdown-Zeile

/// Stylt EINE Zeile Markdown: Aufzählungen (`- `/`* `/`+ `), nummerierte Listen,
/// Überschriften (`#…`), Zitate (`> `) sowie inline `**fett**` und `` `code` ``.
/// Führende Einrückung bleibt erhalten, damit verschachtelte Listen fluchten.
fn style_markdown_spans(line: &str) -> Vec<Span<'static>> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];

    let mut spans: Vec<Span<'static>> = Vec::new();
    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }

    // Überschrift: #, ##, ###, …
    if rest.starts_with('#') {
        let title = rest.trim_start_matches('#').trim_start();
        spans.push(Span::styled(title.to_string(), bold(Color::Magenta)));
        return spans;
    }
    // Zitat: > …
    if let Some(q) = rest.strip_prefix("> ") {
        spans.push(Span::styled("▏ ", fg(Color::DarkGray)));
        spans.extend(style_inline(
            q,
            fg(Color::Gray).add_modifier(Modifier::ITALIC),
        ));
        return spans;
    }
    // Aufzählung: - / * / +
    if let Some(item) = strip_bullet(rest) {
        spans.push(Span::styled("• ", fg(Color::Yellow)));
        spans.extend(style_inline(item, fg(Color::White)));
        return spans;
    }
    // Nummerierte Liste: "1. " / "2) " …
    if let Some((num, item)) = strip_ordered(rest) {
        spans.push(Span::styled(format!("{num}. "), bold(Color::Yellow)));
        spans.extend(style_inline(item, fg(Color::White)));
        return spans;
    }
    // Normaler Text (mit inline-Formatierung).
    spans.extend(style_inline(rest, fg(Color::White)));
    spans
}

/// Entfernt einen Aufzählungs-Marker (`- `, `* `, `+ `) am Zeilenanfang.
fn strip_bullet(s: &str) -> Option<&str> {
    ["- ", "* ", "+ "].iter().find_map(|m| s.strip_prefix(m))
}

/// Erkennt "N. " / "N) " am Zeilenanfang und gibt (N, Rest) zurück.
fn strip_ordered(s: &str) -> Option<(u32, &str)> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let after = &s[digits.len()..];
    let rest = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((digits.parse().ok()?, rest))
}

/// Zerlegt inline-Markdown (`**fett**`, `` `code` ``) in gestylte Spans; alles
/// andere erhält `base`.
fn style_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        // **fett**
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_close(&chars, i + 2, &['*', '*']) {
                flush_span(&mut buf, &mut spans, base);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        // `code`
        if chars[i] == '`' {
            if let Some(end) = find_close(&chars, i + 1, &['`']) {
                flush_span(&mut buf, &mut spans, base);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(inner, fg(Color::Cyan)));
                i = end + 1;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush_span(&mut buf, &mut spans, base);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// Schiebt den gepufferten Klartext als `base`-gestylten Span heraus.
fn flush_span(buf: &mut String, spans: &mut Vec<Span<'static>>, base: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), base));
    }
}

/// Sucht ab `start` das nächste (nicht-leere) schließende `delim`.
fn find_close(chars: &[char], start: usize, delim: &[char]) -> Option<usize> {
    let mut i = start;
    while i + delim.len() <= chars.len() {
        if &chars[i..i + delim.len()] == delim && i > start {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Die Scroll-Berechnung nutzt `Paragraph::line_count` — hier festgenagelt,
    /// dass die Zählung Word-Wrap und Unicode-Breite berücksichtigt (genau die
    /// beiden Fälle, in denen die frühere Zeichen/Breite-Näherung zu wenige
    /// Zeilen schätzte und das Transcript-Ende abgeschnitten wurde).
    #[test]
    fn scroll_zaehlung_entspricht_ratatui_wrap() {
        let rows = |s: &str, w: u16| {
            Paragraph::new(Text::from(vec![Line::raw(s.to_string())]))
                .wrap(Wrap { trim: false })
                .line_count(w)
        };
        // Langes "Wort" wird hart umgebrochen: 25 Zeichen / 10 = 3 Zeilen.
        assert_eq!(rows(&"a".repeat(25), 10), 3);
        // Word-Wrap bricht an Wortgrenzen — 3 Zeilen, obwohl 20 Zeichen / 10 = 2.
        assert_eq!(rows("aaaaaa bbbbbb cccccc", 10), 3);
        // Emoji belegen 2 Spalten — 8 Emoji = 16 Spalten = 2 Zeilen bei Breite 10.
        assert_eq!(rows(&"🤖".repeat(8), 10), 2);
    }

    /// Text einer Zeile aus ihren Spans rekonstruieren (fürs Assertion-Handling).
    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn buf(text: &str, cursor: usize) -> InputBuffer {
        InputBuffer {
            chars: text.chars().collect(),
            cursor,
        }
    }

    #[test]
    fn input_einfuegen_und_loeschen_am_cursor() {
        let mut b = buf("abc", 1);
        b.insert('x');
        assert_eq!(b.text(), "axbc");
        assert_eq!(b.cursor, 2);
        b.backspace();
        assert_eq!(b.text(), "abc");
        assert_eq!(b.cursor, 1);
        b.delete();
        assert_eq!(b.text(), "ac");
        // Ränder: kein Panik, keine Bewegung.
        let mut b = buf("a", 0);
        b.backspace();
        b.left();
        assert_eq!((b.text().as_str(), b.cursor), ("a", 0));
        b.right();
        b.right();
        assert_eq!(b.cursor, 1);
        b.delete();
        assert_eq!(b.text(), "a");
    }

    /// Baut eine App ohne Terminal, mit einem „laufenden" Auftrag, dessen
    /// Kanal nie etwas liefert — so lässt sich das Verhalten während eines
    /// Laufs prüfen, ohne einen Agenten zu starten.
    fn app_mit_laufendem_auftrag() -> (App, mpsc::Sender<Agent>) {
        let agent = Agent::builder(Arc::new(crate::testing::FakeLlm::new(vec![]))).build();
        let (_tx, approval_rx) = mpsc::channel();
        let mut app = App::new(
            agent,
            "test".to_string(),
            Arc::new(AtomicBool::new(true)),
            approval_rx,
            Arc::new(McpHub::empty()),
            ToolRegistry::new(),
        );
        let (done_tx, done) = mpsc::channel();
        app.running = Some(Running {
            done,
            cancel: new_cancel(),
            started: std::time::Instant::now(),
            step: 0,
        });
        // Sender zurückgeben: solange der Test ihn hält, bleibt der Kanal
        // offen und der Lauf gilt als „noch unterwegs".
        (app, done_tx)
    }

    /// Type-ahead: eine Eingabe während des Laufs wird vorgemerkt statt
    /// verworfen, und die Reihenfolge bleibt erhalten.
    #[test]
    fn eingabe_waehrend_des_laufs_wird_vorgemerkt() {
        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.input.insert_str("erste frage");
        app.submit();
        app.input.insert_str("zweite frage");
        app.submit();

        assert_eq!(app.queue.len(), 2, "beide Eingaben müssen warten");
        assert_eq!(app.queue[0], "erste frage");
        assert_eq!(app.queue[1], "zweite frage");
        // Das Eingabefeld ist nach dem Vormerken wieder frei.
        assert!(app.input.is_empty());
        // Der laufende Auftrag wurde NICHT ersetzt.
        assert!(app.running.is_some());
    }

    /// Die lokalen Slash-Befehle beantwortet das TUI selbst; alles Unbekannte
    /// geht als normale Frage ans Modell.
    #[test]
    fn tui_slash_befehle_werden_lokal_beantwortet() {
        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.running = None; // Agent in der Hand

        assert!(app.handle_slash("/help"));
        assert!(app.handle_slash("/tools"));
        assert!(app.handle_slash("/context"));
        assert!(app.handle_slash("/ctx"));
        // Unbekanntes bleibt eine Frage ans Modell.
        assert!(!app.handle_slash("/gibtsnicht"));
        assert!(!app.handle_slash("/wie geht das"));
    }

    /// `/reset` leert die Unterhaltung, behält aber den System-Prompt.
    #[test]
    fn tui_reset_behaelt_den_system_prompt() {
        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.running = None;
        let agent = app.agent.as_mut().unwrap();
        agent.memory = crate::ShortTermMemory::new(Some("Testsystem"));
        agent.memory.add_user("eine frage");
        assert_eq!(app.agent.as_ref().unwrap().memory.messages.len(), 2);

        assert!(app.handle_slash("/reset"));

        let mem = &app.agent.as_ref().unwrap().memory;
        assert_eq!(mem.messages.len(), 1);
        assert_eq!(mem.messages[0]["content"], "Testsystem");
    }

    /// Eine Sitzungsdatei ohne System-Prompt (so schreibt `/export --json`)
    /// bekommt den frischen davorgesetzt — ohne Instruktionen wäre der
    /// fortgesetzte Agent ein anderer als der beendete.
    #[test]
    fn tui_laedt_sitzung_und_ergaenzt_den_system_prompt() {
        let pfad =
            std::env::temp_dir().join(format!("agentkit_tuiload_{}.json", std::process::id()));
        let pfad = pfad.to_str().unwrap().to_string();
        let mut ohne_system = crate::ShortTermMemory::new(None);
        ohne_system.add_user("alte frage");
        ohne_system.save(&pfad).unwrap();

        let mut agent = Agent::builder(Arc::new(crate::testing::FakeLlm::new(vec![])))
            .system("Testsystem")
            .build();
        let (text, _) = load_session(&mut agent, &pfad);

        assert!(text.starts_with("Sitzung geladen"), "{text}");
        assert_eq!(agent.memory.messages[0]["role"], "system");
        assert!(agent.memory.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Testsystem"));
        assert_eq!(agent.memory.messages[1]["content"], "alte frage");
        std::fs::remove_file(&pfad).ok();
    }

    /// Die Sitzungsdatei wird geschrieben — sonst verlöre das TUI beim
    /// Schließen weiterhin alles.
    #[test]
    fn tui_speichert_die_sitzung() {
        let pfad =
            std::env::temp_dir().join(format!("agentkit_tuises_{}.json", std::process::id()));
        let pfad = pfad.to_str().unwrap().to_string();
        std::fs::remove_file(&pfad).ok();

        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.running = None;
        app.session = Some(pfad.clone());
        app.agent.as_mut().unwrap().memory.add_user("gemerkt");

        app.save_session();

        let geladen = crate::ShortTermMemory::load(&pfad).unwrap();
        assert!(geladen.messages.iter().any(|m| m["content"] == "gemerkt"));
        std::fs::remove_file(&pfad).ok();
    }

    /// Der echte Weg: `reclaim_agent` holt den Agenten zurück und lässt die
    /// Warteschlange abfließen. Ein lokal beantworteter Befehl (`/context`)
    /// startet dabei KEINEN Lauf — die Abarbeitung darf deshalb nicht nach
    /// einem Eintrag stehenbleiben, sonst bliebe alles dahinter liegen.
    #[test]
    fn reclaim_laesst_die_warteschlange_abfliessen() {
        let (mut app, done_tx) = app_mit_laufendem_auftrag();
        app.input.insert_str("/context");
        app.submit();
        app.input.insert_str("echte frage");
        app.submit();
        assert_eq!(app.queue.len(), 2);

        // Der Worker gibt den Agenten zurück -> reclaim_agent greift.
        let agent = Agent::builder(Arc::new(crate::testing::FakeLlm::new(vec![]))).build();
        done_tx.send(agent).unwrap();
        app.reclaim_agent();

        assert!(
            app.queue.is_empty(),
            "hinter /context blieb etwas liegen: {:?}",
            app.queue
        );
        // Die echte Frage läuft jetzt; der Kontext-Report steht im Verlauf.
        assert!(
            app.running.is_some(),
            "die echte Frage wurde nicht gestartet"
        );
    }

    /// Stirbt der Worker, kann nichts Vorgemerktes mehr laufen — das wird
    /// verworfen und gesagt, statt still zu versanden.
    #[test]
    fn absturz_verwirft_die_warteschlange() {
        let (mut app, done_tx) = app_mit_laufendem_auftrag();
        app.input.insert_str("kommt nie dran");
        app.submit();
        assert_eq!(app.queue.len(), 1);

        drop(done_tx); // Worker gestorben: Kanal zu, kein Agent zurück
        app.reclaim_agent();

        assert!(app.queue.is_empty());
        assert!(app.running.is_none());
        let text: String = app
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("verworfen"),
            "kein Hinweis im Verlauf:\n{text}"
        );
    }

    /// Esc bricht ab — dann darf NICHT gleich der nächste vorgemerkte Auftrag
    /// anlaufen. Wer abbricht, will Ruhe, nicht die Warteschlange.
    #[test]
    fn abbruch_verwirft_die_warteschlange() {
        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.input.insert_str("kommt noch");
        app.submit();
        assert_eq!(app.queue.len(), 1);

        app.on_key(KeyCode::Esc, KeyModifiers::NONE);

        assert!(app.queue.is_empty(), "Warteschlange überlebte den Abbruch");
        // Der Lauf selbst ist als abgebrochen markiert, das TUI läuft weiter.
        assert!(app.running.as_ref().unwrap().cancel.load(Ordering::Relaxed));
        assert!(!app.should_quit);
    }

    /// Der nachrückende Auftrag darf gerade getippten, noch nicht
    /// abgeschickten Text nicht überschreiben — deshalb geht die
    /// Warteschlange über `start_task`, nicht über das Eingabefeld.
    #[test]
    fn nachruecken_ueberschreibt_die_eingabe_nicht() {
        let (mut app, _done_tx) = app_mit_laufendem_auftrag();
        app.input.insert_str("vorgemerkt");
        app.submit();
        app.running = None; // Lauf beendet
        app.input.insert_str("halb getippt");

        app.start_queued();

        assert!(app.queue.is_empty(), "Warteschlange muss abgeflossen sein");
        assert_eq!(
            app.input.text(),
            "halb getippt",
            "der angefangene Text wurde überschrieben"
        );
    }

    #[test]
    fn input_insert_str_fuegt_am_cursor_ein() {
        let mut b = buf("ad", 1);
        b.insert_str("b\nc");
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn input_home_end_beziehen_sich_auf_logische_zeile() {
        let mut b = buf("erste\nzweite zeile", 9); // in "zweite"
        b.home();
        assert_eq!(b.cursor, 6);
        b.end_of_line();
        assert_eq!(b.cursor, 18);
        let mut b = buf("erste\nzweite", 2);
        b.end_of_line();
        assert_eq!(b.cursor, 5); // vor dem \n, nicht am Textende
    }

    #[test]
    fn input_ctrl_w_und_ctrl_u() {
        let mut b = buf("eins zwei  ", 11);
        b.delete_word_back();
        assert_eq!(b.text(), "eins ");
        let mut b = buf("a\nbc def", 8);
        b.kill_line_start();
        assert_eq!(b.text(), "a\n");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn input_layout_bricht_hart_um() {
        // 25 Zeichen bei Breite 10 -> 3 Zeilen; Cursor am Ende in Zeile 2, Spalte 5.
        let b = buf(&"a".repeat(25), 25);
        let lay = b.layout(10);
        assert_eq!(lay.rows.len(), 3);
        assert_eq!(lay.rows[0].chars().count(), 10);
        assert_eq!((lay.cursor_row, lay.cursor_col), (2, 5));
    }

    #[test]
    fn input_layout_exakt_volle_zeile_hat_cursor_reserve() {
        // Genau Breite gefüllt: der Cursor am Ende braucht eine leere Folgezeile.
        let b = buf(&"a".repeat(10), 10);
        let lay = b.layout(10);
        assert_eq!(lay.rows.len(), 2);
        assert_eq!(lay.rows[1], "");
        assert_eq!((lay.cursor_row, lay.cursor_col), (1, 0));
    }

    #[test]
    fn input_layout_mehrzeilig_mit_cursor_in_erster_zeile() {
        let b = buf("kurz\nlang lang lang", 2);
        let lay = b.layout(80);
        assert_eq!(
            lay.rows,
            vec!["kurz".to_string(), "lang lang lang".to_string()]
        );
        assert_eq!((lay.cursor_row, lay.cursor_col), (0, 2));
        // Cursor direkt auf dem \n zählt zur ersten Zeile (Spalte = Zeilenlänge).
        let b = buf("kurz\nx", 4);
        let lay = b.layout(80);
        assert_eq!((lay.cursor_row, lay.cursor_col), (0, 4));
    }

    #[test]
    fn input_layout_leer_und_endend_mit_newline() {
        let lay = buf("", 0).layout(10);
        assert_eq!(lay.rows, vec![String::new()]);
        assert_eq!((lay.cursor_row, lay.cursor_col), (0, 0));
        // End-\n erzeugt eine leere Schlusszeile, Cursor dahinter.
        let lay = buf("ab\n", 3).layout(10);
        assert_eq!(lay.rows, vec!["ab".to_string(), String::new()]);
        assert_eq!((lay.cursor_row, lay.cursor_col), (1, 0));
    }

    #[test]
    fn highlight_json_pretty_breaks_and_indents() {
        let v = serde_json::json!({"a": 1, "b": [true, null]});
        let lines = highlight_json(&v, true);
        // Mehrzeilig: öffnende Klammer, Felder eingerückt, schließende Klammer.
        assert!(lines.len() > 3);
        assert_eq!(text_of(&lines[0]), "{");
        assert!(text_of(&lines[1]).starts_with("  \"a\": 1"));
        assert_eq!(text_of(lines.last().unwrap()), "}");
    }

    #[test]
    fn highlight_json_compact_is_single_line() {
        let v = serde_json::json!({"path": "inbox"});
        let lines = highlight_json(&v, false);
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines[0]), "{\"path\":\"inbox\"}");
    }

    #[test]
    fn toolcall_short_args_inline() {
        let lines = toolcall_lines("list_files", &serde_json::json!({"path": "inbox"}), "");
        assert_eq!(lines.len(), 1);
        assert!(text_of(&lines[0]).contains("list_files({\"path\":\"inbox\"})"));
    }

    #[test]
    fn toolcall_long_args_multiline() {
        let big = serde_json::json!({
            "command": "pwsh -File tools/gobd-manifest.ps1 -Source 'inbox/x.pdf' -Dir 'out/BK'"
        });
        let lines = toolcall_lines("run_shell", &big, "");
        assert!(lines.len() >= 3); // name( … )
        assert!(text_of(&lines[0]).ends_with("run_shell("));
        assert_eq!(text_of(lines.last().unwrap()), ")");
    }

    #[test]
    fn toolresult_json_is_prettified() {
        let out = r#"{"format":"zugferd","artefakte":5}"#;
        let lines = toolresult_lines("run_shell", out);
        assert!(lines.len() > 1); // aufgebrochen statt einer Zeile
        assert!(text_of(&lines[0]).contains("run_shell:"));
    }

    #[test]
    fn toolresult_multiline_text_preserved() {
        let lines = toolresult_lines("list_files", "a.pdf\nb.pdf\nc.xml");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn toolresult_capped() {
        let many = (0..100)
            .map(|i| format!("zeile {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = toolresult_lines("grep", &many);
        assert_eq!(lines.len(), RESULT_MAX_LINES + 1); // +1 Hinweiszeile
        assert!(text_of(lines.last().unwrap()).contains("weitere Zeilen"));
    }

    #[test]
    fn markdown_bullet_gets_glyph() {
        let spans = style_markdown_spans("- erster Punkt");
        assert_eq!(spans[0].content.as_ref(), "• ");
    }

    #[test]
    fn markdown_ordered_keeps_number() {
        let spans = style_markdown_spans("2. zweiter");
        assert_eq!(spans[0].content.as_ref(), "2. ");
    }

    #[test]
    fn markdown_indented_bullet_keeps_indent() {
        let spans = style_markdown_spans("    - eingerückt");
        assert_eq!(spans[0].content.as_ref(), "    ");
        assert_eq!(spans[1].content.as_ref(), "• ");
    }

    #[test]
    fn inline_bold_and_code_split() {
        let spans = style_inline("ein **fettes** `wort` hier", fg(Color::White));
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "ein fettes wort hier");
        // "fettes" trägt BOLD.
        assert!(spans.iter().any(
            |s| s.content.as_ref() == "fettes" && s.style.add_modifier.contains(Modifier::BOLD)
        ));
    }

    #[test]
    fn heading_is_stripped_and_bold() {
        let spans = style_markdown_spans("## Titel");
        assert_eq!(spans[0].content.as_ref(), "Titel");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn json_code_fence_is_highlighted() {
        let md = "Hier:\n```json\n{\"a\": 1, \"b\": true}\n```\nfertig.";
        let lines = render_markdown_block(md);
        let joined: String = lines
            .iter()
            .map(|l| text_of(l))
            .collect::<Vec<_>>()
            .join("\n");
        // Kein rohes ``` mehr, aber der JSON-Inhalt prettified (aufgebrochen).
        assert!(!joined.contains("```"));
        assert!(joined.contains("\"a\": 1"));
        // Sprach-Tag + mind. eine Balken-Zeile.
        assert!(lines.iter().any(|l| text_of(l).contains("json")));
        assert!(lines.iter().any(|l| text_of(l).starts_with("▏ ")));
    }

    #[test]
    fn plain_code_fence_kept_verbatim() {
        let md = "```\nls -la\necho hi\n```";
        let lines = render_markdown_block(md);
        let joined: String = lines
            .iter()
            .map(|l| text_of(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("ls -la"));
        assert!(joined.contains("echo hi"));
        assert!(!joined.contains("```"));
    }

    #[test]
    fn markdown_table_renders_as_box() {
        let md = "| A | B |\n|---|---|\n| eins | zwei |\n| x | y |";
        let lines = render_markdown_block(md);
        let joined: String = lines
            .iter()
            .map(|l| text_of(l))
            .collect::<Vec<_>>()
            .join("\n");
        // Rahmen + Zellinhalte, keine rohen Pipes-Trennzeile mehr.
        assert!(joined.contains('┌') && joined.contains('┐'));
        assert!(joined.contains('│'));
        assert!(joined.contains("eins") && joined.contains("zwei"));
        assert!(!joined.contains("---"));
    }

    #[test]
    fn table_columns_are_aligned() {
        let md = "| Kurz | Lang |\n|---|---|\n| a | bbbbbbbb |\n| cccc | d |";
        let lines = render_markdown_block(md);
        // Alle Rahmen-/Datenzeilen sind gleich lang (ausgerichtet).
        let widths: Vec<usize> = lines.iter().map(|l| text_of(l).chars().count()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "Spalten nicht ausgerichtet: {widths:?}"
        );
    }

    #[test]
    fn is_table_separator_detects() {
        assert!(is_table_separator("|---|---|"));
        assert!(is_table_separator(" :---: | ---"));
        assert!(!is_table_separator("| a | b |"));
        assert!(!is_table_separator("kein trenner"));
    }

    #[test]
    fn context_zahlen_deutsch_formatiert() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(16190), "16.190");
        assert_eq!(fmt_tokens(1_234_567), "1.234.567");
        assert_eq!(fmt_pct(16_190, 100_000), "16,2 %");
        assert_eq!(fmt_pct(1, 0), "100,0 %"); // whole=0 wird abgefangen
        assert_eq!(fmt_count(1), "1 Eintrag");
        assert_eq!(fmt_count(5), "5 Einträge");
    }

    #[test]
    fn context_lines_raster_und_legende() {
        let seg = |label: &str, tokens: usize| crate::ContextSegment {
            label: label.to_string(),
            tokens,
            count: 1,
            note: None,
        };
        let r = ContextReport {
            segments: vec![seg("System-Prompt", 2_480), seg("Tool-Ergebnisse", 8_400)],
            total: 10_880,
            budget: 100_000,
            managed: false,
        };
        let lines = context_lines(&r);
        let texts: Vec<String> = lines.iter().map(|l| text_of(l)).collect();
        let joined = texts.join("\n");

        // Kopfzeile mit Summe, Budget und Modus.
        assert!(
            joined.contains("10.880 von 100.000 Tokens"),
            "war: {joined}"
        );
        assert!(joined.contains("Schätzung: Zeichen/4"));
        // Raster: CTX_ROWS Zeilen à CTX_COLS Zellen, Belegung ~10,9 % ⇒ 21 Zellen.
        let grid: Vec<&String> = texts
            .iter()
            .filter(|t| {
                t.trim_start().chars().all(|c| c == '█' || c == '░') && !t.trim().is_empty()
            })
            .collect();
        assert_eq!(grid.len(), CTX_ROWS);
        assert!(grid
            .iter()
            .all(|t| t.trim_start().chars().count() == CTX_COLS));
        let used: usize = grid
            .iter()
            .map(|t| t.chars().filter(|c| *c == '█').count())
            .sum();
        assert_eq!(
            used,
            21,
            "10.880/100.000 von {} Zellen",
            CTX_ROWS * CTX_COLS
        );
        // Legende: pro Abschnitt eine Zeile mit Tokens, dazu die Frei-Zeile.
        assert!(joined.contains("System-Prompt") && joined.contains("2.480 Tokens"));
        assert!(joined.contains("frei") && joined.contains("89.120 Tokens"));
    }

    #[test]
    fn context_lines_warnt_bei_ueberschrittenem_budget() {
        let r = ContextReport {
            segments: vec![crate::ContextSegment {
                label: "Tool-Ergebnisse".to_string(),
                tokens: 12_000,
                count: 3,
                note: Some("1 ausgelagert".to_string()),
            }],
            total: 12_000,
            budget: 8_000,
            managed: true,
        };
        let joined: String = context_lines(&r)
            .iter()
            .map(|l| text_of(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Verwaltung: ctxman"));
        assert!(joined.contains("1 ausgelagert"));
        assert!(
            joined.contains("um 4.000 Tokens überschritten"),
            "war: {joined}"
        );
        assert!(!joined.contains("frei"));
    }
}
