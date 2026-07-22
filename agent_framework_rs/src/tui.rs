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

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::coding::ApproveFn;
use crate::demo::{build_llm, demo_tools};
use crate::events::{AgentEvent, EventData};
use crate::{
    build_coding_agent, new_cancel, render_steps, Agent, Cancel, CodingAgentConfig, EventBus,
    McpHub, Strategy, ToolRegistry,
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
        }
    }
}

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
    let (agent, model_label, mcp_base) =
        build_agent(&cfg, approval_mode.clone(), req_tx, hub.clone());

    let terminal = ratatui::init();
    let result = App::new(agent, model_label, approval_mode, req_rx, hub, mcp_base).run(terminal);
    ratatui::restore();
    result
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

/// Baut den Agenten: voller Coding-Agent (echter LLM) oder schlanker Demo-Agent.
/// Gibt zusätzlich die MCP-freie Basis-Registry zurück (Grundlage fürs Umschalten).
fn build_agent(
    cfg: &TuiConfig,
    approval_mode: Arc<AtomicBool>,
    req_tx: Sender<ApprovalReq>,
    hub: Arc<McpHub>,
) -> (Agent, String, ToolRegistry) {
    let (llm, label) = build_llm(cfg.force_demo);

    // Demo-Modus: kleiner, netzfreier Werkzeugkasten — MCP-Tools werden dennoch eingeklinkt.
    if label.starts_with("demo") {
        let mut agent = Agent::builder(llm)
            .tools(demo_tools())
            .strategy(cfg.strategy)
            .max_steps(cfg.max_steps)
            .build();
        let mcp_base = hub.apply(&mut agent);
        return (agent, label, mcp_base);
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
    };
    let (agent, _plan, _skills, _roles, mcp_base) = build_coding_agent(llm, &acfg, approve, hub);
    (agent, label, mcp_base)
}

// ------------------------------------------------------------------------- App/UI

/// Laufende Hintergrund-Aufgabe: der Agent in einem Worker-Thread. Der `done`-Kanal
/// gibt den Agenten nach Abschluss zurück (Memory bleibt für den nächsten Turn erhalten).
struct Running {
    done: Receiver<Agent>,
    cancel: Cancel,
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

