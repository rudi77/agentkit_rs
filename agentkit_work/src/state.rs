//! `WorkState` — die Projektion aller Ereignisse auf einen Zustand, plus die
//! Abfragen darauf (Readiness, Blockaden, abgelaufene Leases).
//!
//! `apply` ist die **einzige** Mutationsfunktion. Sie prüft jeden
//! Item-Statusübergang über [`WorkItemStatus::can_transition_to`] — ein
//! unerlaubter Übergang ist ein `Err`, weil er nur durch einen Programmierfehler
//! im Aufrufer entstehen kann (der Aufrufer entscheidet, welches Ereignis er
//! baut; das Modell entscheidet nur, ob es gültig ist).
//!
//! Ebenso lehnt `apply` ein `*Created`/`WorkItemClaimed`-Ereignis ab, dessen
//! ID schon vergeben ist. Das ist die zweite Verteidigungslinie hinter
//! `WorkStore::submit_with` (ID-Vergabe innerhalb des Schreiber-Locks): sollte
//! trotzdem irgendwo eine ID außerhalb des Locks berechnet werden, geht kein
//! Datensatz still verloren (`BTreeMap::insert` überschreibt sonst
//! kommentarlos) — der Schreibvorgang scheitert sichtbar. Eine Snapshot-Zeile
//! im Journal enthält ihre Datensätze bereits direkt (sie wird beim Replay
//! eingesetzt, nicht über `apply` appliziert, siehe `store/journal.rs`), lässt
//! diese Prüfung also nicht erneut aufsetzen.
//!
//! Dieselbe Absicherung gilt für `ProjectCreated`: ein Vorhaben wird genau
//! einmal angelegt, ein zweites `ProjectCreated` ist ein `Err`. Das Journal
//! ist die auditierbare Historie des Vorhabens — es darf nicht behaupten, ein
//! Projekt sei zweimal angelegt worden, nur weil ein Aufrufer (früher: die
//! CLI für `run --max-steps`) das Budget ändern wollte. Dafür gibt es
//! `BudgetUpdated`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::model::{
    id_order, ArtifactId, AttemptId, CompletionReason, RunId, RunStatus, WorkArtifact, WorkAttempt,
    WorkItem, WorkItemId, WorkItemStatus, WorkLease, WorkProject, WorkRun,
};

/// Der materialisierte Zustand eines Projekt-Journals. `BTreeMap` statt
/// `HashMap`, damit Iteration (Anzeige, `to_ops`-artige Kompaktierung) über
/// Neustarts hinweg dieselbe Reihenfolge liefert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkState {
    pub project: Option<WorkProject>,
    pub runs: BTreeMap<RunId, WorkRun>,
    pub items: BTreeMap<WorkItemId, WorkItem>,
    pub attempts: BTreeMap<AttemptId, WorkAttempt>,
    pub leases: BTreeMap<WorkItemId, WorkLease>,
    pub artifacts: BTreeMap<ArtifactId, WorkArtifact>,
    /// Letzte angewandte Journal-Sequenznummer. Teil des Snapshots, damit ein
    /// neu geöffneter Store nahtlos weiterzählt statt Sequenznummern zu wiederholen.
    pub seq: u64,
    run_seq: u64,
    item_seq: u64,
    attempt_seq: u64,
    artifact_seq: u64,
}

impl WorkState {
    /// Nächste freie Lauf-ID. Reine Abfrage (kein Zähler-Verbrauch) — der
    /// Zähler steigt erst, wenn das zugehörige `RunStarted`-Ereignis
    /// tatsächlich angewandt wird (Muster `IdCounters::observe` aus
    /// agentkit_graph, hier direkt im Zustand statt im Store geführt).
    pub fn next_run_id(&self) -> RunId {
        format!("R-{}", self.run_seq + 1)
    }
    pub fn next_item_id(&self) -> WorkItemId {
        format!("W-{}", self.item_seq + 1)
    }
    pub fn next_attempt_id(&self) -> AttemptId {
        format!("A-{}", self.attempt_seq + 1)
    }
    pub fn next_artifact_id(&self) -> ArtifactId {
        format!("AR-{}", self.artifact_seq + 1)
    }

