//! Räumt abgelaufene Leases auf, bevor ein Lauf fortgesetzt wird (§15).
//!
//! Ein `Claimed`/`Running`-Item mit abgelaufenem Lease ist der Fußabdruck
//! eines Prozesses, der mitten im Versuch gestorben ist (`SIGKILL`, Absturz).
//! Das Lease selbst sagt nur "bis wann der Agent Zeit hatte" — die Recovery
//! macht daraus einen sauberen Domänenzustand: der Versuch wird als
//! `Interrupted` abgeschlossen, das Item kehrt nach `Pending` zurück.
//!
//! Ein Absturz kann aber auch NACH einem bereits geschriebenen
//! `AttemptFinished` passieren, bevor der zugehörige zweite Schritt
//! (`WorkItemCompleted`/`WorkItemFailed`) journalt wurde — der Versuch hat
//! dann schon einen feststehenden Ausgang, nur das Item hängt noch auf
//! `Running` mit einem (ggf. noch nicht abgelaufenen) Lease. Recovery
//! VOLLENDET diesen halb geschriebenen Übergang, statt ihn zu verwerfen
//! (Befund 2 des Code-Reviews): ein erfolgreich beendeter Versuch macht das
//! Item `Completed`, nicht `Pending` — alles andere würde bereits erledigte
//! Arbeit wegwerfen und dem README-Versprechen widersprechen, dass ein
//! Absturz höchstens den LAUFENDEN Versuch kostet.
//!
//! Jeder Schritt läuft über [`crate::store::WorkStore::submit`], nie über
//! einen direkten Zugriff auf `WorkState` — Recovery ist fachlich nichts
//! anderes als normale Ereignisse, kein Sonderpfad am Store vorbei.

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::model::{
    AttemptId, AttemptStatus, FailureInfo, FailureKind, WorkItemId, WorkItemStatus, WorkLease,
};
use crate::store::WorkStore;

/// Was die Wiederaufnahme aufgeräumt hat — für die CLI-Ausgabe und Tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub released_items: Vec<WorkItemId>,
    pub interrupted_attempts: Vec<AttemptId>,
}

/// Räumt jedes Lease auf, dessen Ablaufzeit erreicht oder überschritten ist
/// (`state::expired_leases`-Kriterium). Sicher, mehrfach aufzurufen: sobald
/// ein Lease freigegeben ist, verschwindet es aus dem Zustand und taucht bei
/// einem erneuten Aufruf nicht mehr auf — ein leerer Report ist dann das
/// korrekte, idempotente Ergebnis.
pub fn recover(store: &WorkStore, now_ms: u64) -> Result<RecoveryReport, WorkError> {
    recover_matching(store, now_ms, |lease| lease.expires_at_ms <= now_ms)
}

/// Wie [`recover`], gibt aber JEDES vorhandene Lease frei — unabhängig davon,
/// ob es schon abgelaufen ist.
///
/// Das ist nur beim Start eines Vordergrund-Laufs richtig: das MVP hat genau
/// einen Worker (`max_parallel_agents = 1`, kein Daemon). Ein Lease, das beim
/// Öffnen des Journals noch existiert, kann dann nur von einem Prozess
/// stammen, der nicht mehr läuft — der neue Prozess ist der einzige Worker,
/// also kann das Lease nicht "noch aktiv" sein, selbst wenn seine Frist von
/// zehn Minuten formal noch nicht um ist (der alte Prozess kann Sekunden nach
/// dem Claim gestorben sein). Bei mehreren gleichzeitigen Workern wäre diese
/// Annahme falsch — ein nicht abgelaufenes Lease könnte zu einem anderen,
/// noch lebenden Worker gehören. Verteilte Worker sind kein MVP-Ziel.
pub fn recover_all(store: &WorkStore, now_ms: u64) -> Result<RecoveryReport, WorkError> {
    recover_matching(store, now_ms, |_lease| true)
}

