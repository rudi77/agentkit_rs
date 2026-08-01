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

use std::sync::Arc;

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::graph::GraphGateway;
use crate::model::{
    AttemptId, AttemptStatus, AttemptVerification, FailureInfo, FailureKind, VerificationPolicy,
    WorkItemId, WorkItemStatus, WorkLease,
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

/// Schließt eine zweite, vom Lease UNABHÄNGIGE Absturzlücke (Phase 5b, §11):
/// ein Item, das eine echte Verifikation bestanden hat und `Completed` ist,
/// dessen Claims aber noch nicht promotet wurden (`WorkItem::claims_promoted
/// == false`). Deckt zwei Fälle in einem Rutsch ab, GENAU wie die
/// bestehenden Lücken oben:
///
/// 1. Ein Absturz zwischen `VerificationApproved`/`VerificationRejected` und
///    dem folgenden `WorkItemCompleted` — der DRITTE Rundgang in
///    [`recover_matching`] holt diesen `WorkItemCompleted`-Schritt schon
///    nach; das hinterlässt das Item genau im Zustand, den dieser Rundgang
///    hier erkennt (`Completed`, `claims_promoted == false`) und behandelt.
/// 2. Ein Absturz zwischen `WorkItemCompleted` und `ClaimsPromoted` selbst
///    (die Promotion war noch nicht einmal versucht) — oder ein vorheriger
///    Promotionsversuch, der am Gateway gescheitert ist (siehe
///    `graph::promote_after_completion`, journalt bei einem Fehlschlag
///    bewusst NICHTS): beide Male bleibt `claims_promoted == false` stehen,
///    dieser Rundgang bekommt so bei jedem Resume automatisch einen neuen
///    Versuch.
///
/// Bewusst eine EIGENE Funktion statt in `recover`/`recover_all` verdrahtet:
/// die beiden bräuchten sonst ein `GraphGateway`-Argument, das ihre 25+
/// bestehenden Aufrufer (Lease-Recovery, komplett graph-unabhängig) nicht
/// beträfe. Der Aufrufer (CLI `work run`/`resume`) ruft diese Funktion direkt
/// NACH `recover_all` auf, nur wenn ein Graph angebunden ist.
///
/// Ohne Gateway (`gateway == None`, kein `--graph DIR`/Feature `graph`) ein
/// No-Op — leere Liste, kein Scan. Ein Fehlschlag einzelner Promotionen
/// bricht nichts ab (siehe `graph::promote_after_completion`): die
/// zurückgegebenen Meldungen sind Warnungen, die der Aufrufer anzeigt.
pub fn recover_pending_promotions(
    store: &WorkStore,
    gateway: Option<&Arc<dyn GraphGateway>>,
    now_ms: u64,
) -> Vec<String> {
    let Some(gateway) = gateway else {
        return Vec::new();
    };
    let snapshot = store.snapshot();
    let pending: Vec<(WorkItemId, VerificationPolicy)> = snapshot
        .items
        .values()
        .filter(|it| {
            it.status == WorkItemStatus::Completed
                && !matches!(it.verification_policy, VerificationPolicy::None)
                && !it.claims_promoted
        })
        .map(|it| (it.id.clone(), it.verification_policy.clone()))
        .collect();

    let mut warnings = Vec::new();
    for (item_id, policy) in pending {
        if let Some(msg) =
            crate::graph::promote_after_completion(store, Some(gateway), &item_id, &policy, now_ms)
        {
            warnings.push(msg);
        }
    }
    warnings
}

/// Ausgang von [`recover_git_branch`] — für die CLI-Anzeige und Tests, exakt
/// das Muster von [`RecoveryReport`]: diese Funktion journalt nichts, sie
/// meldet nur, was sie am Git-Zustand vorgefunden (und ggf. repariert) hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitBranchRecovery {
    /// Git-Isolation ist aus, oder der Lauf hat keinen festgehaltenen
    /// Ausgangsbranch (älteres Journal von vor dieser Korrektur) — nichts zu
    /// tun.
    Inactive,
    /// Das Repository stand schon auf dem Ausgangsbranch.
    AlreadyOnBase,
    /// Das Repository stand auf einem EIGENEN, SAUBEREN Item-Branch dieses
    /// Projekts — zurückgewechselt.
    Restored { from: String, to: String },
    /// Das Repository stand auf einem EIGENEN Item-Branch dieses Projekts,
    /// aber mit uncommitteten Änderungen — KEIN Checkout versucht (Befund des
    /// Code-Reviews: `git checkout` würde solche Änderungen, wenn sie mit dem
    /// Ausgangsbranch nicht kollidieren, klaglos MIT auf den Ausgangsbranch
    /// nehmen — ein stiller Datenverlust der Provenance, kein Fehler, den
    /// git meldet). Ein abgestürzter Versuch committet nie, so eine Änderung
    /// ist also der Normalfall nach SIGKILL, nicht die Ausnahme.
    DirtyWorkingTree { current: String, base: String },
    /// Das Repository stand auf einem FREMDEN Branch (kein Item-Branch
    /// dieses Projekts) — eine bewusste Entscheidung des Nutzers, NICHT
    /// stillschweigend überschrieben.
    ForeignBranch { current: String, base: String },
    /// Der aktuelle Branch war nicht ermittelbar — der Lauf läuft trotzdem
    /// weiter, der Git-Zustand bleibt unangetastet.
    BranchLookupFailed { base: String, error: String },
    /// Der Rückwechsel auf den Ausgangsbranch ist trotz sauberem Arbeitsbaum
    /// fehlgeschlagen (ein harter, unerwarteter Git-Fehler) — der Aufrufer
    /// soll den Lauf NICHT starten.
    RestoreFailed {
        from: String,
        to: String,
        error: String,
    },
}

