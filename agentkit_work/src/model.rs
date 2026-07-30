//! Das Domänenmodell: Projekt, Lauf, Work Item, Lease, Versuch, Artefakt.
//!
//! Zwei Festlegungen prägen alles Weitere (siehe Plan „Entschiedene Abweichungen“):
//!
//! 1. **`Ready` und `Blocked` sind abgeleitete Sichten, kein gespeicherter Status.**
//!    Gespeichert wird nur [`WorkItemStatus`]; `state::ready_items` und
//!    `state::blocked_by` rechnen bei jeder Abfrage neu — ein gespeichertes
//!    `Ready` könnte vom Abhängigkeitsgraphen abdriften, ein abgeleitetes nicht.
//! 2. **Zeit ist ein Parameter, keine Systemuhr im Domänencode.** `now_ms()` liefert
//!    die aktuelle Zeit für Aufrufer (Tools, Runner), aber Leases und Übergänge
//!    nehmen `_ms`-Werte entgegen, damit sie ohne Sleep deterministisch testbar sind.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub type ProjectId = String;
pub type RunId = String;
pub type WorkItemId = String;
pub type AttemptId = String;
pub type ArtifactId = String;

/// Unix-Zeit in Millisekunden. Reine Anzeige-/Erzeugungsquelle — Domänenfunktionen
/// nehmen Zeit immer als Parameter entgegen (siehe Moduldoku), damit Leases und
/// Statusübergänge ohne Sleep testbar bleiben.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sortierschlüssel für IDs der Form `W-17`: numerisch, nicht lexikografisch
/// (sonst stünde `W-10` vor `W-9`). Fällt auf 0 zurück, wenn kein Suffix da ist.
pub fn id_order(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Kebab-Slug eines Titels für die Projekt-ID (`"Graceful Swarm Shutdown"` →
/// `"graceful-swarm-shutdown"`). Kollisionen (`-2`, `-3`, …) löst der Aufrufer,
/// der die Zielverzeichnisse kennt — hier gibt es nur die reine Textfunktion.
pub fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "projekt".to_string()
    } else {
        out
    }
}

/// Wie belastbar ein Projekt insgesamt ist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Active,
    Completed,
    Canceled,
}

/// Zustand eines Laufs (`WorkRun`). Ein Lauf zerfällt in Work Items; pausiert er,
/// bleibt der Fortschritt aller Items stehen — deshalb ist Pause reine
/// Buchführung ohne Kaskade auf die Items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Canceled,
}

/// Gespeicherter Status eines Work Items. `Ready` gibt es bewusst nicht (siehe
/// Moduldoku) — nur diese fünf werden je journalt.
///
/// Es gibt bewusst KEIN `Claimed` zwischen `Pending` und `Running`: bei einem
/// synchronen Ein-Prozess-Worker sind Claim und Start derselbe Moment, ein
/// eigener Zwischenzustand hätte keine beobachtbare Dauer. Ein früherer Entwurf
/// hatte `Claimed`, gekoppelt an die erste `LeaseRenewed` als Startsignal — das
/// war ein echter Bug: ein kurzer Versuch ohne jede Lease-Verlängerung (z. B.
/// ein Agent, der in einem einzigen Schritt antwortet) blieb auf `Claimed`
/// stehen, und `WorkItemCompleted` lief dann in den verbotenen Übergang
/// `Claimed → Completed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl WorkItemStatus {
    /// Die erlaubte Übergangsmatrix. `Completed` ist immer endgültig; `Failed`
    /// erlaubt den Rücksprung nach `Pending` (Retry) oder den Übergang nach
    /// `Canceled` (Lauf abgebrochen, bevor ein neuer Versuch startet).
    pub fn can_transition_to(self, next: WorkItemStatus) -> bool {
        use WorkItemStatus::*;
        matches!(
            (self, next),
            (Pending, Running)
                | (Pending, Canceled)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Pending)
                | (Running, Canceled)
                | (Failed, Pending)
                | (Failed, Canceled)
        )
    }
}

/// Art eines Work Items — bestimmt u. a. die Rollenwahl beim Ausführen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    Discovery,
    Analysis,
    Planning,
    Implementation,
    Test,
    Review,
    Documentation,
}

/// Ergebnis eines einzelnen Versuchs, ein Item zu bearbeiten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

/// Warum ein Versuch gescheitert ist. Fünf Varianten, nicht elf (§18 im
/// Originaldokument) — nur diese fünf haben im MVP einen Erzeuger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    ModelFailure,
    MaxSteps,
    InvalidOutput,
    Interrupted,
    BudgetExceeded,
}

/// Ein gescheiterter Versuch trägt seine Ursache in den nächsten Anlauf weiter
/// (§12: „vorherige Fehlerursache im nächsten Arbeitspaket“).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    pub kind: FailureKind,
    pub message: String,
}

/// Art eines Artefakts, das ein Versuch ablegt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Analysis,
    Code,
    Test,
    Documentation,
    Other,
}

/// Warum ein Lauf beendet wurde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    AllItemsDone,
    BudgetExceeded,
    Blocked,
    Canceled,
}

/// Budget eines Projekts. `max_parallel_agents` ist im MVP fix 1 (ein
/// synchroner Vordergrund-Worker) — das Feld bleibt trotzdem Teil des Modells,
/// damit ein künftiger Scheduler nicht das Datenmodell brechen muss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBudget {
    pub max_wall_time_secs: Option<u64>,
    pub max_work_items: Option<u32>,
    pub max_attempts_per_item: u32,
    pub max_steps_per_attempt: u32,
    pub max_parallel_agents: u32,
}