/// Bündelt die Angaben für [`interrupt_attempt`] — reine Bündelung ohne
/// eigenes Verhalten (Muster `runner::AttemptOutcomeMeta`), nötig, weil sonst
/// mehr Parameter durchgereicht würden, als Clippys
/// `too_many_arguments`-Schwelle erlaubt.
pub(crate) struct InterruptedAttempt<'a> {
    pub item_id: &'a str,
    pub attempt_id: &'a str,
    pub steps: u32,
    pub tool_calls: u32,
    pub failure_message: String,
    pub release_reason: String,
    pub at_ms: u64,
}

/// Schließt einen unterbrochenen Versuch ab: `AttemptFinished` (Status
/// `Interrupted`) plus `WorkItemReleased` — dieselbe Regel wie überall in
/// diesem Crate, „ein unterbrochener Versuch zählt nicht gegen
/// `max_attempts`" (`WorkItemReleased` statt `WorkItemFailed`, kein
/// `attempt_count`-Inkrement), an genau EINER Stelle statt zweimal identisch
/// nachgebaut. Gemeinsamer Nutzer von [`recover_matching`] (Lease-Ablauf) und
/// `runner::record_interrupted` (Stop-Knopf, dieselbe Journalfolge, nur mit
/// anderen Texten und Herkunft der Schritt-/Tool-Zählung).
pub(crate) fn interrupt_attempt(
    store: &WorkStore,
    attempt: InterruptedAttempt,
) -> Result<(), WorkError> {
    store.submit(WorkEvent::AttemptFinished {
        attempt: attempt.attempt_id.to_string(),
        status: AttemptStatus::Interrupted,
        summary: None,
        failure: Some(FailureInfo {
            kind: FailureKind::Interrupted,
            message: attempt.failure_message,
        }),
        steps: attempt.steps,
        tool_calls: attempt.tool_calls,
        at_ms: attempt.at_ms,
    })?;
    store.submit(WorkEvent::WorkItemReleased {
        item: attempt.item_id.to_string(),
        reason: attempt.release_reason,
        at_ms: attempt.at_ms,
    })?;
    Ok(())
}

/// Schließt einen fachlich GESCHEITERTEN Versuch ab, dessen `AttemptFinished`
/// schon journalt ist: `WorkItemFailed` (erhöht `attempt_count`), danach —
/// nur wenn noch Versuche übrig sind — `WorkItemReleased`. Dieselbe Regel wie
/// [`interrupt_attempt`] für den unterbrochenen Fall, an genau EINER Stelle
/// statt zweimal nachgebaut: gemeinsamer Nutzer von `runner::record_failure`
/// (regulärer Fehlschlag, direkt nach `AttemptFinished`) und
/// [`recover_matching`] (nachgeholter Fehlschlag nach einem Absturz zwischen
/// `AttemptFinished` und `WorkItemFailed`, Befund 2 des Code-Reviews) — beide
/// müssen dieselbe "ist `max_attempts` erschöpft?"-Entscheidung treffen,
/// nicht zwei unabhängig gepflegte Kopien davon.
///
/// `release_reason` bekommt das schon journalte `WorkItem` (mit dem gerade
/// erhöhten `attempt_count`), weil der Text der beiden Aufrufer
/// unterschiedlich ist (`runner`: "Wiederholung X/Y" aus dem Item; `recovery`:
/// der Absturzgrund aus dem Lease) und nur nach dem `WorkItemFailed` feststeht.
/// Gibt zurück, ob das Item freigegeben wurde (`false` heißt: `Failed` bleibt
/// stehen, endgültig blockiert nach `state::blocked_by`).
pub(crate) fn finish_failed_attempt(
    store: &WorkStore,
    item_id: &str,
    attempt_id: &str,
    release_reason: impl FnOnce(&crate::model::WorkItem) -> String,
    at_ms: u64,
) -> Result<bool, WorkError> {
    store.submit(WorkEvent::WorkItemFailed {
        item: item_id.to_string(),
        attempt: attempt_id.to_string(),
        at_ms,
    })?;
    let snapshot = store.snapshot();
    let item = snapshot
        .items
        .get(item_id)
        .ok_or_else(|| WorkError::NotFound(format!("WorkItem '{item_id}'")))?;
    let exhausted = item.attempt_count >= item.max_attempts;
    if !exhausted {
        let reason = release_reason(item);
        store.submit(WorkEvent::WorkItemReleased {
            item: item_id.to_string(),
            reason,
            at_ms,
        })?;
    }
    Ok(!exhausted)
}

