//! Der Scheduler: rein, deterministisch, keine I/O. Trifft KEINE Entscheidung
//! selbst durch — er sagt dem Runner nur, was als Nächstes zu tun ist. Zeit
//! und Zustand kommen als Parameter (siehe `model.rs`-Moduldoku), damit
//! `decide` ohne Sleep und ohne echten Store testbar ist.

use crate::model::{WorkBudget, WorkItem, WorkItemId, WorkItemKind, WorkItemStatus};
use crate::state::WorkState;

/// Was der Scheduler gerade zu tun empfiehlt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Dieses Item als Nächstes bearbeiten.
    Run(WorkItemId),
    /// Es läuft schon so viel wie erlaubt — warten (heute unerreichbar bei
    /// max_parallel_agents = 1, aber der Runner muss den Fall kennen).
    AtCapacity,
    /// Ein Budgetlimit ist erreicht.
    BudgetExhausted(String),
    /// Nichts ist ausführbar, aber der Lauf ist NICHT blockiert (Phase 5a):
    /// mindestens eines der offenen Items wartet in `AwaitingVerification` auf
    /// eine menschliche Freigabe, und alles andere hängt (mittelbar) daran.
    /// Anders als `Blocked` kommt dieser Lauf weiter, sobald `agentkit work
    /// approve`/`reject` entscheidet — er ist nicht endgültig gescheitert.
    AwaitingVerification(Vec<WorkItemId>),
    /// Es gibt offene Items, aber keins davon kann je laufen.
    Blocked(Vec<WorkItemId>),
    /// Alle Items des Laufs sind terminal — fertig.
    Done,
}

