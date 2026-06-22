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
use crate::{build_coding_agent, new_cancel, Agent, Cancel, CodingAgentConfig, EventBus, Strategy};

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

    let (agent, model_label) = build_agent(&cfg, approval_mode.clone(), req_tx);

    let terminal = ratatui::init();
    let result = App::new(agent, model_label, approval_mode, req_rx).run(terminal);
    ratatui::restore();
    result
}

/// Baut den Agenten: voller Coding-Agent (echter LLM) oder schlanker Demo-Agent.
fn build_agent(
    cfg: &TuiConfig,
    approval_mode: Arc<AtomicBool>,
    req_tx: Sender<ApprovalReq>,
) -> (Agent, String) {
    let (llm, label) = build_llm(cfg.force_demo);

    // Demo-Modus: kleiner, netzfreier Werkzeugkasten.
    if label.starts_with("demo") {
        let agent = Agent::builder(llm)
            .tools(demo_tools())
            .strategy(cfg.strategy)
            .max_steps(cfg.max_steps)
            .build();
        return (agent, label);
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

    let acfg = CodingAgentConfig {
        workspace: &cfg.workspace,
        strategy: cfg.strategy,
        max_steps: cfg.max_steps,
        skills: cfg.skills.as_deref(),
        agents: cfg.agents.as_deref(),
        memory: cfg.memory.as_deref(),
        subagents: cfg.subagents,
        plan_sep: "  ", // einzeilige PLAN-Anzeige im TUI
    };
    let (agent, _plan, _skills, _roles) = build_coding_agent(llm, &acfg, approve);
    (agent, label)
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

    input: String,
    lines: Vec<Line<'static>>,
    /// Index der gerade gestreamten Assistant-Zeile (Tokens werden als Spans angehängt).
    cur_assistant: Option<usize>,

    /// Scroll-Offset in gerenderten Zeilen; `follow` heftet ans Ende (Auto-Scroll).
    scroll: usize,
    follow: bool,
    should_quit: bool,
}

impl App {
    fn new(
        agent: Agent,
        model_label: String,
        approval_mode: Arc<AtomicBool>,
        approval_rx: Receiver<ApprovalReq>,
    ) -> Self {
        let bus = EventBus::new();
        let events = bus.subscribe();
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
            cur_assistant: None,
            scroll: 0,
            follow: true,
            should_quit: false,
        };
        app.push(note_line(
            "Willkommen beim agentkit-TUI. Stelle eine Frage und drücke Enter. \
             Ctrl-Tab schaltet die Shell-Freigabe um.",
            Color::DarkGray,
        ));
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
                KeyCode::Char('j') | KeyCode::Char('J') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.answer_approval(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer_approval(false),
                _ => {}
            }
            return;
        }

        // Freigabe-Modus umschalten: Ctrl-Tab oder Shift-Tab (BackTab).
        if (mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Tab) || code == KeyCode::BackTab
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
            self.cur_assistant = None;
            self.push(note_line(&text, color));
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
        self.cur_assistant = None;
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
            self.cur_assistant = None;
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
            true
        } else {
            false
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev.data {
            EventData::Step { step } => {
                self.cur_assistant = None;
                self.push(step_line(step));
            }
            EventData::ToolCall { name, args } => {
                self.cur_assistant = None;
                self.push(toolcall_line(&name, &args, &ev.source));
            }
            EventData::ToolResult { name, result } => {
                self.cur_assistant = None;
                self.push(toolresult_line(&name, &result));
            }
            EventData::TextDelta(t) => {
                // Sub-Agenten nicht Token-für-Token streamen (würde verschränkt unleserlich).
                if ev.source.is_empty() {
                    self.stream_text(&t);
                }
            }
            EventData::Final(t) => {
                // Kam der Text schon als Deltas, steht er bereits; sonst hier nachtragen
                // (mit Zeilenumbruch-Behandlung wie beim Streaming).
                if ev.source.is_empty() && self.cur_assistant.is_none() && !t.is_empty() {
                    self.stream_text(&t);
                }
                self.cur_assistant = None;
            }
            EventData::Plan(p) => {
                self.cur_assistant = None;
                self.push(plan_line(&p));
            }
            EventData::Error { name, error } => {
                self.cur_assistant = None;
                self.push(error_line(name.as_deref(), &error));
            }
            EventData::Cancelled { where_ } => {
                self.cur_assistant = None;
                self.push(note_line(&format!("⨯ abgebrochen ({where_})"), Color::Red));
            }
            EventData::Done | EventData::None => {}
        }
    }

    /// Hängt gestreamten Antwort-Text an und bricht an `\n` in neue Zeilen um — sonst
    /// landet die ganze (oft mehrzeilige, z. B. Code/Tree-)Antwort in EINER Zeile.
    fn stream_text(&mut self, t: &str) {
        let mut segments = t.split('\n');
        if let Some(first) = segments.next() {
            self.append_assistant(first);
        }
        // Jedes weitere Segment folgte auf ein '\n' -> neue Fortsetzungszeile.
        for seg in segments {
            self.lines.push(assistant_cont_line(seg));
            self.cur_assistant = Some(self.lines.len() - 1);
        }
    }

    /// Hängt Text an die laufende Antwort-Zeile an (O(1) pro Token) oder beginnt eine.
    fn append_assistant(&mut self, t: &str) {
        match self.cur_assistant {
            Some(idx) => self.lines[idx]
                .spans
                .push(Span::styled(t.to_string(), fg(Color::White))),
            None => {
                self.lines.push(assistant_line(t));
                self.cur_assistant = Some(self.lines.len() - 1);
            }
        }
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    // -------------------------------------------------------------- Render

    fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Titel
                Constraint::Min(3),    // Transcript
                Constraint::Length(3), // Eingabe / Freigabe
                Constraint::Length(1), // Fußzeile
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
        let title = Line::from(vec![
            Span::styled(" agentkit TUI ", bold(Color::White).bg(Color::Blue)),
            Span::raw(" · "),
            Span::styled(self.model_label.clone(), fg(Color::Cyan)),
            Span::raw(" · "),
            status,
            Span::raw(" "),
            Span::styled(mode_txt, fg(Color::Black).bg(mode_col)),
        ]);
        f.render_widget(Paragraph::new(title), chunks[0]);

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
        let footer = Line::from(vec![
            Span::styled("Enter", key_style()),
            Span::raw(" senden  "),
            Span::styled("Esc", key_style()),
            Span::raw(" abbrechen/beenden  "),
            Span::styled("Ctrl-Tab", key_style()),
            Span::raw(" Freigabe-Modus  "),
            Span::styled("↑↓/PgUp/PgDn", key_style()),
            Span::raw(" scrollen"),
        ])
        .style(fg(Color::DarkGray));
        f.render_widget(Paragraph::new(footer), chunks[3]);
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

        let (prompt_style, content): (Style, String) = if self.running.is_some() {
            (
                fg(Color::DarkGray),
                "Esc drücken, um den laufenden Auftrag abzubrechen…".to_string(),
            )
        } else {
            (fg(Color::White), self.input.clone())
        };
        let input = Paragraph::new(Line::from(vec![
            Span::styled("› ", bold(Color::Green)),
            Span::styled(content, prompt_style),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Eingabe ")
                .border_style(fg(if self.running.is_some() {
                    Color::DarkGray
                } else {
                    Color::Green
                })),
        );
        f.render_widget(input, area);

        if self.running.is_none() {
            const PROMPT_W: u16 = 3;
            let cx = area.x + PROMPT_W + self.input.chars().count() as u16;
            let cy = area.y + 1;
            f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(2)), cy));
        }
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