/// Gemeinsamer Kern von [`recover`] und [`recover_all`]: welche Leases
/// betroffen sind, entscheidet allein das `include`-Prädikat.
///
/// Reihenfolge ist deterministisch: `leases` liegt in einer `BTreeMap`
/// (Schlüssel = Item-ID), Iteration folgt also immer derselben Ordnung.
fn recover_matching(
    store: &WorkStore,
    now_ms: u64,
    include: impl Fn(&WorkLease) -> bool,
) -> Result<RecoveryReport, WorkError> {
    let snapshot = store.snapshot();
    let leases: Vec<WorkLease> = snapshot
        .leases
        .values()
        .filter(|lease| include(lease))
        .cloned()
        .collect();

    let mut report = RecoveryReport::default();
    for lease in leases {
        // Ein Attempt, der schon `finished_at_ms` gesetzt hat, wird NICHT
        // erneut abgeschlossen (idempotent): das kann vorkommen, wenn der
        // Prozess exakt zwischen `AttemptFinished` und dem folgenden
        // `WorkItemCompleted`/`WorkItemFailed` gestorben ist — der Ausgang
        // des Versuchs steht dann schon fest und darf nicht nachträglich zu
        // `Interrupted` überschrieben werden.
        let attempt_offen = snapshot
            .attempts
            .get(&lease.attempt_id)
            .is_some_and(|a| a.finished_at_ms.is_none());

        // Der unterbrochene Versuch zählt NICHT gegen `max_attempts` (§18,
        // siehe Plan-Abweichungstabelle) — ein gekillter Prozess ist kein
        // fachlicher Fehlversuch. `interrupt_attempt` journalt deshalb
        // `WorkItemReleased` (kein `attempt_count`-Inkrement) statt
        // `WorkItemFailed`.
        let release_reason = format!(
            "Lease abgelaufen um {} ms (jetzt {} ms) — Item für einen neuen Versuch freigegeben",
            lease.expires_at_ms, now_ms
        );
        if attempt_offen {
            // Zwischenstände wie Schrittzahl gibt es im MVP nicht (nur
            // `AttemptFinished` selbst setzt `steps`/`tool_calls`) — der
            // vorhandene Attempt trägt also den letzten bekannten Stand.
            let steps = snapshot
                .attempts
                .get(&lease.attempt_id)
                .map(|a| a.steps)
                .unwrap_or(0);
            let tool_calls = snapshot
                .attempts
                .get(&lease.attempt_id)
                .map(|a| a.tool_calls)
                .unwrap_or(0);
            interrupt_attempt(
                store,
                InterruptedAttempt {
                    item_id: &lease.work_item_id,
                    attempt_id: &lease.attempt_id,
                    steps,
                    tool_calls,
                    failure_message: format!(
                        "Item '{}': Lease abgelaufen um {} ms (jetzt {} ms) — Versuch wurde \
                         unterbrochen, vermutlich durch einen abgestürzten Prozess",
                        lease.work_item_id, lease.expires_at_ms, now_ms
                    ),
                    release_reason,
                    at_ms: now_ms,
                },
            )?;
            report.interrupted_attempts.push(lease.attempt_id.clone());
            report.released_items.push(lease.work_item_id.clone());
        } else {
            // Der Versuch hat schon ein `AttemptFinished` — sein Ausgang
            // steht fest (Befund 2 des Code-Reviews). Der fehlende ZWEITE
            // Schritt (`WorkItemCompleted`/`WorkItemFailed`) wird jetzt
            // NACHGEHOLT, statt das Item pauschal auf `Pending`
            // zurückzuwerfen — sonst würde ein bereits erfolgreich beendeter
            // Versuch stillschweigend verworfen und komplett neu ausgeführt.
            let attempt_status = snapshot
                .attempts
                .get(&lease.attempt_id)
                .map(|a| a.status)
                .ok_or_else(|| WorkError::NotFound(format!("Attempt '{}'", lease.attempt_id)))?;
            match attempt_status {
                AttemptStatus::Succeeded => {
                    store.submit(WorkEvent::WorkItemCompleted {
                        item: lease.work_item_id.clone(),
                        attempt: lease.attempt_id.clone(),
                        at_ms: now_ms,
                    })?;
                    // Bewusst KEIN `released_items.push` hier: das Item ist
                    // FERTIG, nicht für einen neuen Versuch freigegeben — die
                    // CLI-Meldung "N Item(s) freigegeben" soll ein
                    // tatsächlich abgeschlossenes Item nicht mitzählen.
                }
                AttemptStatus::Failed => {
                    // `finish_failed_attempt` (siehe dort) ist derselbe Kern
                    // wie `runner::record_failure`: `WorkItemFailed` journalen
                    // und nur freigeben, wenn nach dem gerade nachgetragenen
                    // Fehlversuch noch Versuche übrig sind — sonst bleibt das
                    // Item `Failed` stehen und zählt als endgültig blockiert
                    // (`state::blocked_by`).
                    let released = finish_failed_attempt(
                        store,
                        &lease.work_item_id,
                        &lease.attempt_id,
                        move |_item| release_reason,
                        now_ms,
                    )?;
                    if released {
                        report.released_items.push(lease.work_item_id.clone());
                    }
                }
                AttemptStatus::Interrupted | AttemptStatus::Running => {
                    // `Running` kann an dieser Stelle eigentlich nicht
                    // auftreten: `attempt_offen` prüft exakt
                    // `finished_at_ms.is_none()`, und nur `AttemptFinished`
                    // setzt `finished_at_ms` UND `status` gemeinsam (siehe
                    // `state::apply`) — ein Attempt mit gesetztem
                    // `finished_at_ms` hat also nie mehr `status ==
                    // Running`. Defensiv trotzdem wie `Interrupted`
                    // behandelt (freigeben, nicht gegen `max_attempts`
                    // zählen): bricht diese Invariante durch einen künftigen
                    // Bug doch einmal, ist "Item neu versuchen" der
                    // sicherere Fehlerfall als "Item für immer auf
                    // `Running` stehen lassen".
                    store.submit(WorkEvent::WorkItemReleased {
                        item: lease.work_item_id.clone(),
                        reason: release_reason,
                        at_ms: now_ms,
                    })?;
                    report.released_items.push(lease.work_item_id.clone());
                }
            }
        }
    }

    // Zweiter, vom Lease unabhängiger Rundgang: ein Item, dessen Prozess GENAU
    // zwischen `WorkItemFailed` und dem folgenden `WorkItemReleased` gestorben
    // ist, bleibt `Failed` stehen, obwohl noch Versuche übrig wären —
    // `WorkItemFailed` entfernt das Lease schon (siehe `state::apply`), also
    // findet der Lease-Rundgang oben so ein Item nie. Ohne diese zweite
    // Prüfung wäre es (und alles, was von ihm abhängt) für immer
    // "endgültig blockiert" (`state::blocked_by`), obwohl `max_attempts`
    // noch nicht ausgeschöpft ist — genau der Absturz, den diese Runtime
    // überleben soll. Gilt für `recover` UND `recover_all` gleichermaßen:
    // das ist kein Lease-Ablauf-Fall, sondern ein halb geschriebener
    // Fehlschlag, unabhängig davon, wie lange er schon zurückliegt.
    let snapshot = store.snapshot();
    let stuck: Vec<WorkItemId> = snapshot
        .items
        .values()
        .filter(|it| it.status == WorkItemStatus::Failed && it.attempt_count < it.max_attempts)
        .map(|it| it.id.clone())
        .collect();
    for item_id in stuck {
        store.submit(WorkEvent::WorkItemReleased {
            item: item_id.clone(),
            reason: "Prozess zwischen Fehlschlag und Freigabe abgestürzt — Item für einen \
                     neuen Versuch freigegeben"
                .to_string(),
            at_ms: now_ms,
        })?;
        report.released_items.push(item_id);
    }

    Ok(report)
}
