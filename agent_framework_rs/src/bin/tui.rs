//! agentkit TUI — ein interaktives Terminal-UI für den Rust-Agenten.
//!
//! ```bash
//! cargo run --bin tui --features tui                 # mit Azure/OpenAI (Default-Features)
//! cargo run --bin tui --no-default-features --features tui   # nur Demo-Modus (kein Netz)
//! cargo run --bin tui --features tui -- --demo       # Demo-Modus erzwingen
//! ```
//!
//! Warum das gut zum Framework passt: Der Agent-Loop ist bereits *event-basiert*
//! (`run_on_bus` publiziert [`AgentEvent`]s auf einen [`EventBus`]). Das TUI ist
//! genau ein weiterer Consumer dieses Stroms — der Agent läuft in einem
//! Worker-Thread, das UI rendert die Events live (Schritte, Tool-Calls, gestreamte
//! Tokens). `Esc` setzt den kooperativen Stop-Knopf (`Cancel`).
//!
//! Bewusst schlank gehalten: nur `ratatui` als zusätzliche Abhängigkeit (crossterm
//! kommt re-exportiert via `ratatui::crossterm`). Kein async-Runtime — passend zum
//! Rest des Crates.

use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use agentkit::events::{AgentEvent, EventData};
use agentkit::llm::{Chunk, ChunkStream, Llm, Message};
use agentkit::{new_cancel, Agent, Cancel, EventBus, Strategy, ToolRegistry};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }
    let force_demo = args.iter().any(|a| a == "--demo");
    let strategy = if args.iter().any(|a| a == "--plan") {
        Strategy::Plan
    } else if args.iter().any(|a| a == "--plain") {
        Strategy::Plain
    } else {
        Strategy::React
    };

    let (llm, model_label) = build_llm(force_demo);
    let agent = Agent::builder(llm)
        .tools(demo_tools())
        .strategy(strategy)
        .build();

    let terminal = ratatui::init();
    let result = App::new(agent, model_label).run(terminal);
    ratatui::restore();
    result
}

fn print_help() {
    println!(
        "agentkit TUI — interaktives Terminal-UI für den Rust-Agenten\n\n\
         AUFRUF:\n  \
           cargo run --bin tui --features tui [-- OPTIONEN]\n\n\
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

// --------------------------------------------------------------------- LLM-Auswahl

/// Wählt den LLM: Azure -> OpenAI -> Demo (Fallback). Gibt zusätzlich ein
/// Label für die Titelzeile zurück.
fn build_llm(force_demo: bool) -> (Arc<dyn Llm>, String) {
    if !force_demo {
        #[cfg(feature = "openai")]
        {
            if std::env::var("AZURE_OPENAI_API_KEY").is_ok() {
                if let Ok(llm) = agentkit::azure_from_env() {
                    let dep =
                        std::env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "?".into());
                    return (Arc::new(llm), format!("azure:{dep}"));
                }
            }
            if std::env::var("OPENAI_API_KEY").is_ok() {
                if let Ok(llm) = agentkit::openai_from_env() {
                    let model =
                        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
                    return (Arc::new(llm), format!("openai:{model}"));
                }
            }
        }
    }
    (Arc::new(DemoLlm), "demo (kein Netz)".to_string())
}

/// Ein kleiner Demo-Werkzeugkasten — dieselben Tools, die das `DemoLlm` ansteuert,
/// aber auch ein echtes Modell kann sie nutzen.
fn demo_tools() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.add(
        "add",
        "Addiert zwei ganze Zahlen a und b.",
        json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
        |args: Value| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        },
    );
    reg.add(
        "wetter",
        "Liefert (frei erfundenes) Wetter für eine Stadt.",
        json!({"type":"object","properties":{"stadt":{"type":"string"}},"required":["stadt"]}),
        |args: Value| {
            let stadt = args["stadt"].as_str().unwrap_or("");
            Ok(format!("In {stadt}: 18°C, leicht bewölkt, schwacher Wind."))
        },
    );
    reg.add(
        "reverse",
        "Dreht eine Zeichenkette um.",
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        |args: Value| {
            let t = args["text"].as_str().unwrap_or("");
            Ok(t.chars().rev().collect())
        },
    );
    reg
}