/// Trifft die nächste Scheduling-Entscheidung für `run_id`. Reihenfolge ist
/// Teil des Vertrags (siehe Plan): fertig, dann Budget, dann Kapazität, dann
/// Auswahl, dann Blockade — niemals umsortiert, weil jede Stufe eine
/// vorherige voraussetzt (z. B. darf ein überzogenes Budget keinen weiteren
/// Versuch mehr zulassen, selbst wenn etwas bereit wäre).
pub fn decide(
    state: &WorkState,
    run_id: &str,
    budget: &WorkBudget,
    started_at_ms: u64,
    now_ms: u64,
) -> Decision {
    // 1. Ein Lauf ohne offene Items gilt als trivial fertig (so dokumentiert
    // in `state::is_run_complete`) — die Planungsphase davor behandelt der
    // Runner, hier zählt nur der Zustand.
    //
    // Ausnahme: ein Lauf, in dem AUSSER dem Planungs-Item nie ein Item
    // existierte, ist zwar terminal, aber nicht "erledigt" — die Zerlegung
    // hat nichts erzeugt, das Vorhaben blieb unbearbeitet. Ohne diese Prüfung
    // würde ein Planungsversuch, der erfolgreich abschließt, aber kein
    // einziges Folge-Item anlegt, den Lauf mit `AllItemsDone` beenden (Exit 0
    // in der CLI) — eine falsche Erfolgsmeldung für ein Skript, das nur den
    // Exit-Code sieht. Absichtlich HIER statt im Runner: die Regel ist eine
    // reine Eigenschaft des Zustands ("gibt es ein abgeschlossenes
    // Nicht-Planungs-Item"), keine Aktion, und soll — wie der Rest von
    // `decide` — ohne I/O und an GENAU einer Stelle geprüft werden.
    if state.is_run_complete(run_id) {
        let items_of_run: Vec<&WorkItem> = state
            .items
            .values()
            .filter(|it| it.run_id == run_id)
            .collect();
        let has_real_progress = items_of_run
            .iter()
            .any(|it| it.kind != WorkItemKind::Planning && it.status == WorkItemStatus::Completed);
        if !items_of_run.is_empty() && !has_real_progress {
            return Decision::Blocked(items_of_run.iter().map(|it| it.id.clone()).collect());
        }
        return Decision::Done;
    }

    // 2. Budget VOR der Item-Auswahl: ein überzogener Lauf darf keinen
    // weiteren Versuch mehr starten, auch wenn gerade ein Item bereit wäre.
    if let Some(max_secs) = budget.max_wall_time_secs {
        let elapsed_secs = now_ms.saturating_sub(started_at_ms) / 1000;
        if elapsed_secs >= max_secs {
            return Decision::BudgetExhausted(format!(
                "max_wall_time_secs ({max_secs}s) überschritten: Lauf läuft seit {elapsed_secs}s"
            ));
        }
    }
    if let Some(max_items) = budget.max_work_items {
        let item_count = state
            .items
            .values()
            .filter(|it| it.run_id == run_id)
            .count() as u32;
        if item_count >= max_items {
            return Decision::BudgetExhausted(format!(
                "max_work_items ({max_items}) erreicht: Lauf hat {item_count} Items"
            ));
        }
    }

    // 3. Laufende Items zählen: mehr als erlaubt darf nicht gleichzeitig
    // arbeiten. Bei max_parallel_agents = 1 (heutiger MVP-Wert) genügt schon
    // ein einziges Running-Item.
    let running = state
        .items
        .values()
        .filter(|it| it.run_id == run_id && it.status == WorkItemStatus::Running)
        .count() as u32;
    if running >= budget.max_parallel_agents {
        return Decision::AtCapacity;
    }

    // 4. Nächstes bereites Item — `ready_items` sortiert schon nach Priorität
    // (absteigend) dann Erzeugungsreihenfolge (aufsteigend, fair bei
    // Gleichstand). Das erste Element ist die Wahl des Schedulers.
    if let Some(item) = state.ready_items(run_id).first() {
        return Decision::Run(item.id.clone());
    }

    // 5. Nichts ist bereit, der Lauf ist aber laut Schritt 1 nicht fertig und
    // laut Schritt 3 nicht an der Kapazitätsgrenze (sonst wäre schon
    // `AtCapacity` zurückgekommen). Jedes offene Item wartet also entweder
    // auf einen endgültig blockierten Vorgänger — dann kommt der Lauf nie
    // mehr voran — oder auf ein Item, das gerade noch läuft und irgendwann
    // fertig wird.
    let open: Vec<&WorkItem> = state
        .items
        .values()
        .filter(|it| {
            it.run_id == run_id
                && !matches!(
                    it.status,
                    WorkItemStatus::Completed | WorkItemStatus::Failed | WorkItemStatus::Canceled
                )
        })
        .collect();

    let all_permanently_blocked = open
        .iter()
        .all(|it| !state.blocked_by(&it.id).is_empty() || it.attempt_count >= it.max_attempts);

    if all_permanently_blocked {
        Decision::Blocked(open.iter().map(|it| it.id.clone()).collect())
    } else {
        // Nicht alles ist endgültig blockiert — irgendetwas könnte noch
        // voran kommen. Wenn dabei mindestens ein Item WIRKLICH läuft, ist
        // die alte Antwort weiter richtig: warten, bis dieser Versuch endet
        // (nur bei künftigem max_parallel_agents > 1 überhaupt erreichbar,
        // siehe Schritt 3 — bei 1 hätte der schon oben AtCapacity geliefert).
        let something_running = open.iter().any(|it| it.status == WorkItemStatus::Running);
        let awaiting_verification: Vec<WorkItemId> = open
            .iter()
            .filter(|it| it.status == WorkItemStatus::AwaitingVerification)
            .map(|it| it.id.clone())
            .collect();
        // Läuft nichts mehr, aber mindestens ein offenes Item wartet in
        // AwaitingVerification: JEDES andere offene, nicht endgültig
        // blockierte Item muss (transitiv) auf genau so ein Gate warten —
        // sonst wäre es entweder `ready_items` (Schritt 4, hätte schon
        // `Run` geliefert) oder `all_permanently_blocked` (oben behandelt).
        // Der Lauf steht also NICHT still, weil er blockiert wäre, sondern
        // weil er auf eine Freigabe wartet (Phase 5a, §10/§20).
        if !something_running && !awaiting_verification.is_empty() {
            Decision::AwaitingVerification(awaiting_verification)
        } else {
            // Mindestens ein offenes Item wartet auf ein Item, das (noch)
            // läuft oder retrybar ist — das kann sich nur durch Fortschritt
            // eines laufenden Versuchs auflösen, nicht durch erneutes Fragen.
            Decision::AtCapacity
        }
    }
}