    /// Eingabepuffer (mehrzeilig: `\n` trennt Zeilen; Alt/Shift-Enter fügt eine ein).
    input: String,
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
            input: String::new(),
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
             eine neue Zeile ein). Ctrl-Tab schaltet die Shell-Freigabe um.",
            Color::DarkGray,
        ));
        if let Some(msg) = mcp_note {
            app.push(note_line(&msg, Color::Magenta));
        }
        app
    }

    fn run(mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        let mut dirty = true;
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
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }

            dirty |= self.drain_events();
            dirty |= self.drain_approvals();
            dirty |= self.reclaim_agent();
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

        match code {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::End => self.follow = true,
            KeyCode::Home => {
                self.scroll = 0;
                self.follow = false;
            }
            KeyCode::Esc => {
                if let Some(run) = &self.running {
                    run.cancel.store(true, Ordering::Relaxed);
                } else {
                    self.should_quit = true;
                }
            }
            // Alt/Shift-Enter fügt eine neue Zeile ein (mehrzeilige Eingabe), Enter sendet.
            KeyCode::Enter
                if self.running.is_none()
                    && (mods.contains(KeyModifiers::ALT) || mods.contains(KeyModifiers::SHIFT)) =>
            {
                self.input.push('\n');
            }
            KeyCode::Enter => self.submit(),
            // Texteingabe nur, solange keine Aufgabe läuft.
            KeyCode::Backspace if self.running.is_none() => {
                self.input.pop();
            }
            KeyCode::Char(c) if self.running.is_none() => self.input.push(c),
            _ => {}
        }
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
        if self.running.is_some() {
            return;
        }
        let task = self.input.trim().to_string();
        if task.is_empty() {
            return;
        }
        self.input.clear();
        self.end_assistant();
        self.push(user_line(&task));
        self.follow = true;

        let mut agent = match self.agent.take() {
            Some(a) => a,
            None => return,
        };
        let cancel = new_cancel();
        let bus = self.bus.clone();
        let (tx, rx) = mpsc::channel();
        let cancel_thread = cancel.clone();
        thread::spawn(move || {
            agent.run_on_bus(&task, &bus, 0, Some(&cancel_thread), "");
            let _ = tx.send(agent);
        });
        self.running = Some(Running { done: rx, cancel });
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
        let finished = self.running.as_ref().and_then(|r| r.done.try_recv().ok());
        if let Some(agent) = finished {
            self.agent = Some(agent);
            self.running = None;
            // Während des Laufs umgeschaltete MCP-Server jetzt am Haupt-Agenten nachziehen.
            if self.mcp_dirty {
                self.rewire_main();
                self.mcp_dirty = false;
            }
            true
        } else {
            false
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev.data {
            EventData::Step { step } => {
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

    /// Höhe des Eingabefelds inkl. Rahmen: wächst mit der Zahl der Eingabezeilen
    /// (mehrzeilig via Alt-Enter), gedeckelt, damit das Transcript nicht verschwindet.
    fn input_height(&self) -> u16 {
        let rows = self.input.split('\n').count().clamp(1, 8) as u16;
        rows + 2 // oben/unten Rahmen
    }

    fn draw(&mut self, f: &mut Frame) {
        let input_h = self.input_height();
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
        let status = if self.running.is_some() {
            Span::styled(" arbeitet… ", fg(Color::Black).bg(Color::Yellow))
        } else {
            Span::styled(" bereit ", fg(Color::Black).bg(Color::Green))
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
            self.draw_input(f, chunks[2]);
            self.draw_footer(f, chunks[3]);
            return;
        }

        // --- Transcript (scrollbar, mit Zeilenumbruch)
        let inner_w = chunks[1].width.saturating_sub(2);
        let inner_h = chunks[1].height.saturating_sub(2) as usize;
        let max_scroll = wrapped_rows(&self.lines, inner_w).saturating_sub(inner_h);
        self.scroll = if self.follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        if self.scroll >= max_scroll {
            self.follow = true;
        }
        let scroll = self.scroll;

        let transcript = Paragraph::new(Text::from(self.lines.clone()))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Verlauf ")
                    .border_style(fg(Color::DarkGray)),
            );
        f.render_widget(transcript, chunks[1]);

        // --- Eingabe- oder Freigabe-Zeile
        self.draw_input(f, chunks[2]);

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
                Span::raw(" senden  "),
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

    fn draw_input(&self, f: &mut Frame, area: ratatui::layout::Rect) {
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

        // Läuft ein Auftrag, zeigt das Feld nur einen Hinweis (keine Eingabe möglich).
        if self.running.is_some() {
            let hint = Paragraph::new(Line::from(vec![
                Span::styled("› ", bold(Color::DarkGray)),
                Span::styled(
                    "Esc drücken, um den laufenden Auftrag abzubrechen…",
                    fg(Color::DarkGray),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Eingabe ")
                    .border_style(fg(Color::DarkGray)),
            );
            f.render_widget(hint, area);
            return;
        }

        // Mehrzeilige Eingabe: jede '\n'-Zeile ist eine eigene Zeile. Erste Zeile trägt den
        // Prompt "› ", Folgezeilen sind um zwei Spalten eingerückt. Rückfragen des Agenten
        // beantwortest du hier ganz normal als nächste Nachricht (kein Sonderdialog mehr).
        let segs: Vec<&str> = self.input.split('\n').collect();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(segs.len());
        for (i, seg) in segs.iter().enumerate() {
            let prefix = if i == 0 { "› " } else { "  " };
            let pstyle = if i == 0 {
                bold(Color::Green)
            } else {
                fg(Color::Green)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, pstyle),
                Span::styled((*seg).to_string(), fg(Color::White)),
            ]));
        }
        let input = Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Eingabe (Alt-Enter: neue Zeile) ")
                .border_style(fg(Color::Green)),
        );
        f.render_widget(input, area);

        // Cursor ans Ende der letzten Eingabezeile.
        const PROMPT_W: u16 = 3; // 1 Rahmen + 2 Prompt
        let last_idx = segs.len().saturating_sub(1) as u16;
        let last_len = segs.last().map_or(0, |s| s.chars().count()) as u16;
        let cx = (area.x + PROMPT_W + last_len).min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + last_idx).min(area.y + area.height.saturating_sub(2));
        f.set_cursor_position((cx, cy));
    }
}

// ----------------------------------------------------------------- Zeilen-Helfer

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

fn bold(color: Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
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

/// Schätzt die Anzahl gerenderter (umgebrochener) Zeilen für das Auto-Scrolling.
fn wrapped_rows(lines: &[Line], width: u16) -> usize {
    let w = (width as usize).max(1);
    lines
        .iter()
        .map(|l| {
            let len: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            if len == 0 {
                1
            } else {
                len.div_ceil(w)
            }
        })
        .sum()
}

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

/// Auf eine Zeile zusammenziehen und auf `max` Zeichen kürzen.
fn one_line(s: &str, max: usize) -> String {
    let collapsed: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if collapsed.chars().count() > max {
        let truncated: String = collapsed.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_rows_counts_soft_wraps() {
        let lines = vec![Line::raw("a".repeat(25))];
        assert_eq!(wrapped_rows(&lines, 10), 3); // 25 Zeichen / 10 = aufgerundet 3
    }

    /// Text einer Zeile aus ihren Spans rekonstruieren (fürs Assertion-Handling).
    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
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
}