fn assistant_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("🤖 ", fg(Color::Green)),
        Span::styled(text.to_string(), fg(Color::White)),
    ])
}

/// Fortsetzungszeile einer Antwort (nach einem `\n`) — ohne 🤖-Präfix, damit
/// Code-Blöcke/Trees ihre eigene Einrückung behalten.
fn assistant_cont_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), fg(Color::White)))
}

fn step_line(step: usize) -> Line<'static> {
    Line::styled(
        format!("── Schritt {step} ──"),
        fg(Color::DarkGray).add_modifier(Modifier::DIM),
    )
}

/// Tool-Call-Zeile; Sub-Agenten werden mit ihrem Rollen-Tag vorangestellt.
fn toolcall_line(name: &str, args: &Value, source: &str) -> Line<'static> {
    let mut spans = Vec::new();
    if !source.is_empty() {
        let label = source.split(':').next().unwrap_or(source);
        spans.push(Span::styled(format!("[{label}] "), fg(Color::DarkGray)));
    }
    spans.push(Span::styled("🔧 ", fg(Color::Yellow)));
    spans.push(Span::styled(name.to_string(), bold(Color::Yellow)));
    spans.push(Span::styled(
        format!("({})", compact_json(args)),
        fg(Color::Yellow),
    ));
    Line::from(spans)
}

fn toolresult_line(name: &str, result: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("   ↳ ", fg(Color::DarkGray)),
        Span::styled(format!("{name}: "), fg(Color::DarkGray)),
        Span::styled(one_line(result, 200), fg(Color::Gray)),
    ])
}

fn plan_line(plan: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("📋 ", fg(Color::Magenta)),
        Span::styled(one_line(plan, 300), fg(Color::Magenta)),
    ])
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

/// Kompaktes JSON ohne Whitespace.
fn compact_json(v: &Value) -> String {
    one_line(&v.to_string(), 200)
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
}