    /// Gesamtzahl der Datensätze — Maß für den Kompaktierungs-Schwellwert des
    /// Journals (`store::REWRITE_FACTOR`/`REWRITE_MIN_LINES`).
    pub fn record_count(&self) -> usize {
        self.project.is_some() as usize
            + self.runs.len()
            + self.items.len()
            + self.attempts.len()
            + self.leases.len()
            + self.artifacts.len()
    }

    /// Wendet ein Ereignis an. Der einzige Mutator — siehe Moduldoku.
    pub fn apply(&mut self, event: &WorkEvent) -> Result<(), WorkError> {
        match event {
            WorkEvent::ProjectCreated { project } => {
                // Dieselbe Absicherung wie bei RunStarted/WorkItemCreated/
                // ArtifactCreated (siehe Moduldoku): ein Vorhaben existiert
                // genau einmal. Ein zweites `ProjectCreated` war der Weg, über
                // den die CLI früher `--max-steps` umgesetzt hat — dafür gibt
                // es jetzt `BudgetUpdated`.
                if self.project.is_some() {
                    return Err(WorkError::Invalid(
                        "Projekt existiert bereits — ein Vorhaben wird nur einmal angelegt"
                            .to_string(),
                    ));
                }
                self.project = Some(project.clone());
            }
            WorkEvent::BudgetUpdated { budget, .. } => {
                let project = self
                    .project
                    .as_mut()
                    .ok_or_else(|| WorkError::NotFound("Projekt".to_string()))?;
                project.budget = budget.clone();
            }
            WorkEvent::RunStarted { run } => {
                // Doppelte ID ablehnen statt sie still zu überschreiben: die
                // eigentliche Absicherung ist `WorkStore::submit_with` (ID-
                // Vergabe INNERHALB des Schreiber-Locks), das hier ist das
                // zweite Netz — falls ein künftiger Aufrufer trotzdem eine ID
                // außerhalb des Locks berechnet, scheitert der Schreibvorgang
                // sichtbar statt einen Datensatz verschwinden zu lassen.
                if self.runs.contains_key(&run.id) {
                    return Err(WorkError::Invalid(format!(
                        "Lauf '{}' existiert bereits",
                        run.id
                    )));
                }
                self.run_seq = self.run_seq.max(id_order(&run.id));
                self.runs.insert(run.id.clone(), run.clone());
            }
            WorkEvent::WorkItemCreated { item } => {
                // Siehe Kommentar bei `RunStarted` — dieselbe Absicherung für
                // Work Items.
                if self.items.contains_key(&item.id) {
                    return Err(WorkError::Invalid(format!(
                        "Work Item '{}' existiert bereits",
                        item.id
                    )));
                }
                self.item_seq = self.item_seq.max(id_order(&item.id));
                self.items.insert(item.id.clone(), item.clone());
            }
            WorkEvent::WorkItemClaimed {
                item,
                agent,
                attempt,
                lease_expires_ms,
                at_ms,
            } => {
                // Siehe Kommentar bei `RunStarted` — dieselbe Absicherung für
                // Versuche (der Attempt entsteht hier, nicht in einem eigenen
                // `AttemptCreated`-Ereignis).
                if self.attempts.contains_key(attempt) {
                    return Err(WorkError::Invalid(format!(
                        "Attempt '{attempt}' existiert bereits"
                    )));
                }
                self.attempt_seq = self.attempt_seq.max(id_order(attempt));
                let it = self.item_mut(item)?;
                // Claim und Start fallen bei einem synchronen Ein-Prozess-Worker
                // zusammen — kein `Claimed`-Zwischenzustand (siehe
                // `WorkItemStatus`-Moduldoku für den Bug, den das behebt).
                transition(it, WorkItemStatus::Running)?;
                it.updated_at_ms = *at_ms;
                self.attempts.insert(
                    attempt.clone(),
                    WorkAttempt {
                        id: attempt.clone(),
                        work_item_id: item.clone(),
                        agent_id: agent.clone(),
                        started_at_ms: *at_ms,
                        finished_at_ms: None,
                        status: crate::model::AttemptStatus::Running,
                        summary: None,
                        failure: None,
                        steps: 0,
                        tool_calls: 0,
                        claim_ids: Vec::new(),
                        verification: None,
                    },
                );
                self.leases.insert(
                    item.clone(),
                    WorkLease {
                        work_item_id: item.clone(),
                        agent_id: agent.clone(),
                        attempt_id: attempt.clone(),
                        claimed_at_ms: *at_ms,
                        expires_at_ms: *lease_expires_ms,
                        last_heartbeat_ms: *at_ms,
                    },
                );
            }
            WorkEvent::LeaseRenewed {
                item,
                attempt,
                lease_expires_ms,
                at_ms,
            } => {
                // Reine Fristverlängerung, keine Statuswirkung — das Item ist
                // bereits seit dem Claim `Running` (siehe event.rs-Moduldoku).
                let it = self.item_mut(item)?;
                it.updated_at_ms = *at_ms;
                let lease = self
                    .leases
                    .get_mut(item)
                    .ok_or_else(|| WorkError::NotFound(format!("Lease für '{item}'")))?;
                if &lease.attempt_id != attempt {
                    return Err(WorkError::Invalid(format!(
                        "Lease für '{item}' gehört zu Attempt '{}', nicht '{attempt}'",
                        lease.attempt_id
                    )));
                }
                lease.expires_at_ms = *lease_expires_ms;
                lease.last_heartbeat_ms = *at_ms;
            }
            WorkEvent::ArtifactCreated { artifact } => {
                // Siehe Kommentar bei `RunStarted` — dieselbe Absicherung für
                // Artefakte.
                if self.artifacts.contains_key(&artifact.id) {
                    return Err(WorkError::Invalid(format!(
                        "Artefakt '{}' existiert bereits",
                        artifact.id
                    )));
                }
                self.artifact_seq = self.artifact_seq.max(id_order(&artifact.id));
                self.artifacts.insert(artifact.id.clone(), artifact.clone());
            }
            WorkEvent::AttemptFinished {
                attempt,
                status,
                summary,
                failure,
                steps,
                tool_calls,
                at_ms,
            } => {
                let a = self
                    .attempts
                    .get_mut(attempt)
                    .ok_or_else(|| WorkError::NotFound(format!("Attempt '{attempt}'")))?;
                a.status = *status;
                a.summary = summary.clone();
                a.failure = failure.clone();
                a.steps = *steps;
                a.tool_calls = *tool_calls;
                a.finished_at_ms = Some(*at_ms);
            }
            WorkEvent::WorkItemCompleted {
                item,
                attempt,
                at_ms,
            } => {
                self.assert_attempt_belongs(item, attempt)?;
                let it = self.item_mut(item)?;
                transition(it, WorkItemStatus::Completed)?;
                it.updated_at_ms = *at_ms;
                self.leases.remove(item);
            }
            WorkEvent::WorkItemFailed {
                item,
                attempt,
                at_ms,
            } => {
                self.assert_attempt_belongs(item, attempt)?;
                let it = self.item_mut(item)?;
                transition(it, WorkItemStatus::Failed)?;
                // Ein fachlich gescheiterter Versuch zählt gegen max_attempts;
                // ein unterbrochener (siehe WorkItemReleased) zählt NICHT.
                it.attempt_count += 1;
                it.updated_at_ms = *at_ms;
                self.leases.remove(item);
            }
            WorkEvent::WorkItemReleased {
                item,
                reason: _,
                at_ms,
            } => {
                let it = self.item_mut(item)?;
                transition(it, WorkItemStatus::Pending)?;
                it.updated_at_ms = *at_ms;
                self.leases.remove(item);
            }
            WorkEvent::CheckpointCreated { .. } => {
                // Reine Journal-Markierung — der Store kompaktiert, die Domäne
                // hat hier nichts zu tun.
            }
            WorkEvent::RunPaused { run, .. } => {
                self.run_mut(run)?.status = RunStatus::Paused;
            }
            WorkEvent::RunResumed { run, .. } => {
                self.run_mut(run)?.status = RunStatus::Running;
            }
            WorkEvent::RunCompleted { run, reason, at_ms } => {
                let r = self.run_mut(run)?;
                r.status = RunStatus::Completed;
                r.completed_at_ms = Some(*at_ms);
                r.completion_reason = Some(*reason);
            }
            WorkEvent::RunCanceled { run, at_ms } => {
                let r = self.run_mut(run)?;
                r.status = RunStatus::Canceled;
                r.completed_at_ms = Some(*at_ms);
                r.completion_reason = Some(CompletionReason::Canceled);
                // Kaskade: Abbruch ist im MVP immer laufweit, es gibt kein
                // eigenes `work_item_canceled`-Ereignis (siehe event.rs).
                // `Failed` schließt hier mit ein: die Matrix erlaubt
                // Failed -> Canceled genau dafür — ein Item, das schon
                // gescheitert war, aber noch retrybar wäre, wird beim
                // Laufabbruch endgültig geschlossen statt in der Schwebe zu
                // bleiben. `AwaitingVerification` ebenso (Phase 5a): ein Item,
                // das auf eine Freigabe wartet, darf beim Laufabbruch nicht
                // unangetastet in der Schwebe bleiben — es wird wie jedes
                // andere offene Item endgültig geschlossen.
                let run_id = run.clone();
                for it in self.items.values_mut().filter(|i| i.run_id == run_id) {
                    if matches!(
                        it.status,
                        WorkItemStatus::Pending
                            | WorkItemStatus::Running
                            | WorkItemStatus::Failed
                            | WorkItemStatus::AwaitingVerification
                    ) {
                        it.status = WorkItemStatus::Canceled;
                        it.updated_at_ms = *at_ms;
                    }
                }
                let canceled: Vec<WorkItemId> = self
                    .items
                    .values()
                    .filter(|i| i.run_id == run_id && i.status == WorkItemStatus::Canceled)
                    .map(|i| i.id.clone())
                    .collect();
                for id in canceled {
                    self.leases.remove(&id);
                }
            }
            WorkEvent::ClaimsRecorded {
                attempt,
                claim_ids,
                at_ms: _,
            } => {
                // HÄNGT an, ersetzt nicht (siehe event.rs-Moduldoku) — ein
                // Versuch darf `work_claim` mehrfach aufrufen, jeder Aufruf
                // journalt nur SEINE neuen IDs.
                let a = self
                    .attempts
                    .get_mut(attempt)
                    .ok_or_else(|| WorkError::NotFound(format!("Attempt '{attempt}'")))?;
                a.claim_ids.extend(claim_ids.iter().cloned());
            }
            WorkEvent::WorkItemSubmittedForVerification {
                item,
                attempt: _,
                at_ms,
            } => {
                // Entfernt bewusst NICHT das Lease (siehe event.rs-Moduldoku)
                // — solange das Item wartet, bleibt es der Weg, den
                // zugehörigen Versuch wiederzufinden (CLI `approve`/`reject`,
                // Recovery-Lückenschluss). `state::expired_leases` und
                // `recovery::recover_matching` schließen es trotzdem
                // strukturell von jedem Zeitablauf aus.
                let it = self.item_mut(item)?;
                transition(it, WorkItemStatus::AwaitingVerification)?;
                it.updated_at_ms = *at_ms;
            }
            WorkEvent::VerificationApproved {
                attempt,
                by,
                reason,
                at_ms: _,
                item: _,
            } => {
                // Reine Buchführung am Versuch — der Statusübergang läuft
                // über das nachfolgende `WorkItemCompleted` (siehe
                // event.rs-Moduldoku).
                let a = self
                    .attempts
                    .get_mut(attempt)
                    .ok_or_else(|| WorkError::NotFound(format!("Attempt '{attempt}'")))?;
                a.verification = Some(crate::model::AttemptVerification::Approved {
                    by: by.clone(),
                    reason: reason.clone(),
                });
            }
            WorkEvent::VerificationRejected {
                attempt,
                by,
                reason,
                at_ms: _,
                item: _,
            } => {
                // Reine Buchführung am Versuch — der Statusübergang läuft
                // über das nachfolgende `WorkItemFailed` (siehe
                // event.rs-Moduldoku).
                let a = self
                    .attempts
                    .get_mut(attempt)
                    .ok_or_else(|| WorkError::NotFound(format!("Attempt '{attempt}'")))?;
                a.verification = Some(crate::model::AttemptVerification::Rejected {
                    by: by.clone(),
                    reason: reason.clone(),
                });
            }
        }
        Ok(())
    }

