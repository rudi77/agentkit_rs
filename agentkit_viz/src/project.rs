//! Projektionen: aus dem flachen Ereignisstrom die fünf Sichten bauen.
//!
//! Hexagonal — dieses Modul kennt weder HTTP noch Dateisystem. Es bekommt
//! [`TraceEvent`]s und liefert serde-Strukturen; die Adapter (`trace.rs` außen
//! zum Dateisystem, `server.rs` außen zum Browser) hängen daran.
//!
//! **Alles im Speicher.** [`TraceState`] hält den kompletten Strom. Das ist für
//! ein Debug-Werkzeug die richtige Wahl: ein Trace ist so groß wie ein Lauf, und
//! jede Projektion braucht ohnehin den ganzen Verlauf. Sollte das je nicht mehr
//! reichen, ist ein Index über Offsets der dokumentierte nächste Schritt.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};

use crate::model::{agent_label, AgentKind, TraceData, TraceEvent};

/// `kind` des Kontext-Datensatzes, den die CLI nach jedem Zug schreibt.
///
/// Ein Vertrag über die Datei, kein geteilter Typ — der Betrachter hängt
/// bewusst nicht am agentkit-Crate (siehe README). Läuft der Name auseinander,
/// zeigt der Betrachter für den Haupt-Agenten `rekonstruiert: true` statt einen
/// Fehler; das ist die sichtbare Folge, an der man es merkt.
pub const CONTEXT_SNAPSHOT_KIND: &str = "context_snapshot";

/// Der gelesene Trace, aus dem alle Sichten entstehen.
#[derive(Default)]
pub struct TraceState {
    events: Vec<TraceEvent>,
}

/// Ein Agent, wie er im Ereignisstrom erscheint.
#[derive(Debug, Clone, Serialize)]
pub struct AgentView {
    /// Das `source`-Tag; leer für den Haupt-Agenten.
    pub id: String,
    pub label: String,
    pub kind: AgentKind,
    pub events: usize,
    pub steps: usize,
    pub tool_calls: usize,
    pub errors: usize,
    pub first_at_ms: u64,
    pub last_at_ms: u64,
    /// „läuft", „fertig", „abgebrochen" oder „fehler" — siehe [`status_of`].
    pub status: &'static str,
}

/// Ein Eintrag der Zeitleiste: das Wesentliche einer Zeile in einem Satz.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub seq: u64,
    pub at_ms: u64,
    pub source: String,
    pub etype: String,
    pub label: String,
}

/// „Was steht im Kontext dieses Agenten."
#[derive(Debug, Clone, Serialize)]
pub struct ContextView {
    pub agent: String,
    /// Die Nachrichten des Agenten, aus den `context_snapshot`-Datensätzen
    /// zusammengesetzt (bzw. rekonstruiert, siehe `rekonstruiert`).
    pub messages: Vec<Value>,
    /// Der `ContextReport` des letzten Datensatzes (Segmente, Tokens, Budget) —
    /// `null` für rekonstruierte Kontexte.
    pub report: Option<Value>,
    /// true ⇔ es gibt keine `context_snapshot`-Datensätze für diesen Agenten,
    /// die Nachrichten sind also aus dem Ereignisstrom REKONSTRUIERT.
    ///
    /// Das ist die dokumentierte Grenze aus dem Plan: auf die `memory` von
    /// Sub-Agenten und Schwarm-Mitgliedern hat der schreibende Prozess keinen
    /// Zugriff. Die Rekonstruktion zeigt, was der Agent GETAN hat, nicht seinen
    /// System-Prompt und nicht, was eine Verdichtung aus ihm gemacht hat.
    pub rekonstruiert: bool,
    /// true ⇔ ein Datensatz setzte HINTER dem an, was der Betrachter hat
    /// (`messages_from` > bisherige Anzahl). Dann fehlt ein Stück in der Mitte:
    /// die gezeigten Nachrichten stimmen, ihre POSITIONEN nicht mehr. Sichtbar
    /// machen statt glattbügeln — eine unbemerkte Lücke im Kontext ist genau
    /// der Befund, für den es dieses Werkzeug gibt.
    pub unvollstaendig: bool,
}

impl TraceState {
    pub fn new() -> TraceState {
        TraceState::default()
    }

    pub fn extend(&mut self, events: impl IntoIterator<Item = TraceEvent>) {
        self.events.extend(events);
    }

    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// Höchste gesehene Sequenznummer — der Stand, ab dem ein Browser weiterfragt.
    pub fn last_seq(&self) -> u64 {
        self.events.last().map(|e| e.seq).unwrap_or(0)
    }