impl Default for WorkBudget {
    fn default() -> Self {
        WorkBudget {
            max_wall_time_secs: None,
            max_work_items: None,
            max_attempts_per_item: 3,
            max_steps_per_attempt: 40,
            max_parallel_agents: 1,
        }
    }
}

/// Ein Vorhaben — die Wurzel, unter der Läufe und Items liegen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProject {
    pub id: ProjectId,
    pub title: String,
    pub objective: String,
    pub workspace: String,
    pub status: ProjectStatus,
    pub created_at_ms: u64,
    pub budget: WorkBudget,
}

/// Ein Lauf eines Projekts. `base_revision` hält fest, auf welchem Stand des
/// Workspace (z. B. Git-Revision) der Lauf gestartet wurde — reine Anzeige im
/// MVP, ohne Worktree-Isolation (Phase 7, nicht im Umfang).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRun {
    pub id: RunId,
    pub project_id: ProjectId,
    pub status: RunStatus,
    pub started_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub base_revision: Option<String>,
    pub completion_reason: Option<CompletionReason>,
}

/// Eine Teilaufgabe. `seq` ist die Erzeugungsreihenfolge (numerisch, für
/// stabile Sortierung) — unabhängig von `id`, die als Anzeige-String dient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub run_id: RunId,
    pub title: String,
    pub description: String,
    pub kind: WorkItemKind,
    pub status: WorkItemStatus,
    pub priority: u8,
    pub seq: u64,
    /// Name einer agentkit `AgentRole` — welche Rolle diesen Versuch ausführen soll.
    pub required_role: Option<String>,
    pub dependencies: Vec<WorkItemId>,
    pub acceptance_criteria: Vec<String>,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub updated_at_ms: u64,
}

/// Der exklusive Anspruch eines Agenten auf ein Item, solange das Lease läuft.
/// Läuft es ab, gilt der Agent als tot — `state::expired_leases` findet genau das.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkLease {
    pub work_item_id: WorkItemId,
    pub agent_id: String,
    pub attempt_id: AttemptId,
    pub claimed_at_ms: u64,
    pub expires_at_ms: u64,
    pub last_heartbeat_ms: u64,
}

/// Ein einzelner Anlauf, ein Item zu bearbeiten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAttempt {
    pub id: AttemptId,
    pub work_item_id: WorkItemId,
    pub agent_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: AttemptStatus,
    pub summary: Option<String>,
    pub failure: Option<FailureInfo>,
    pub steps: u32,
    pub tool_calls: u32,
    /// IDs der über `work_claim` festgehaltenen Aussagen (Phase 4, §11/§14).
    /// Wächst nur — `state::apply` HÄNGT bei `ClaimsRecorded` an, weil ein
    /// Versuch `work_claim` mehrfach aufrufen darf, jeder Aufruf journalt
    /// aber ein eigenes Ereignis mit genau seinen neuen IDs.
    #[serde(default)]
    pub claim_ids: Vec<String>,
}

/// Ein Artefakt, das ein Versuch abgelegt hat. Liegt bewusst im Workspace
/// (`<workspace>/.agentkit/work/<project-id>/artifacts/<item>/<versuch>/<datei>`),
/// damit der nächste Agent es mit dem vorhandenen `read_file`-Tool erreicht.
/// Der Versuch steckt mit im Pfad, nicht nur das Item — sonst würde ein
/// Wiederholungsversuch beim erneuten Ablegen desselben Dateinamens auf die
/// Datei seines Vorgängers treffen (siehe `tools::resolve_artifact_path`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkArtifact {
    pub id: ArtifactId,
    pub work_item_id: WorkItemId,
    pub attempt_id: AttemptId,
    pub kind: ArtifactKind,
    pub rel_path: String,
    pub summary: String,
    pub created_at_ms: u64,
}

impl fmt::Display for WorkItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WorkItemStatus::Pending => "pending",
            WorkItemStatus::Running => "running",
            WorkItemStatus::Completed => "completed",
            WorkItemStatus::Failed => "failed",
            WorkItemStatus::Canceled => "canceled",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalisiert_und_kollabiert_trenner() {
        assert_eq!(slug("Graceful Swarm Shutdown"), "graceful-swarm-shutdown");
        assert_eq!(slug("  A -- B  "), "a-b");
        assert_eq!(slug("Größe & Öko"), "gr-e-ko");
        assert_eq!(slug("---"), "projekt");
        assert_eq!(slug(""), "projekt");
    }

    #[test]
    fn id_order_sortiert_numerisch() {
        let mut ids = vec!["W-10".to_string(), "W-9".to_string(), "W-2".to_string()];
        ids.sort_by_key(|i| id_order(i));
        assert_eq!(ids, vec!["W-2", "W-9", "W-10"]);
    }

    #[test]
    fn erlaubte_und_verbotene_uebergaenge() {
        use WorkItemStatus::*;
        assert!(Pending.can_transition_to(Running));
        assert!(Running.can_transition_to(Completed));
        assert!(Failed.can_transition_to(Pending));
        assert!(Failed.can_transition_to(Canceled));

        assert!(!Completed.can_transition_to(Pending));
        assert!(!Completed.can_transition_to(Running));
        assert!(!Canceled.can_transition_to(Pending));
        assert!(!Pending.can_transition_to(Completed));
    }
}