// ------------------------------------------------------------------------- Demo-LLM

/// Ein winziger, deterministischer LLM ohne Netz — für den Demo-Modus.
///
/// Er schaut auf die letzte Nachricht: liegt schon ein Tool-Ergebnis vor, fasst er
/// es zusammen; sonst sucht er in der letzten User-Nachricht nach einem passenden
/// Tool-Aufruf (Addition `a + b`, `wetter in <Stadt>`) und ruft es auf — andernfalls
/// antwortet er direkt. Dadurch ist das TUI auch ohne API-Key interaktiv.
struct DemoLlm;

impl DemoLlm {
    fn answer_chunks(text: &str) -> Vec<Chunk> {
        // Wort für Wort streamen — zeigt den Streaming-Pfad des UIs. `split_inclusive`
        // behält das trennende Leerzeichen am Wort, sodass die Stücke wieder den
        // Originaltext ergeben.
        text.split_inclusive(' ').map(Chunk::text).collect()
    }
}

impl Llm for DemoLlm {
    fn complete(&self, _messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("(komprimierte Zusammenfassung)".to_string()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        let last = messages.last();
        let last_role = last.and_then(|m| m["role"].as_str()).unwrap_or("");

        // Schon ein Tool-Ergebnis da -> finale Antwort.
        if last_role == "tool" {
            let result = last.and_then(|m| m["content"].as_str()).unwrap_or("");
            let text = format!("Ergebnis: {result}");
            return Ok(Box::new(DemoLlm::answer_chunks(&text).into_iter()));
        }

        // Letzte User-Nachricht heranziehen.
        let user = messages
            .iter()
            .rev()
            .find(|m| m["role"].as_str() == Some("user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_string();
        let lower = user.to_lowercase();

        // 1) Addition "a + b"?
        if let Some((a, b)) = parse_addition(&user) {
            let args = json!({"a": a, "b": b}).to_string();
            return Ok(Box::new(
                vec![Chunk::tool(0, "demo-add", "add", &args)].into_iter(),
            ));
        }

        // 2) Wetter?
        if lower.contains("wetter") || lower.contains("weather") {
            let stadt = parse_city(&user).unwrap_or_else(|| "Wien".to_string());
            let args = json!({"stadt": stadt}).to_string();
            return Ok(Box::new(
                vec![Chunk::tool(0, "demo-wetter", "wetter", &args)].into_iter(),
            ));
        }

        // 3) Sonst: direkte Demo-Antwort.
        let text = format!(
            "Demo-Modus (kein Netz): Ich habe »{}« erhalten. Setze einen API-Key \
             (OPENAI_API_KEY oder AZURE_OPENAI_*), um ein echtes Modell zu nutzen. \
             Probier z. B. »17 + 25« oder »Wetter in Berlin«.",
            user.trim()
        );
        Ok(Box::new(DemoLlm::answer_chunks(&text).into_iter()))
    }
}

/// Findet das erste Muster `<int> + <int>` in einem Text: den Ziffernlauf direkt
/// links bzw. rechts vom ersten `+` (Satzzeichen/Wörter drumherum werden ignoriert).
fn parse_addition(text: &str) -> Option<(i64, i64)> {
    let (left, right) = text.split_once('+')?;
    let a = left
        .trim_end()
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?;
    let b = right
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Sehr einfache Stadt-Extraktion: das Wort nach einem alleinstehenden "in".
fn parse_city(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if w.eq_ignore_ascii_case("in") {
            if let Some(next) = words.get(i + 1) {
                let city: String = next
                    .chars()
                    .filter(|c| c.is_alphabetic() || *c == '-')
                    .collect();
                if !city.is_empty() {
                    return Some(city);
                }
            }
        }
    }
    None
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
    fn new(agent: Agent, model_label: String) -> Self {
        let bus = EventBus::new();
        let events = bus.subscribe();
        let mut app = App {
            agent: Some(agent),
            model_label,
            bus,
            events,
            running: None,
            input: String::new(),
            lines: Vec::new(),
            cur_assistant: None,
            scroll: 0,
            follow: true,
            should_quit: false,
        };
        app.push(note_line(
            "Willkommen beim agentkit-TUI. Stelle eine Frage und drücke Enter.",
            Color::DarkGray,
        ));
        app
    }

    fn run(mut self, mut terminal: DefaultTerminal) -> std::io::Result<()> {
        // Nur neu zeichnen, wenn sich etwas geändert hat — im Leerlauf (der Normalfall)
        // spart das den Aufbau eines kompletten Frames pro 50-ms-Tick.
        let mut dirty = true;
        while !self.should_quit {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            // Eingaben pollen (nicht-blockierend, damit Events weiter fließen).
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
        match code {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::End => {
                self.follow = true;
            }
            KeyCode::Home => {
                self.scroll = 0;
                self.follow = false;
            }
            KeyCode::Esc => {
                if let Some(run) = &self.running {
                    // Laufenden Auftrag kooperativ abbrechen.
                    run.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                if self.running.is_none() {
                    self.input.pop();
                }
            }
            KeyCode::Char(c) => {
                if self.running.is_none() {
                    self.input.push(c);
                }
            }
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        // Nur den Offset verschieben; `draw` klemmt ans Maximum und heftet wieder
        // ans Ende, sobald man dort ankommt (setzt `follow`).
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

    /// Verarbeitet alle wartenden Events; `true`, wenn mindestens eines ankam.
    fn drain_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(ev) = self.events.try_recv() {
            self.apply_event(ev);
            any = true;
        }
        any
    }

    /// Holt den Agenten zurück, sobald der Worker-Thread fertig ist; `true` bei Übernahme.
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
        // Jeder Event-Typ außer Text-Deltas beendet eine ggf. laufende Antwort-Zeile.
        match ev.data {
            EventData::Step { step } => {
                self.cur_assistant = None;
                self.push(step_line(step));
            }
            EventData::ToolCall { name, args } => {
                self.cur_assistant = None;
                self.push(toolcall_line(&name, &args));
            }
            EventData::ToolResult { name, result } => {
                self.cur_assistant = None;
                self.push(toolresult_line(&name, &result));
            }
            EventData::TextDelta(t) => self.stream_text(&t),
            EventData::Final(t) => {
                // Kam der Text schon als Deltas, steht er bereits; sonst hier nachtragen.
                if self.cur_assistant.is_none() && !t.is_empty() {
                    self.push(assistant_line(&t));
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

    /// Hängt ein Text-Delta als Span an die laufende Assistant-Zeile an (oder beginnt
    /// eine neue). Anhängen statt Neuaufbau hält das pro Token bei O(1).
    fn stream_text(&mut self, t: &str) {
        match self.cur_assistant {
            Some(idx) => self.lines[idx].spans.push(Span::styled(
                t.to_string(),
                Style::default().fg(Color::White),
            )),
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
                Constraint::Length(3), // Eingabe
                Constraint::Length(1), // Fußzeile
            ])
            .split(f.area());

        // --- Titelzeile
        let status = if self.running.is_some() {
            Span::styled(" arbeitet… ", fg(Color::Black).bg(Color::Yellow))
        } else {
            Span::styled(" bereit ", fg(Color::Black).bg(Color::Green))
        };
        let title = Line::from(vec![
            Span::styled(" agentkit TUI ", bold(Color::White).bg(Color::Blue)),
            Span::raw(" · Modell: "),
            Span::styled(self.model_label.clone(), fg(Color::Cyan)),
            Span::raw(" · "),
            status,
        ]);
        f.render_widget(Paragraph::new(title), chunks[0]);

        // --- Transcript (scrollbar, mit Zeilenumbruch)
        let inner_w = chunks[1].width.saturating_sub(2);
        let inner_h = chunks[1].height.saturating_sub(2) as usize;
        let max_scroll = wrapped_rows(&self.lines, inner_w).saturating_sub(inner_h);
        // Offset klemmen; am Ende angekommen -> wieder ans Ende heften (Auto-Scroll).
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

        // --- Eingabezeile
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
        f.render_widget(input, chunks[2]);

        if self.running.is_none() {
            // Cursor hinter den Eingabetext setzen: linker Rand (1) + Prompt "› " (2).
            const PROMPT_W: u16 = 3;
            let cx = chunks[2].x + PROMPT_W + self.input.chars().count() as u16;
            let cy = chunks[2].y + 1;
            f.set_cursor_position((cx.min(chunks[2].x + chunks[2].width.saturating_sub(2)), cy));
        }

        // --- Fußzeile
        let footer = Line::from(vec![
            Span::styled("Enter", key_style()),
            Span::raw(" senden  "),
            Span::styled("Esc", key_style()),
            Span::raw(" abbrechen/beenden  "),
            Span::styled("Ctrl-C", key_style()),
            Span::raw(" beenden  "),
            Span::styled("↑↓/PgUp/PgDn/End", key_style()),
            Span::raw(" scrollen"),
        ])
        .style(fg(Color::DarkGray));
        f.render_widget(Paragraph::new(footer), chunks[3]);
    }
}

// ----------------------------------------------------------------- Zeilen-Helfer

/// Vordergrundfarbe als Style.
fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

/// Vordergrundfarbe + fett.
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

fn step_line(step: usize) -> Line<'static> {
    Line::styled(
        format!("── Schritt {step} ──"),
        fg(Color::DarkGray).add_modifier(Modifier::DIM),
    )
}

fn toolcall_line(name: &str, args: &Value) -> Line<'static> {
    Line::from(vec![
        Span::styled("🔧 ", fg(Color::Yellow)),
        Span::styled(name.to_string(), bold(Color::Yellow)),
        Span::styled(format!("({})", compact_json(args)), fg(Color::Yellow)),
    ])
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
    fn addition_is_parsed() {
        assert_eq!(parse_addition("Was ist 17 + 25?"), Some((17, 25)));
        assert_eq!(parse_addition("rechne 3+4"), Some((3, 4)));
        assert_eq!(parse_addition("kein plus hier"), None);
    }

    #[test]
    fn city_is_extracted() {
        assert_eq!(
            parse_city("Wie ist das Wetter in Berlin?").as_deref(),
            Some("Berlin")
        );
        assert_eq!(parse_city("Wetter heute").as_deref(), None);
    }

    /// Demo-LLM treibt einen echten Agent-Loop: Tool-Call -> Ergebnis -> Antwort.
    #[test]
    fn demo_agent_runs_tool_then_answers() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Was ist 17 + 25?");
        assert!(answer.contains("42"), "Antwort war: {answer}");
    }

    #[test]
    fn demo_agent_handles_weather() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Wie ist das Wetter in Graz?");
        assert!(
            answer.to_lowercase().contains("graz"),
            "Antwort war: {answer}"
        );
    }

    #[test]
    fn demo_agent_plain_reply_without_tool() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Hallo!");
        assert!(answer.contains("Demo-Modus"), "Antwort war: {answer}");
    }

    #[test]
    fn wrapped_rows_counts_soft_wraps() {
        let lines = vec![Line::raw("a".repeat(25))];
        assert_eq!(wrapped_rows(&lines, 10), 3); // 25 Zeichen / 10 = aufgerundet 3
    }
}
