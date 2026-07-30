//! `WorkEvent` — der einzige Mutator des Zustands.
//!
//! Jede Zeile im Journal ist eines dieser 16 Ereignisse (siehe Plan
//! „Journal-Format“). Es gibt bewusst **kein** `WorkItemReady` (Readiness ist
//! eine abgeleitete Sicht, `state::ready_items`) und **kein** eigenes
//! `WorkItemStarted`/`WorkItemCanceled`:
//!
//! - `WorkItemClaimed` schaltet das Item direkt auf `Running` — es gibt kein
//!   `Claimed` mehr dazwischen (siehe `WorkItemStatus`-Moduldoku in `model.rs`
//!   für den Bug, den das behebt). `LeaseRenewed` ist danach reine
//!   Lease-Verlängerung ohne Statuswirkung.
//! - Der Wechsel eines Items nach `Canceled` hängt an `RunCanceled`: Abbruch ist
//!   im MVP immer laufweit (§28 Umfang), daher kaskadiert ein Ereignis auf alle
//!   nicht-terminalen Items des Laufs statt eines Ereignisses je Item.
//! - `ProjectCreated` gehört einem Vorhaben genau einmal — ein zweites lehnt
//!   `state::apply` ab (siehe dort). Ein Budget-Wechsel läuft deshalb über das
//!   eigene `BudgetUpdated`, nicht über ein zweites `ProjectCreated`.

use serde::{Deserialize, Serialize};

use crate::model::{
    AttemptId, AttemptStatus, CompletionReason, FailureInfo, RunId, WorkArtifact, WorkBudget,
    WorkItem, WorkItemId, WorkProject, WorkRun,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkEvent {
    /// Ein neues Vorhaben wurde angelegt.
    ProjectCreated { project: WorkProject },
    /// Das Budget des Vorhabens wurde geändert — der Weg, einen wegen
    /// `BudgetExceeded` pausierten Lauf fortsetzbar zu machen. Bewusst ein eigenes
    /// Ereignis und kein zweites `ProjectCreated`: das Journal ist die Historie des
    /// Vorhabens und darf nicht behaupten, es sei zweimal angelegt worden.
    BudgetUpdated { budget: WorkBudget, at_ms: u64 },
    /// Ein neuer Lauf des Projekts hat begonnen.
    RunStarted { run: WorkRun },
    /// Ein neues Work Item wurde eingeplant (Planungspfad oder `--items`-Datei).
    WorkItemCreated { item: WorkItem },
    /// Ein Agent hat ein Item beansprucht: legt Attempt und Lease an, Item
    /// wechselt `Pending` → `Running` (Claim und Start fallen bei einem
    /// synchronen Ein-Prozess-Worker zusammen, siehe Moduldoku).
    WorkItemClaimed {
        item: WorkItemId,
        agent: String,
        attempt: AttemptId,
        lease_expires_ms: u64,
        at_ms: u64,
    },
    /// Das Lease wurde verlängert (Heartbeat aus dem `AgentEvent`-Callback).
    /// Reine Fristverlängerung — keine Statuswirkung auf das Item.
    LeaseRenewed {
        item: WorkItemId,
        attempt: AttemptId,
        lease_expires_ms: u64,
        at_ms: u64,
    },
    /// Ein Versuch hat ein Artefakt abgelegt.
    ArtifactCreated { artifact: WorkArtifact },
    /// Ein Versuch ist zu Ende — Ergebnis am Attempt selbst, unabhängig davon,
    /// ob das Item danach als `Completed` oder `Failed` gilt.
    AttemptFinished {
        attempt: AttemptId,
        status: AttemptStatus,
        summary: Option<String>,
        failure: Option<FailureInfo>,
        steps: u32,
        tool_calls: u32,
        at_ms: u64,
    },
    /// Das Item ist fertig: `Running` → `Completed`.
    WorkItemCompleted {
        item: WorkItemId,
        attempt: AttemptId,
        at_ms: u64,
    },
    /// Der Versuch ist fachlich gescheitert: Item → `Failed`,
    /// `attempt_count` steigt (zählt gegen `max_attempts`).
    WorkItemFailed {
        item: WorkItemId,
        attempt: AttemptId,
        at_ms: u64,
    },
    /// Das Item kehrt nach `Pending` zurück — entweder als Retry nach `Failed`
    /// oder als Recovery eines abgelaufenen Leases (aus `Running`). In
    /// letzterem Fall zählt der unterbrochene Versuch NICHT gegen
    /// `max_attempts` (siehe Plan, §18-Abweichung).
    WorkItemReleased {
        item: WorkItemId,
        reason: String,
        at_ms: u64,
    },
    /// Reine Journal-Markierung für eine Kompaktierung — mutiert keinen
    /// Domänenzustand, nur der Store reagiert darauf.
    CheckpointCreated { seq: u64, at_ms: u64 },
    /// Ein laufender Lauf wurde pausiert.
    RunPaused {
        run: RunId,
        reason: String,
        at_ms: u64,
    },
    /// Ein pausierter Lauf wurde fortgesetzt.
    RunResumed { run: RunId, at_ms: u64 },
    /// Der Lauf ist beendet — erfolgreich oder wegen Budget/Blockade.
    RunCompleted {
        run: RunId,
        reason: CompletionReason,
        at_ms: u64,
    },
    /// Der Lauf wurde abgebrochen; kaskadiert auf alle nicht-terminalen Items.
    RunCanceled { run: RunId, at_ms: u64 },
}