/// Befund 1 der Handprobe: ein hart beendeter Prozess (SIGKILL, Absturz,
/// Stromausfall — in der Praxis auch das zweite, erzwungene Ctrl-C dieses
/// Programms) lässt den Arbeitsbaum auf dem Item-Branch stehen, denn der
/// `Drop`-Guard aus `runner::GitAttemptCtx` läuft dann nicht mehr — es läuft
/// schlicht kein Rust-Code mehr. Ein `Drop` kann das strukturell nicht
/// abfangen; die Wiederherstellung gehört deshalb GENAU HIER hin, wo diese
/// Laufzeit ohnehin schon jeden anderen halb fertigen Übergang aufräumt
/// (`recover_all`/`recover_pending_promotions`) — nicht in die CLI, die nur
/// noch das Ergebnis rendert (dasselbe Verhältnis wie bei den beiden
/// genannten Funktionen und `cmd_run`).
///
/// Steht das Repository auf einem Item-Branch DIESES Projekts
/// (`work/<projekt-id>/…`), ist das die eigene Hinterlassenschaft der
/// Laufzeit — sie wechselt selbst zurück, aber NUR wenn der Arbeitsbaum
/// sauber ist (siehe [`GitBranchRecovery::DirtyWorkingTree`]). Steht es auf
/// einem ANDEREN Branch, war das eine bewusste Entscheidung des Nutzers (z. B.
/// ein manueller Checkout zwischen zwei Aufrufen); die überschreibt diese
/// Funktion NICHT stillschweigend, sie meldet den Zustand nur.
///
/// Liest `workspace`/`project_id`/`git_isolation` selbst aus dem Snapshot
/// (Befund des Code-Reviews: drei separate Parameter hätten dem Aufrufer
/// erlaubt, einen davon nicht zum selben Projekt passend zu übergeben) —
/// dasselbe Muster wie [`recover_pending_promotions`], das ebenfalls direkt
/// vom `WorkStore` liest.
pub fn recover_git_branch(store: &WorkStore, run_id: &str) -> GitBranchRecovery {
    let snapshot = store.snapshot();
    let Some(project) = snapshot.project.as_ref() else {
        return GitBranchRecovery::Inactive;
    };
    if !project.git_isolation {
        return GitBranchRecovery::Inactive;
    }
    let Some(base_branch) = snapshot
        .runs
        .get(run_id)
        .and_then(|r| r.base_branch.clone())
    else {
        return GitBranchRecovery::Inactive;
    };
    let workspace = &project.workspace;
    match crate::git::current_branch(workspace) {
        Ok(current) if current == base_branch => GitBranchRecovery::AlreadyOnBase,
        Ok(current) if crate::model::is_item_branch_of(&project.id, &current) => {
            match crate::git::is_clean(workspace) {
                Ok(true) => match crate::git::checkout(workspace, &base_branch) {
                    Ok(()) => GitBranchRecovery::Restored {
                        from: current,
                        to: base_branch,
                    },
                    Err(error) => GitBranchRecovery::RestoreFailed {
                        from: current,
                        to: base_branch,
                        error,
                    },
                },
                Ok(false) => GitBranchRecovery::DirtyWorkingTree {
                    current,
                    base: base_branch,
                },
                // Der Status selbst war nicht ermittelbar — dieselbe
                // vorsichtige Haltung wie bei `Ok(false)`: OHNE Gewissheit
                // über einen sauberen Arbeitsbaum wird kein Checkout versucht.
                Err(error) => GitBranchRecovery::RestoreFailed {
                    from: current,
                    to: base_branch,
                    error,
                },
            }
        }
        Ok(current) => GitBranchRecovery::ForeignBranch {
            current,
            base: base_branch,
        },
        Err(error) => GitBranchRecovery::BranchLookupFailed {
            base: base_branch,
            error,
        },
    }
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
        // Phase 5a: ein Item in `AwaitingVerification` behält sein Lease
        // bewusst (siehe event.rs), damit sich der wartende Versuch
        // wiederfinden lässt — genau DESHALB darf kein Zeitablauf es
        // anfassen. Gilt für `recover` UND `recover_all` gleichermaßen: bei
        // `recover_all` wäre das `include`-Prädikat allein (`|_| true`) sonst
        // gerade das, was ein tagelang legitim wartendes Human-Gate zerstören
        // würde.
        .filter(|lease| !snapshot.is_awaiting_verification(&lease.work_item_id))
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
                    // Phase 5a: ein erfolgreicher Versuch schließt das Item
                    // nur bei `VerificationPolicy::None` DIREKT ab — sonst
                    // fehlt (mindestens) noch `WorkItemSubmittedForVerification`,
                    // dessen Absturz-Lücke selbst wieder eine ist (siehe
                    // dritter Rundgang unten, der `AwaitingVerification`-Items
                    // mit noch fehlendem Prüfergebnis auflöst). Ohne diese
                    // Fallunterscheidung würde ein Absturz hier eine
                    // verifikationspflichtige Arbeit ungeprüft als `Completed`
                    // durchwinken.
                    let policy = snapshot
                        .items
                        .get(&lease.work_item_id)
                        .map(|it| it.verification_policy.clone())
                        .unwrap_or_default();
                    if matches!(policy, VerificationPolicy::None) {
                        store.submit(WorkEvent::WorkItemCompleted {
                            item: lease.work_item_id.clone(),
                            attempt: lease.attempt_id.clone(),
                            at_ms: now_ms,
                        })?;
                    } else {
                        store.submit(WorkEvent::WorkItemSubmittedForVerification {
                            item: lease.work_item_id.clone(),
                            attempt: lease.attempt_id.clone(),
                            at_ms: now_ms,
                        })?;
                    }
                    // Bewusst KEIN `released_items.push` hier: das Item ist
                    // FERTIG (oder wartet legitim auf Verifikation), nicht für
                    // einen neuen Versuch freigegeben — die
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

    // Dritter Rundgang, lease-unabhängig (Phase 5a): Items in
    // `AwaitingVerification`, deren Lease absichtlich nicht angefasst wurde
    // (siehe oben) — hier wird stattdessen geprüft, ob ihr Prüfergebnis schon
    // feststeht, aber der abschließende Schritt fehlt. Zwei Lücken:
    //
    // 1. Zwischen `VerificationApproved`/`VerificationRejected` und dem
    //    folgenden `WorkItemCompleted`/`WorkItemFailed` (derselbe
    //    "halb geschriebene Übergang"-Fall wie beim regulären
    //    `AttemptFinished` weiter oben, nur eine Ebene später) — wird
    //    nachgeholt.
    // 2. Bei `AutomatedTests`: das Prüfkommando löst SYNCHRON im selben
    //    Versuch auf (siehe `runner::record_success`) — ein Item darf diesen
    //    Zustand über einen Prozessneustart hinweg also NIE mit `verification
    //    == None` erreichen. Taucht das doch auf, ist das der Fußabdruck
    //    eines Absturzes MITTEN in der Prüfung; kein fachlicher Fehlschlag
    //    (die Prüfung kam gar nicht zu einem Ergebnis), also Freigabe ohne
    //    `attempt_count`-Erhöhung — dieselbe Regel wie bei jedem anderen
    //    unterbrochenen Versuch.
    //
    // `HumanApproval` mit `verification == None` ist der NORMALE, legitim
    // wartende Fall (das ist der ganze Sinn des Gates) — wird hier bewusst
    // NICHT angefasst.
    let snapshot = store.snapshot();
    let awaiting: Vec<(WorkItemId, AttemptId, VerificationPolicy)> = snapshot
        .items
        .values()
        .filter(|it| it.status == WorkItemStatus::AwaitingVerification)
        .filter_map(|it| {
            snapshot.leases.get(&it.id).map(|lease| {
                (
                    it.id.clone(),
                    lease.attempt_id.clone(),
                    it.verification_policy.clone(),
                )
            })
        })
        .collect();

    for (item_id, attempt_id, policy) in awaiting {
        let verification = snapshot
            .attempts
            .get(&attempt_id)
            .and_then(|a| a.verification.clone());
        match verification {
            Some(AttemptVerification::Approved { .. }) => {
                store.submit(WorkEvent::WorkItemCompleted {
                    item: item_id,
                    attempt: attempt_id,
                    at_ms: now_ms,
                })?;
                // Kein `released_items.push`: das Item ist FERTIG (siehe
                // derselbe Kommentar beim regulären Succeeded-Fall oben).
            }
            Some(AttemptVerification::Rejected { reason, .. }) => {
                let released = finish_failed_attempt(
                    store,
                    &item_id,
                    &attempt_id,
                    move |_item| reason,
                    now_ms,
                )?;
                if released {
                    report.released_items.push(item_id);
                }
            }
            None => {
                // `AutomatedTests` erreicht diesen Zweig nur nach einem
                // Absturz mitten in der (synchron auflösenden) Prüfung.
                // `None` erreicht `AwaitingVerification` nach heutigem Code
                // gar nicht (`record_success` schließt bei `None` direkt ab,
                // siehe dort) — bewusst trotzdem defensiv mitbehandelt
                // (Befund des Code-Reviews): ein hand-editiertes Journal oder
                // ein künftiger zweiter Erzeuger dürfte kein Item für immer
                // in der Schwebe lassen, das kein `HumanApproval` erwartet.
                if !matches!(policy, VerificationPolicy::HumanApproval) {
                    store.submit(WorkEvent::WorkItemReleased {
                        item: item_id.clone(),
                        reason: "Prozess mitten in der automatisierten Prüfung abgestürzt — \
                                 Item für einen neuen Versuch freigegeben"
                            .to_string(),
                        at_ms: now_ms,
                    })?;
                    report.released_items.push(item_id);
                }
                // HumanApproval (oder — praktisch unerreichbar — None):
                // legitim wartend, nicht anfassen.
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod git_branch_tests {
    //! `recover_git_branch` als Spezifikation (Befund 1 der Handprobe, plus
    //! die Nachbesserungen aus dem Code-Review): jeder der sieben Ausgänge
    //! einzeln, mit einem echten `git init`-Repo — dieselbe Fixture wie
    //! `git.rs`s eigene Unit-Tests.
    use super::*;
    use crate::model::{ProjectStatus, WorkBudget, WorkProject, WorkRun};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tmp_repo(name: &str) -> std::path::PathBuf {
        static NR: AtomicUsize = AtomicUsize::new(0);
        let nr = NR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agentkit_work_recovery_git_{name}_{}_{nr}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_with_run(
        workspace: &str,
        git_isolation: bool,
        base_branch: Option<&str>,
    ) -> WorkStore {
        let store = WorkStore::in_memory();
        store
            .submit(WorkEvent::ProjectCreated {
                project: WorkProject {
                    id: "demo".to_string(),
                    title: "Demo".to_string(),
                    objective: "Testen".to_string(),
                    workspace: workspace.to_string(),
                    status: ProjectStatus::Active,
                    created_at_ms: 0,
                    budget: WorkBudget::default(),
                    git_isolation,
                },
            })
            .unwrap();
        store
            .submit(WorkEvent::RunStarted {
                run: WorkRun {
                    id: "R-1".to_string(),
                    project_id: "demo".to_string(),
                    status: crate::model::RunStatus::Running,
                    started_at_ms: 0,
                    completed_at_ms: None,
                    base_revision: None,
                    base_branch: base_branch.map(str::to_string),
                    completion_reason: None,
                },
            })
            .unwrap();
        store
    }

    #[test]
    fn ohne_git_isolation_ist_inactive() {
        let dir = tmp_repo("ohne_isolation");
        let ws = dir.to_string_lossy().to_string();
        let base = crate::git::init_repo_with_commit(&ws);
        crate::git::ensure_item_branch(&ws, "work/demo/W-1", &base).unwrap();
        let store = store_with_run(&ws, false, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::Inactive
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ein Journal von vor dieser Korrektur kennt kein `base_branch` —
    /// `#[serde(default)]` liefert `None`, und `recover_git_branch` lässt den
    /// Git-Zustand dann unangetastet.
    #[test]
    fn ohne_bekannten_ausgangsbranch_ist_inactive() {
        let dir = tmp_repo("ohne_base_branch");
        let ws = dir.to_string_lossy().to_string();
        crate::git::init_repo_with_commit(&ws);
        let store = store_with_run(&ws, true, None);

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::Inactive
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schon_auf_dem_ausgangsbranch_ist_already_on_base() {
        let dir = tmp_repo("already_on_base");
        let ws = dir.to_string_lossy().to_string();
        crate::git::init_repo_with_commit(&ws);
        let store = store_with_run(&ws, true, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::AlreadyOnBase
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sauberer_eigener_item_branch_wird_zurueckgeholt() {
        let dir = tmp_repo("restored");
        let ws = dir.to_string_lossy().to_string();
        let base = crate::git::init_repo_with_commit(&ws);
        crate::git::ensure_item_branch(&ws, "work/demo/W-1", &base).unwrap();
        let store = store_with_run(&ws, true, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::Restored {
                from: "work/demo/W-1".to_string(),
                to: "main".to_string(),
            }
        );
        assert_eq!(crate::git::current_branch(&ws).unwrap(), "main");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regressionstest zum schwersten Befund des Code-Reviews: ein
    /// abgestürzter Versuch committet nie, uncommittete Änderungen auf dem
    /// Item-Branch sind also der Regelfall, nicht die Ausnahme. Ein
    /// automatischer Checkout hätte sie stillschweigend auf den
    /// Ausgangsbranch mitgenommen — stattdessen KEIN Checkout, klarer Befund.
    #[test]
    fn eigener_item_branch_mit_uncommitteten_aenderungen_wird_nicht_angefasst() {
        let dir = tmp_repo("dirty");
        let ws = dir.to_string_lossy().to_string();
        let base = crate::git::init_repo_with_commit(&ws);
        crate::git::ensure_item_branch(&ws, "work/demo/W-1", &base).unwrap();
        std::fs::write(dir.join("halbfertig.txt"), "nie committet").unwrap();
        let store = store_with_run(&ws, true, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::DirtyWorkingTree {
                current: "work/demo/W-1".to_string(),
                base: "main".to_string(),
            }
        );
        // Kein Checkout versucht: Branch UND Datei bleiben genau so stehen.
        assert_eq!(crate::git::current_branch(&ws).unwrap(), "work/demo/W-1");
        assert!(dir.join("halbfertig.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn voellig_fremder_branch_bleibt_unangetastet() {
        let dir = tmp_repo("foreign");
        let ws = dir.to_string_lossy().to_string();
        crate::git::init_repo_with_commit(&ws);
        std::process::Command::new("git")
            .args(["checkout", "-b", "feature/unabhaengig"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let store = store_with_run(&ws, true, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::ForeignBranch {
                current: "feature/unabhaengig".to_string(),
                base: "main".to_string(),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Die eigentliche Abgrenzung, die Befund 1 braucht: ein Item-Branch
    /// eines ANDEREN Projekts (`work/anderes-projekt/...`) ist für DIESES
    /// Projekt ein FREMDER Branch — er wird nicht als eigene Hinterlassenschaft
    /// missverstanden und deshalb nicht angefasst.
    #[test]
    fn item_branch_eines_anderen_projekts_gilt_als_fremd() {
        let dir = tmp_repo("foreign_project");
        let ws = dir.to_string_lossy().to_string();
        let base = crate::git::init_repo_with_commit(&ws);
        crate::git::ensure_item_branch(&ws, "work/anderes-projekt/W-1", &base).unwrap();
        let store = store_with_run(&ws, true, Some("main"));

        assert_eq!(
            recover_git_branch(&store, "R-1"),
            GitBranchRecovery::ForeignBranch {
                current: "work/anderes-projekt/W-1".to_string(),
                base: "main".to_string(),
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