    /// Ereignisse ab (ausschließlich) einer Sequenznummer — die Naht für das
    /// Nachladen im Browser.
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &TraceEvent> {
        self.events.iter().filter(move |e| e.seq > seq)
    }

    /// Die Agenten des Laufs, gruppiert über `source`. Haupt-Agent zuerst,
    /// danach alphabetisch — eine stabile Reihenfolge ist wichtiger als eine
    /// nach Aktivität sortierte, die bei jedem Nachladen springt.
    pub fn agents(&self) -> Vec<AgentView> {
        let mut nach_id: BTreeMap<&str, AgentView> = BTreeMap::new();
        for ev in &self.events {
            let eintrag = nach_id.entry(&ev.source).or_insert_with(|| AgentView {
                id: ev.source.clone(),
                label: agent_label(&ev.source).to_string(),
                kind: AgentKind::of(&ev.source),
                events: 0,
                steps: 0,
                tool_calls: 0,
                errors: 0,
                first_at_ms: ev.at_ms,
                last_at_ms: ev.at_ms,
                status: "läuft",
            });
            eintrag.events += 1;
            eintrag.last_at_ms = ev.at_ms;
            match &ev.data {
                TraceData::Step { .. } => eintrag.steps += 1,
                TraceData::ToolCall { .. } => eintrag.tool_calls += 1,
                TraceData::Error { .. } => eintrag.errors += 1,
                _ => {}
            }
            if let Some(status) = status_of(&ev.data) {
                eintrag.status = status;
            }
        }
        let mut out: Vec<AgentView> = nach_id.into_values().collect();
        out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.id.cmp(&b.id)));
        out
    }

    /// Der Verlauf EINES Agenten in Reihenfolge — die Ereignisse mit diesem
    /// `source`-Tag, jeweils mit `label` angereichert.
    ///
    /// Das Label kommt aus [`label_of`], damit die Kopfzeile eines Ereignisses
    /// GENAU EINMAL formuliert ist: die Zeitleiste und der Verlauf zeigen
    /// dieselben Worte, und ein neuer Ereignistyp braucht keine zweite,
    /// mitzupflegende Übersetzung im Browser.
    pub fn history(&self, agent: &str) -> Vec<Value> {
        self.events
            .iter()
            .filter(|e| e.source == agent)
            .map(with_label)
            .collect()
    }

    /// Die Zeitleiste des ganzen Laufs, eine Zeile je Ereignis.
    pub fn timeline(&self) -> Vec<TimelineEntry> {
        self.events
            .iter()
            .map(|e| TimelineEntry {
                seq: e.seq,
                at_ms: e.at_ms,
                source: e.source.clone(),
                etype: e.etype.clone(),
                label: label_of(&e.data),
            })
            .collect()
    }

    /// „Was steht im Kontext dieses Agenten."
    ///
    /// Für den Haupt-Agenten aus den `context_snapshot`-Datensätzen
    /// zusammengesetzt: jeder trägt `messages_from` (ab welchem Index seine
    /// Nachrichten gelten) und die Nachrichten selbst. `messages_from = 0`
    /// ersetzt den bisherigen Stand vollständig — so schreibt die CLI es nach
    /// einer Verdichtung oder bei einem frischen Agenten.
    ///
    /// Für alle anderen wird der Kontext aus dem Ereignisstrom REKONSTRUIERT
    /// (siehe [`ContextView::rekonstruiert`]).
    pub fn context(&self, agent: &str) -> ContextView {
        let mut messages: Vec<Value> = Vec::new();
        let mut report: Option<Value> = None;
        let mut gesehen = false;
        let mut unvollstaendig = false;
        for ev in self.events.iter().filter(|e| e.source == agent) {
            let Some(payload) = ev.structured_of(CONTEXT_SNAPSHOT_KIND) else {
                continue;
            };
            gesehen = true;
            let ab = payload
                .get("messages_from")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            // Setzt der Datensatz HINTER dem an, was hier steht, fehlt ein
            // Stück (eine verlorene Zeile im Trace). Angehängt wird trotzdem —
            // die Ereignisse sind echt —, aber die Ansicht sagt es dann.
            if ab > messages.len() {
                unvollstaendig = true;
            }
            messages.truncate(ab.min(messages.len()));
            if let Some(neu) = payload.get("messages").and_then(Value::as_array) {
                messages.extend(neu.iter().cloned());
            }
            report = payload.get("report").cloned();
        }
        if gesehen {
            return ContextView {
                agent: agent.to_string(),
                messages,
                report,
                rekonstruiert: false,
                unvollstaendig,
            };
        }
        ContextView {
            agent: agent.to_string(),
            messages: self.rekonstruiere(agent),
            report: None,
            rekonstruiert: true,
            unvollstaendig: false,
        }
    }

    /// Nachrichten eines Agenten aus dem Ereignisstrom nachbauen — so weit, wie
    /// der Strom es hergibt: Tool-Aufrufe, Tool-Ergebnisse und die finale
    /// Antwort. System-Prompt und User-Nachricht stehen dort nicht.
    fn rekonstruiere(&self, agent: &str) -> Vec<Value> {
        let mut out = Vec::new();
        for ev in self.events.iter().filter(|e| e.source == agent) {
            match &ev.data {
                // Mit der Korrelations-ID wird die Rekonstruktion genauer: der
                // Agent legt seine Tool-Antworten mit `tool_call_id` ab, und
                // erst damit ist auch hier ablesbar, welches Ergebnis zu
                // welchem Aufruf gehört. Leer bei Traces von vor der ID.
                TraceData::ToolCall { name, args } => out.push(json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": ev.call_id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}],
                })),
                TraceData::ToolResult { name, result } => out.push(json!({
                    "role": "tool",
                    "tool_call_id": ev.call_id,
                    "name": name,
                    "content": result,
                })),
                TraceData::Final(text) if !text.is_empty() => out.push(json!({
                    "role": "assistant",
                    "content": text,
                })),
                _ => {}
            }
        }
        out
    }
}

