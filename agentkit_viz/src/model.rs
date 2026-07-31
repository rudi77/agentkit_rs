//! Spiegeltypen des Trace-Formats (`agentkit::trace`).
//!
//! **Warum eigene Typen statt der aus `agentkit`?** Der Kern leitet nur
//! `Serialize` ab: `AgentEvent::etype` ist ein `&'static str` und wäre gar
//! nicht deserialisierbar. Hier stehen deshalb `String`-Varianten derselben
//! Form — der Betrachter bleibt damit auch unabhängig von der Version, mit der
//! ein Trace geschrieben wurde.
//!
//! **Vorwärtskompatibel.** Ein neuerer agentkit kann eine `EventData`-Variante
//! ergänzen, die dieser Betrachter nicht kennt. Deshalb wird die Nutzlast erst
//! als [`serde_json::Value`] gelesen und dann gedeutet; misslingt das, bleibt
//! sie als [`TraceData::Unbekannt`] erhalten statt die ganze Zeile zu
//! verwerfen.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Zeilenformat, das dieser Betrachter versteht (`agentkit::trace::SCHEMA_VERSION`).
pub const SCHEMA_VERSION: &str = "1";

/// Eine rohe Trace-Zeile, so wie sie in der Datei steht.
#[derive(Debug, Clone, Deserialize)]
pub struct TraceLine {
    pub schema_version: String,
    pub seq: u64,
    pub at_ms: u64,
    #[serde(default)]
    pub task_id: i64,
    #[serde(default)]
    pub source: String,
    pub etype: String,
    pub data: Value,
}

/// Ein gelesenes Ereignis: die Kopfdaten der Zeile plus die gedeutete Nutzlast.
#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    pub seq: u64,
    pub at_ms: u64,
    pub task_id: i64,
    /// Leer = Haupt-Agent; sonst das Label des Sub-Agenten bzw. die ID des
    /// Schwarm-Mitglieds (`AgentEvent::source`).
    pub source: String,
    pub etype: String,
    pub data: TraceData,
}

/// Die Nutzlast — Spiegel von `agentkit::EventData`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceData {
    Step {
        step: usize,
    },
    TextDelta(String),
    ToolCall {
        name: String,
        args: Value,
    },
    ToolResult {
        name: String,
        result: String,
    },
    Error {
        name: Option<String>,
        error: String,
    },
    Plan(Vec<PlanStep>),
    Final(String),
    Cancelled {
        where_: String,
    },
    Structured {
        kind: String,
        payload: Value,
    },
    Done,
    None,
    /// Eine Variante, die dieser Betrachter nicht kennt (neuerer agentkit) —
    /// die Rohform bleibt erhalten. Wird nie deserialisiert, sondern von
    /// [`TraceEvent::from_line`] als Rückfallebene gesetzt.
    #[serde(skip_deserializing)]
    Unbekannt(Value),
}

/// Spiegel von `agentkit::Step` (ein Schritt der mitgeführten Todo-Liste).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}

impl TraceEvent {
    /// Deutet eine gelesene Zeile. Eine unbekannte Nutzlast wird zu
    /// [`TraceData::Unbekannt`], nicht zu einem Fehler — ein Betrachter, der an
    /// einem neuen Ereignistyp scheitert, wäre genau dann nutzlos, wenn man ihn
    /// braucht.
    pub fn from_line(line: TraceLine) -> TraceEvent {
        let data = match TraceData::deserialize(&line.data) {
            Ok(data) => data,
            Err(_) => TraceData::Unbekannt(line.data),
        };
        TraceEvent {
            seq: line.seq,
            at_ms: line.at_ms,
            task_id: line.task_id,
            source: line.source,
            etype: line.etype,
            data,
        }
    }

    /// Die Nutzlast, falls dies ein strukturierter Datensatz genau dieses
    /// `kind` ist (`context_snapshot`, `swarm_event`, `swarm_result`, …).
    pub fn structured_of(&self, kind: &str) -> Option<&Value> {
        match &self.data {
            TraceData::Structured { kind: k, payload } if k == kind => Some(payload),
            _ => None,
        }
    }
}

/// Wie ein Agent im Ereignisstrom entstanden ist. Aus dem `source`-Tag
/// abgeleitet — eine ehrliche Heuristik, keine übermittelte Wahrheit: der Bus
/// trägt nur einen String, und die Herkunft steckt allein in dessen Form.
/// Die Reihenfolge der Varianten IST die Anzeige-Reihenfolge (`Ord`) — der
/// Haupt-Agent oben, darunter seine Sub-Agenten, dann die Schwarm-Mitglieder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Leeres `source` — der Agent, den der Nutzer gestartet hat.
    Haupt,
    /// `<rolle>:<auftrag>` — ein über das `task`-Tool delegierter Sub-Agent.
    SubAgent,
    /// Alles andere: die ID eines Schwarm-Mitglieds.
    SchwarmMitglied,
}

impl AgentKind {
    pub fn of(source: &str) -> AgentKind {
        if source.is_empty() {
            AgentKind::Haupt
        } else if source.contains(':') {
            AgentKind::SubAgent
        } else {
            AgentKind::SchwarmMitglied
        }
    }
}

/// Anzeigename eines Agenten (leeres `source` ist der Haupt-Agent).
pub fn agent_label(source: &str) -> &str {
    if source.is_empty() {
        "(Haupt-Agent)"
    } else {
        source
    }
}
