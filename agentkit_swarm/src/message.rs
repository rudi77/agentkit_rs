//! Nachrichtenmodell des Schwarms — getrennt in fachliche [`SwarmMessage`]s
//! (was Agenten einander sagen) und interne [`AgentCommand`]s (was die Laufzeit
//! einem Actor befiehlt). Die Trennung ist bewusst: ein `Shutdown` ist keine
//! Nachricht, die ein LLM je sehen sollte.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Kennung eines Schwarm-Agenten. Bewusst ein schlichter `String` (kein Newtype):
/// die IDs kommen aus dem Builder und werden nur verglichen/angezeigt.
pub type AgentId = String;

/// Absenderkennung der initialen Aufgabe — die Laufzeit selbst, kein Agent.
pub const RUNTIME_SENDER: &str = "runtime";

/// Empfänger einer [`SwarmMessage`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    /// Genau ein Agent.
    Agent(AgentId),
    /// Alle erreichbaren Peers des Absenders (jeder erhält eine Kopie).
    Broadcast,
}

/// Fachliche Art einer Nachricht — Teil der Zusammenarbeit, nicht der Laufzeit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Task,
    Request,
    Reply,
    Observation,
    Information,
    Proposal,
    Critique,
    Vote,
    Completion,
}

impl MessageKind {
    /// Anzeigename (identisch zur serde-Repräsentation) — für Prompt-Rendering
    /// und Status-JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageKind::Task => "task",
            MessageKind::Request => "request",
            MessageKind::Reply => "reply",
            MessageKind::Observation => "observation",
            MessageKind::Information => "information",
            MessageKind::Proposal => "proposal",
            MessageKind::Critique => "critique",
            MessageKind::Vote => "vote",
            MessageKind::Completion => "completion",
        }
    }
}

/// Eine fachliche Nachricht zwischen Schwarm-Agenten.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SwarmMessage {
    pub id: String,
    pub swarm_id: String,
    pub from: AgentId,
    pub to: Recipient,
    pub kind: MessageKind,
    pub content: String,
    /// ID der Nachricht, auf die geantwortet wird.
    pub reply_to: Option<String>,
    /// Fachlicher Faden (z. B. Proposal-ID bei Votes); wird bei Replies vererbt.
    pub correlation_id: Option<String>,
    /// Unix-Zeit in Millisekunden.
    pub created_at: u64,
    /// Länge der Reply-Kette — die Bremse gegen endloses Ping-Pong (`max_hops`).
    pub hop_count: u32,
}

impl SwarmMessage {
    /// Rendert die Nachricht als Auftragstext für [`agentkit::Agent::run_on_bus`].
    /// Der Agent sieht sie als ganz normale User-Nachricht — so bleibt seine
    /// Memory über mehrere Schwarm-Nachrichten hinweg erhalten, ohne dass der
    /// Agent-Loop den Schwarm kennen muss.
    pub fn render_prompt(&self) -> String {
        let mut out = String::from("[SWARM MESSAGE]\n");
        out.push_str(&format!("Nachricht-ID: {}\n", self.id));
        out.push_str(&format!("Von: {}\n", self.from));
        out.push_str(&format!("Art: {}\n", self.kind.as_str()));
        if let Some(reply_to) = &self.reply_to {
            out.push_str(&format!("Antwort auf: {reply_to}\n"));
        }
        if let Some(corr) = &self.correlation_id {
            out.push_str(&format!("Korrelation: {corr}\n"));
        }
        out.push('\n');
        out.push_str(&self.content);
        // Der Kickoff kommt von der Laufzeit, nicht von einem Agenten: `runtime`
        // steht in keiner PeerDirectory, ein `swarm_reply` darauf scheitert
        // zwangsläufig. Der pauschale Reply-Hinweis hätte das Mitglied also
        // ausgerechnet bei seiner ersten Nachricht in den einen Sendepfad
        // geführt, der hier nicht funktionieren KANN.
        if self.from == RUNTIME_SENDER {
            out.push_str(
                "\n\nHinweis: Das ist die Initialaufgabe des Schwarms — sie kommt von der \
                 Laufzeit, es gibt also keinen Absender zum Antworten. Verteile Teilaufgaben \
                 mit `swarm_send` bzw. `swarm_broadcast` an deine Nachbarn (`swarm_peers` \
                 listet sie) und schließe am Ende mit `swarm_propose` ab.",
            );
        } else {
            out.push_str(
                "\n\nHinweis: Mit `swarm_reply` antwortest du dem Absender; \
                 mit `swarm_send` erreichst du andere Agenten (`swarm_peers` listet sie).",
            );
        }
        out
    }
}

/// Aktuelle Unix-Zeit in Millisekunden (für `created_at`).
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Internes Kommando an einen Actor — Laufzeit-Ebene, für das LLM unsichtbar.
#[derive(Debug)]
pub enum AgentCommand {
    /// Fachliche Nachricht zustellen (der Actor verarbeitet sie als einen Turn).
    Deliver(SwarmMessage),
    /// Den gerade laufenden Turn kooperativ abbrechen (der Actor lebt weiter).
    CancelCurrentTurn,
    /// Actor beenden; verbleibende Mailbox-Nachrichten werden zu Dead Letters.
    Shutdown,
}

/// Ergebnis eines (nicht blockierenden) Zustellversuchs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryResult {
    Delivered,
    MailboxFull,
    RecipientUnavailable,
    NotAllowed,
    /// Das globale Nachrichtenlimit (`max_messages`) ist erschöpft.
    LimitReached,
    /// Der Schwarm ist bereits abgeschlossen (Konsens, Limit, Abbruch) und nimmt
    /// keine fachlichen Nachrichten mehr an.
    ///
    /// KEIN Fehler: ein Mitglied, das seinen Turn zu Ende bringt, während der
    /// Konsens schon steht, hat nichts falsch gemacht. Deshalb auch kein Dead
    /// Letter — die Nachricht ist erwartbar gegenstandslos, nicht verloren.
    SwarmCompleted,
}

impl DeliveryResult {
    /// Deutscher Statuswert für das Tool-Ergebnis-JSON.
    pub fn status_str(&self) -> &'static str {
        match self {
            DeliveryResult::Delivered => "zugestellt",
            DeliveryResult::MailboxFull => "postfach_voll",
            DeliveryResult::RecipientUnavailable => "empfaenger_weg",
            DeliveryResult::NotAllowed => "nicht_erlaubt",
            DeliveryResult::LimitReached => "limit_erreicht",
            DeliveryResult::SwarmCompleted => "schwarm_beendet",
        }
    }

    /// Lohnt ein späterer neuer Versuch? (Für das Status-JSON an das Modell.)
    pub fn retryable(&self) -> bool {
        matches!(self, DeliveryResult::MailboxFull)
    }
}