/// Ein Ereignis als JSON, um sein `label` ergänzt — die Form, in der Verlauf
/// (`/api/agents/<id>/history`) und Nachschub (`/api/events`) es ausliefern.
/// Beide MÜSSEN dieselbe Form haben: der Browser hängt den Nachschub direkt an
/// die Verlaufsliste an, statt sie neu zu bauen.
pub fn with_label(event: &TraceEvent) -> Value {
    let mut wert = serde_json::to_value(event).unwrap_or_else(|_| json!({}));
    wert["label"] = json!(label_of(&event.data));
    wert
}

/// Ereignisse, die den Zustand eines Agenten festlegen. `None` = ändert nichts.
fn status_of(data: &TraceData) -> Option<&'static str> {
    match data {
        TraceData::Done => Some("fertig"),
        TraceData::Cancelled { .. } => Some("abgebrochen"),
        TraceData::Error { .. } => Some("fehler"),
        _ => None,
    }
}

/// Eine Zeile Klartext für ein Ereignis — für die Zeitleiste.
pub fn label_of(data: &TraceData) -> String {
    match data {
        TraceData::Step { step } => format!("Schritt {step}"),
        TraceData::TextDelta(t) => kurz(t, 80),
        TraceData::ToolCall { name, args } => format!("{name}({})", kurz(&args.to_string(), 60)),
        TraceData::ToolResult { name, result } => format!("{name} → {}", kurz(result, 80)),
        TraceData::Error { name, error } => match name {
            Some(n) => format!("Fehler in {n}: {}", kurz(error, 80)),
            None => format!("Fehler: {}", kurz(error, 80)),
        },
        TraceData::Plan(steps) => format!("Plan mit {} Schritt(en)", steps.len()),
        TraceData::Final(t) => kurz(t, 100),
        TraceData::Cancelled { where_ } => format!("abgebrochen ({where_})"),
        TraceData::Structured { kind, .. } => kind.clone(),
        TraceData::Done => "fertig".to_string(),
        TraceData::None => String::new(),
        TraceData::Unbekannt(_) => "(unbekannter Ereignistyp)".to_string(),
    }
}

/// Auf eine Zeile bringen und kürzen — die Zeitleiste hat eine Zeile je Eintrag.
///
/// Gleicht `agentkit::one_line`, ist aber bewusst eigener Code: acht Zeilen
/// sind der Preis dafür, dass dieses Crate NICHT am agentkit-Crate hängt (siehe
/// README, „Bewusste Design-Entscheidungen") — die Naht zum Agenten ist die
/// Datei, nicht ein gemeinsamer Typ.
fn kurz(s: &str, max: usize) -> String {
    let flach: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if flach.chars().count() > max {
        format!("{}…", flach.chars().take(max).collect::<String>())
    } else {
        flach
    }
}
