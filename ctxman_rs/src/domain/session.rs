use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::enums::SessionStatus;
use super::policy::PolicyConfig;

/// Repräsentiert den Context eines Agent-Laufs (Spec §2.1). Die Session ist Träger der
/// optimistic-concurrency-Version (`context_version`), der Static-Epoche (`static_epoch`,
/// siehe I1 / §4.2) und des aktuellen Turns. Die effektive Policy wird bei der Erstellung als
/// Snapshot eingefroren (Reproduzierbarkeit, §5).
///
/// Die Frames der Session (Spec §2.1: `frames: Stack<Frame>`) hält im Rust-Port die
/// `ContextSession` — Stack-Disziplin (LIFO, §2.5) wird dort erzwungen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    id: Ulid,
    agent_template_id: Option<String>,
    policy: PolicyConfig,
    context_version: u64,
    static_epoch: u32,
    current_turn: u32,
    status: SessionStatus,
    created_at: i64,
    updated_at: i64,
}

impl Session {
    /// Neue aktive Session mit eingefrorener Policy (Spec §2.1/§5); `context_version` startet
    /// bei 0, `static_epoch` und `current_turn` bei 0 (wie der C#-Session-Endpunkt).
    pub fn new(id: Ulid, agent_template_id: Option<String>, policy: PolicyConfig, now: i64) -> Self {
        Session {
            id,
            agent_template_id,
            policy,
            context_version: 0,
            static_epoch: 0,
            current_turn: 0,
            status: SessionStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    /// Stellt eine Session aus vollständigem Zustand wieder her (Snapshot-Load; Ersatz für den
    /// vollständigen C#-Konstruktor).
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        id: Ulid,
        agent_template_id: Option<String>,
        policy: PolicyConfig,
        context_version: u64,
        static_epoch: u32,
        current_turn: u32,
        status: SessionStatus,
        created_at: i64,
        updated_at: i64,
    ) -> Self {
        Session {
            id,
            agent_template_id,
            policy,
            context_version,
            static_epoch,
            current_turn,
            status,
            created_at,
            updated_at,
        }
    }

    /// ULID der Session (Spec §2.1).
    pub fn id(&self) -> Ulid {
        self.id
    }

    /// Referenziert die Policy-Konfiguration; `None` im Standalone-Setup (Spec §2.1).
    pub fn agent_template_id(&self) -> Option<&str> {
        self.agent_template_id.as_deref()
    }

    /// Aufgelöste, effektive Policy — Snapshot bei Erstellung (Spec §2.1 / §5).
    pub fn policy(&self) -> &PolicyConfig {
        &self.policy
    }

    /// Monoton, optimistic concurrency (Spec §2.1 / §4.4).
    pub fn context_version(&self) -> u64 {
        self.context_version
    }

    /// Version der Static-Region (Spec §2.1, siehe I1 und §4.2).
    pub fn static_epoch(&self) -> u32 {
        self.static_epoch
    }

    /// Aktueller Turn (Spec §2.1); ein Turn = ein Model-Call (§3).
    pub fn current_turn(&self) -> u32 {
        self.current_turn
    }

    /// active | archived (Spec §2.1).
    pub fn status(&self) -> SessionStatus {
        self.status
    }

    /// Erstellungszeitpunkt in Unix-Millis (Spec §2.1).
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    /// Letzter Änderungszeitpunkt in Unix-Millis (Spec §2.1).
    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }

    /// Erhöht `context_version` um 1 und setzt `updated_at`. Einziger Ort, der die Monotonie
    /// der context_version garantiert (Spec §4.4).
    pub fn increment_version(&mut self, now: i64) {
        self.context_version += 1; // Spec §4.4
        self.updated_at = now;
    }

    /// Erhöht `current_turn` um 1 und setzt `updated_at`. Einziger Ort, der die Monotonie des
    /// current_turn garantiert (Spec §4.4).
    pub fn advance_turn(&mut self, now: i64) {
        self.current_turn += 1; // Spec §4.4
        self.updated_at = now;
    }

    /// Erhöht `static_epoch` um 1 und setzt `updated_at` (Spec §4.2, siehe I1). Erhöht bewusst
    /// nicht die context_version — der Aufrufer ruft dafür separat `increment_version`.
    pub fn bump_static_epoch(&mut self, now: i64) {
        self.static_epoch += 1; // Spec §4.2
        self.updated_at = now;
    }

    /// Setzt den Status auf `archived` und aktualisiert `updated_at` (Spec §4.3).
    pub fn archive(&mut self, now: i64) {
        self.status = SessionStatus::Archived; // Spec §4.3
        self.updated_at = now;
    }
}