    /// Bereite Items EINES Laufs: `Pending`, alle Abhängigkeiten `Completed`,
    /// noch Versuche übrig. Sortiert nach Priorität absteigend, dann
    /// Erzeugungsreihenfolge aufsteigend (fair innerhalb derselben Priorität).
    ///
    /// Der `run_id`-Filter ist keine Zukunftsvorsorge, sondern eine Absicherung:
    /// die Items ALLER Läufe liegen in einer Map, und ein zweiter Lauf desselben
    /// Projekts würde ohne Filter die Reste des ersten mit einplanen. Die
    /// Abhängigkeiten werden bewusst ungefiltert aufgelöst — eine Abhängigkeit
    /// auf ein Item eines früheren Laufs bleibt gültig, wenn es fertig ist.
    pub fn ready_items(&self, run_id: &str) -> Vec<&WorkItem> {
        let mut ready: Vec<&WorkItem> = self
            .items
            .values()
            .filter(|it| it.run_id == run_id)
            .filter(|it| it.status == WorkItemStatus::Pending)
            .filter(|it| it.attempt_count < it.max_attempts)
            .filter(|it| {
                it.dependencies.iter().all(|dep| {
                    self.items
                        .get(dep)
                        .is_some_and(|d| d.status == WorkItemStatus::Completed)
                })
            })
            .collect();
        ready.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.seq.cmp(&b.seq)));
        ready
    }

    /// Abhängigkeiten eines Items, die es JETZT noch nicht laufen lassen: alles,
    /// was nicht `Completed` ist — das schließt `Failed`/`Canceled` und noch
    /// nicht abgeschlossene Vorgänger gleichermaßen ein, unabhängig davon, ob
    /// sie sich je noch erledigen. Fehlt eine Abhängigkeit ganz, wartet das
    /// Item ebenfalls auf sie (sicherer Default). Für die Frage „kommt das
    /// noch oder nie mehr" siehe [`WorkState::blocked_by`].
    pub fn waiting_on(&self, id: &str) -> Vec<WorkItemId> {
        let Some(item) = self.items.get(id) else {
            return Vec::new();
        };
        item.dependencies
            .iter()
            .filter(|dep| {
                !self
                    .items
                    .get(dep.as_str())
                    .is_some_and(|d| d.status == WorkItemStatus::Completed)
            })
            .cloned()
            .collect()
    }

    /// Abhängigkeiten, die ENDGÜLTIG verhindern, dass das Item je läuft:
    /// `Failed` mit ausgeschöpften Versuchen, `Canceled`, eine fehlende ID,
    /// oder eine Abhängigkeit, die selbst schon so blockiert ist (transitiv).
    /// Ein `Pending`/`Running` Vorgänger blockiert dagegen nicht — er ist nur
    /// (noch) nicht fertig, siehe [`WorkState::waiting_on`].
    ///
    /// Der Runner nutzt das, um bei Stillstand zu entscheiden, ob er wartet
    /// oder den Lauf mit `CompletionReason::Blocked` beendet; die CLI nutzt es,
    /// um dem Nutzer zu sagen, warum nichts mehr vorankommt.
    pub fn blocked_by(&self, id: &str) -> Vec<WorkItemId> {
        let Some(item) = self.items.get(id) else {
            return Vec::new();
        };
        item.dependencies
            .iter()
            .filter(|dep| {
                // Frischer Besucht-Satz je Abhängigkeit: ein Zyklus in einem
                // Zweig darf die Prüfung eines anderen Zweigs nicht verfälschen.
                let mut visited = std::collections::HashSet::new();
                visited.insert(id.to_string());
                self.is_permanently_blocked(dep, &mut visited)
            })
            .cloned()
            .collect()
    }

    /// Iterative Hilfsfunktion für `blocked_by`: `id` ist endgültig blockiert,
    /// wenn es fehlt, selbst `Canceled`/ausgeschöpft-`Failed` ist, oder jede
    /// seiner Abhängigkeiten das (rekursiv) ist. Der Besucht-Satz macht die
    /// Prüfung zyklensicher, egal ob der Graph selbst einen Zyklus enthält.
    fn is_permanently_blocked(
        &self,
        id: &str,
        visited: &mut std::collections::HashSet<WorkItemId>,
    ) -> bool {
        if !visited.insert(id.to_string()) {
            // Schon auf diesem Pfad gesehen: ein Zyklus entscheidet nichts neu.
            return false;
        }
        match self.items.get(id) {
            None => true,
            Some(item) => match item.status {
                WorkItemStatus::Canceled => true,
                WorkItemStatus::Failed => item.attempt_count >= item.max_attempts,
                WorkItemStatus::Completed => false,
                // Ein Item in AwaitingVerification hat schon erfolgreich
                // durchlaufen — seine eigenen Abhängigkeiten müssen also
                // längst erfüllt sein (der Scheduler ließ es erst laufen,
                // nachdem sie `Completed` waren). Es rekursiv wie
                // Pending/Running zu behandeln ist trotzdem korrekt UND
                // notwendig: ein Nachfolger, der von diesem Item abhängt, ist
                // erst dann "endgültig blockiert", wenn die Prüfung selbst
                // scheitert (→ Failed/Pending) oder das Item abgebrochen wird
                // (→ Canceled) — solange es wartet, ist "irgendwann noch
                // möglich" die richtige Antwort, nicht "nie".
                WorkItemStatus::Pending
                | WorkItemStatus::Running
                | WorkItemStatus::AwaitingVerification => item
                    .dependencies
                    .iter()
                    .any(|dep| self.is_permanently_blocked(dep, visited)),
            },
        }
    }

    /// Prüft eine geplante Abhängigkeitsliste, BEVOR das Item entsteht — Items
    /// kommen im Regelfall aus `work_add_item`, also von einem LLM, und ein
    /// unbemerkter Zyklus macht `ready_items` dauerhaft leer, ohne dass
    /// irgendetwas fehlschlägt: der schlimmste Ausgang. Lehnt ab: unbekannte
    /// IDs, Selbstreferenz, Duplikate, und jede Abhängigkeit, die über den
    /// bestehenden Graphen zyklisch auf `new_item_id` zurückführt.
    pub fn validate_dependencies(
        &self,
        new_item_id: &str,
        deps: &[WorkItemId],
    ) -> Result<(), WorkError> {
        let mut seen = std::collections::HashSet::new();
        for dep in deps {
            if dep == new_item_id {
                return Err(WorkError::Invalid(format!(
                    "Item '{new_item_id}' kann nicht von sich selbst abhängen"
                )));
            }
            if !self.items.contains_key(dep) {
                return Err(WorkError::Invalid(format!(
                    "Abhängigkeit '{dep}' existiert nicht"
                )));
            }
            if !seen.insert(dep.clone()) {
                return Err(WorkError::Invalid(format!(
                    "Abhängigkeit '{dep}' ist doppelt angegeben"
                )));
            }
        }
        for dep in deps {
            if self.leads_back_to(dep, new_item_id) {
                return Err(WorkError::Invalid(format!(
                    "Abhängigkeit '{dep}' führt über den bestehenden Graphen zyklisch zurück auf '{new_item_id}'"
                )));
            }
        }
        Ok(())
    }

    /// Iterative Tiefensuche mit Besucht-Menge: ist `target` von `start` aus
    /// über vorhandene Abhängigkeiten erreichbar? Die Besucht-Menge sorgt
    /// dafür, dass ein bereits im Graphen vorhandener Zyklus die Suche nicht
    /// in eine Endlosschleife schickt.
    fn leads_back_to(&self, start: &str, target: &str) -> bool {
        let mut stack = vec![start.to_string()];
        let mut visited = std::collections::HashSet::new();
        while let Some(current) = stack.pop() {
            if current == target {
                return true;
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            if let Some(item) = self.items.get(&current) {
                stack.extend(item.dependencies.iter().cloned());
            }
        }
        false
    }

    /// Leases, deren Ablaufzeit erreicht oder überschritten ist — Grundlage der
    /// Recovery beim Öffnen (§15).
    ///
    /// Schließt Leases von Items in `AwaitingVerification` STRUKTURELL aus,
    /// unabhängig von `expires_at_ms` (Phase 5a): ein Item, das auf ein
    /// `HumanApproval`-Gate wartet, kann legitim Tage offen bleiben —
    /// `WorkItemSubmittedForVerification` entfernt sein Lease bewusst nicht
    /// (siehe event.rs), ein reiner Zeitablauf darf dieses Warten aber nicht
    /// zerstören. `recovery::recover_matching` wendet dieselbe Ausnahme an.
    pub fn expired_leases(&self, now_ms: u64) -> Vec<&WorkLease> {
        self.leases
            .values()
            .filter(|l| l.expires_at_ms <= now_ms)
            .filter(|l| !self.is_awaiting_verification(&l.work_item_id))
            .collect()
    }

    /// Ob `id` aktuell `AwaitingVerification` ist — reine Abfrage, geteilt von
    /// [`WorkState::expired_leases`] und `recovery::recover_matching`.
    pub fn is_awaiting_verification(&self, id: &str) -> bool {
        self.items
            .get(id)
            .is_some_and(|it| it.status == WorkItemStatus::AwaitingVerification)
    }

    /// Ein Lauf ist fertig abgearbeitet, wenn kein seiner Items mehr in einem
    /// nicht-terminalen Zustand steckt (unabhängig vom Ausgang je Item). Ein
    /// Lauf ohne Items gilt als trivial fertig.
    pub fn is_run_complete(&self, run_id: &str) -> bool {
        self.items
            .values()
            .filter(|it| it.run_id == run_id)
            .all(|it| {
                matches!(
                    it.status,
                    WorkItemStatus::Completed | WorkItemStatus::Failed | WorkItemStatus::Canceled
                )
            })
    }

    fn item_mut(&mut self, id: &str) -> Result<&mut WorkItem, WorkError> {
        self.items
            .get_mut(id)
            .ok_or_else(|| WorkError::NotFound(format!("WorkItem '{id}'")))
    }

    fn run_mut(&mut self, id: &str) -> Result<&mut WorkRun, WorkError> {
        self.runs
            .get_mut(id)
            .ok_or_else(|| WorkError::NotFound(format!("Run '{id}'")))
    }

    fn assert_attempt_belongs(&self, item: &str, attempt: &str) -> Result<(), WorkError> {
        match self.attempts.get(attempt) {
            Some(a) if a.work_item_id == item => Ok(()),
            Some(_) => Err(WorkError::Invalid(format!(
                "Attempt '{attempt}' gehört nicht zu Item '{item}'"
            ))),
            None => Err(WorkError::NotFound(format!("Attempt '{attempt}'"))),
        }
    }
}

/// Prüft und vollzieht einen Item-Statusübergang — die einzige Stelle, die
/// `WorkItem::status` direkt setzt (neben der `RunCanceled`-Kaskade, die
/// dieselbe Matrix implizit respektiert, da sie nur aus nicht-terminalen
/// Zuständen heraus abbricht).
fn transition(item: &mut WorkItem, next: WorkItemStatus) -> Result<(), WorkError> {
    if !item.status.can_transition_to(next) {
        return Err(WorkError::Transition(format!(
            "Item '{}': {} -> {next} nicht erlaubt",
            item.id, item.status
        )));
    }
    item.status = next;
    Ok(())
}
