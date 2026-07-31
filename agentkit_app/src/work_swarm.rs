//! Der Schwarm-Executor für `agentkit_work` (Phase 6, §13 des Konzepts).
//!
//! `agentkit_work` bekommt KEINE Dependency auf `agentkit_swarm` (CLAUDE.md,
//! Einbahnrichtung) — der Kniff ist, dass der Port dafür schon existiert:
//! [`agentkit_work::AgentExecutor`]. Dieses Modul ist der komponierende
//! Executor, der beide Fälle kennt:
//!
//! - [`SwarmWorkExecutor`] führt EINEN Versuch über einen frisch gebauten,
//!   kurzlebigen Schwarm aus einer Vorlage aus — „Der Schwarm bearbeitet eine
//!   begrenzte Arbeitsphase. `agentkit-work` verwaltet das gesamte
//!   langfristige Vorhaben" (§13).
//! - [`DispatchingExecutor`] wählt je Versuch anhand von `pkg.item.executor`
//!   (`ExecutorKind`, gesetzt vom Operator beim Anlegen — NIE vom Modell,
//!   siehe `agentkit_work::tools::register_work_tools`-Doku) zwischen ihm und
//!   dem normalen [`CodingAgentExecutor`]. Fehlt der Schwarm (Feature aus wäre
//!   hier nie der Fall — `agentkit-swarm` ist eine Pflicht-Dependency dieses
//!   Crates — praktisch also nur `--no-swarm`), degradiert er EHRLICH auf den
//!   Einzelagenten, statt das Item scheitern zu lassen.
//!
//! Der Runner (`agentkit_work::runner::run_attempt`) sieht von alldem nichts:
//! er ruft weiterhin nur `&dyn AgentExecutor` — an ihm ändert sich für
//! Phase 6 nichts.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use agentkit::{
    builtin_roles, is_likely_destructive, Agent, AgentEvent, ApproveFn, Cancel, CodingTools,
    EventBus, EventData, Llm, Strategy, ToolRegistry, CODING_TOKEN_BUDGET, TOOL_RESULT,
};
use agentkit_swarm::{
    forward_swarm_events, CompletionPolicy, CompletionReason, SwarmBuilder, SwarmLimits,
};
use agentkit_work::{
    register_work_tools, AgentExecutor, AgentWorkPackage, CodingAgentExecutor, ExecutorKind,
    WorkStore, WorkToolCtx,
};

/// Protokoll-Fragment für jedes Schwarm-Mitglied. Eigenständig statt aus
/// `agentkit_swarm::dynamic` importiert: dessen `MEMBER_PROTOCOL` ist bewusst
/// privat (kein zweiter Nutzer außerhalb dieses Moduls war vorgesehen), und
/// ein kurzes, dupliziertes Prompt-Fragment ist hier die schmalere Lösung als
/// eine neue öffentliche Konstante in einem fremden Crate nur für diesen
/// einen Aufrufer (Guidelines §2/§4).
const MEMBER_PROTOCOL: &str = "Du bist Mitglied eines kurzlebigen Agenten-Schwarms, der GENAU \
EIN Work Item gemeinsam bearbeitet. Nachrichten anderer Mitglieder erreichen dich im Format \
[SWARM MESSAGE]; nutze 'swarm_peers'/'swarm_send'/'swarm_reply'/'swarm_broadcast' für die \
Zusammenarbeit und 'swarm_propose'/'swarm_vote', um euch auf ein gemeinsames Ergebnis zu \
einigen. Der angenommene Vorschlagstext IST das Ergebnis dieses Work Items — lege das \
vollständige Ergebnis hinein, nicht nur eine Zusammenfassung.";

/// Obergrenze für die Laufzeit EINES Schwarm-Versuchs, wenn das Vorhaben KEIN
/// `max_wall_time_secs`-Budget gesetzt hat (Befund 2 der Handprobe, §17/§28
/// des Konzepts). Ohne diese Grenze läuft ein Schwarm-Item unbegrenzt weiter —
/// in der Handprobe über vier Minuten, bis von Hand abgebrochen wurde — und
/// ignoriert damit vollständig das Budget-Konzept dieser Laufzeit. 15 Minuten,
/// dieselbe Größenordnung wie `agentkit_swarm::dynamic::SwarmLimits::
/// default().max_runtime_s`: DIE Grenze gilt für den zur Laufzeit selbst
/// gebauten `swarm`-Tool-Aufruf INNERHALB eines Agenten-Turns, diese hier für
/// einen GANZEN Work-Item-Versuch — unterschiedliche Aufrufer und
/// Konfigurationswege, deshalb eine eigene, unabhängig gepflegte Konstante
/// statt einer stillen Wiederverwendung, aber dieselbe Grundüberlegung ("ein
/// Multi-Agenten-Abschnitt braucht eine Obergrenze, wenn niemand explizit
/// eine gewählt hat"). Gilt NUR, wenn das Vorhaben kein `max_wall_time_secs`
/// konfiguriert hat — mit Budget bestimmt dessen Restlaufzeit die Grenze,
/// beliebig groß, wenn der Bediener sie so gewählt hat (siehe
/// `swarm_item_max_runtime_secs`).
const SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS: u64 = 900;

/// Ein Mitglied einer Schwarm-Vorlage: feste ID + Name einer eingebauten
/// Rolle (`agentkit::builtin_roles`).
struct TemplateMember {
    id: &'static str,
    role: &'static str,
}

/// Löst einen Vorlagennamen in seine Mitglieder auf (§13). GENAU ZWEI
/// Vorlagen, nicht mehr (Rule of Three: eine wäre keine Vorlage, drei wären
/// geraten):
///
/// - `discovery` — zwei lesende `explorer`-Mitglieder, die eine Frage aus
///   verschiedenen Richtungen angehen und sich auf einen Befund einigen.
/// - `review` — zwei prüfende `reviewer`-Mitglieder, Konsens über
///   Annahme/Ablehnung.
///
/// Beide GENAU zwei Mitglieder, mesh-verbunden (bei zwei Mitgliedern ist das
/// dieselbe einzelne Kante wie eine Kette) — das erreichbare Quorum ist damit
/// für beide Seiten exakt 1 (`CompletionPolicy::Consensus`).
fn template_members(template: &str) -> Result<Vec<TemplateMember>, String> {
    match template {
        "discovery" => Ok(vec![
            TemplateMember {
                id: "explorer-a",
                role: "explorer",
            },
            TemplateMember {
                id: "explorer-b",
                role: "explorer",
            },
        ]),
        "review" => Ok(vec![
            TemplateMember {
                id: "reviewer-a",
                role: "reviewer",
            },
            TemplateMember {
                id: "reviewer-b",
                role: "reviewer",
            },
        ]),
        other => Err(format!(
            "unbekannte Schwarm-Vorlage '{other}' — erlaubt sind: discovery, review"
        )),
    }
}

/// Effektive Laufzeitgrenze EINES Schwarm-Versuchs (Befund 2 der Handprobe) —
/// reine Funktion statt inline in `execute`, damit die Naht ohne einen echten
/// (Thread-startenden) Schwarm testbar ist: `remaining_wall_secs` kommt aus
/// `AgentWorkPackage` (gesetzt vom Runner aus dem Budget des Vorhabens, siehe
/// dessen Felddoku), `.max(1)` schützt vor `Duration::ZERO`, falls zwischen
/// `scheduler::decide` und diesem Aufruf ein winziger Rest an Sekunden
/// verstrichen ist.
fn swarm_item_max_runtime_secs(remaining_wall_secs: Option<u64>) -> u64 {
    remaining_wall_secs
        .unwrap_or(SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS)
        .max(1)
}

/// Führt einen Versuch über einen frisch gebauten Schwarm aus einer der oben
/// genannten Vorlagen aus.
pub struct SwarmWorkExecutor {
    pub llm: Arc<dyn Llm>,
    pub approve: ApproveFn,
    pub cancel: Cancel,
    pub dry_run: bool,
    pub shell_timeout: u64,
}

impl AgentExecutor for SwarmWorkExecutor {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        // Die Mitglieder laufen in eigenen Actor-Threads
        // (`agentkit_swarm::runtime`) und publizieren ihre `AgentEvent`s in
        // den Bus, den `SwarmBuilder::agent_bus` bekommt. `on_event` ist
        // `&mut dyn FnMut` (kein `Send`, an diesen Stack-Frame gebunden) und
        // kann deshalb nicht in einen Leser-Thread wandern — also wandert
        // umgekehrt das WARTEN: `join_with_cancel` läuft in einem eigenen
        // Thread, und dieser Frame liest den Bus leer, solange der Schwarm
        // arbeitet. Damit sieht der Runner die Turns der Mitglieder LIVE, und
        // sein Lease-Heartbeat (`runner::run_attempt` verlängert in genau
        // diesem Callback) greift während der ganzen Schwarm-Laufzeit.
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        let template = match &pkg.item.executor {
            ExecutorKind::Swarm { template } => template.clone(),
            ExecutorKind::SingleAgent => {
                return Err(
                    "SwarmWorkExecutor mit einem SingleAgent-Item aufgerufen (interner \
                     Dispatcher-Fehler in agentkit_app)"
                        .to_string(),
                );
            }
        };
        let members = template_members(&template)?;

        let coding = CodingTools::with_approve(
            &pkg.workspace,
            true,
            self.approve.clone(),
            self.shell_timeout,
        );
        let roles = builtin_roles();
        let limits = SwarmLimits::default();

        // Befund 2 der Handprobe: die Laufzeitgrenze kommt aus dem Budget des
        // VORHABENS, nicht aus `SwarmLimits::default()` (die für den
        // dynamischen `swarm`-Tool-Aufruf gedacht ist, siehe
        // `SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS`-Doku). Ist ein
        // `max_wall_time_secs`-Budget gesetzt, liefert `agentkit_work::runner`
        // die verbleibende RESTLAUFZEIT des Laufs über
        // `AgentWorkPackage::remaining_wall_secs` — das Arbeitspaket ist die
        // schon bestehende Naht dafür, `agentkit_work` muss dazu
        // `agentkit_swarm` nicht kennenlernen (CLAUDE.md, Einbahnrichtung).
        // Absichtlich die RESTZEIT, nicht das volle Budget: sonst könnte ein
        // einzelnes Schwarm-Item das Budget des gesamten Laufs allein
        // aufbrauchen. Ohne Budget gilt die dokumentierte Obergrenze, damit
        // ein Schwarm-Item nie unbegrenzt läuft.
        let max_runtime_secs = swarm_item_max_runtime_secs(pkg.remaining_wall_secs);

        // Der Bus, über den die Mitglieder ihre Ereignisse zurückgeben. Vor dem
        // Start abonnieren — ein Subscriber sieht nur, was NACH ihm publiziert
        // wird (siehe `agentkit::EventBus::subscribe`).
        let bus = EventBus::new();
        let member_events = bus.subscribe();

        let mut builder = SwarmBuilder::new(&format!("work-{}-{}", pkg.item.id, ctx.attempt_id))
            .completion(CompletionPolicy::Consensus {
                required_approvals: 1,
            })
            .mailbox_capacity(limits.mailbox_capacity)
            .max_messages(limits.max_messages)
            .max_hops(agentkit_swarm::DEFAULT_MAX_HOPS)
            .max_runtime(Duration::from_secs(max_runtime_secs))
            .agent_bus(bus.clone());

        for member in &members {
            let role = roles
                .iter()
                .find(|r| r.name == member.role)
                .expect("Vorlagen referenzieren ausschließlich eingebaute Rollen");

            let mut reg = ToolRegistry::new();
            match &role.tools {
                None => coding.register(&mut reg, None),
                Some(names) => {
                    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                    coding.register(&mut reg, Some(&refs));
                }
            }
            // Work-Tools DIREKT in die Registry jedes Mitglieds — derselbe
            // Weg wie `SwarmToolConfig::extra_member_tools`
            // (`agentkit_app::lib::FrontendTools::build`): ein
            // dynamisch/hier gebautes Schwarm-Mitglied baut seine Registry
            // von Grund auf und erbt nichts vom Aufrufer. Ohne das könnte
            // kein Mitglied sein Ergebnis über `work_artifact`/`work_submit`
            // abliefern (siehe Auftrag im Konzept — „ohne das ist Phase 6
            // wertlos"). `agent_id` bekommt den Mitgliedsnamen angehängt, damit
            // Provenance (`work_claim`) den wirklichen Autor zeigt, nicht nur
            // den einen Worker-Prozess.
            let mut member_ctx = ctx.clone();
            member_ctx.agent_id = format!("{}:{}", ctx.agent_id, member.id);
            register_work_tools(&mut reg, store.clone(), member_ctx);

            if self.dry_run {
                reg = reg.dry_run_blocking(is_likely_destructive);
            }

            // "Deine Agent-ID ist '…'" — dieselbe Zeile wie
            // `agentkit_swarm::dynamic::member_system`: das Modell kennt
            // damit seine eigene Kennung (z. B. um sie in `swarm_reply`
            // einzuordnen), und Tests können ein Mitglied an seinem
            // System-Prompt identifizieren, ohne eine eigene Test-Naht zu
            // brauchen (Muster `agentkit_swarm/tests/dynamic.rs::PerAgentLlm`).
            let system = format!(
                "{MEMBER_PROTOCOL}\n\nDeine Agent-ID ist '{}'.\n\n{}",
                member.id, role.system
            );
            let agent = Agent::builder(self.llm.clone())
                .tools(reg)
                .system(&system)
                .strategy(Strategy::React)
                .max_steps(pkg.max_steps as usize)
                .token_budget(CODING_TOKEN_BUDGET)
                .build();
            builder = builder.agent(member.id, agent);
        }
        // Beide Vorlagen haben genau zwei Mitglieder — eine einzelne Kante
        // verbindet sie vollständig (mesh == Kette bei n=2).
        for pair in members.windows(2) {
            builder = builder.connect_bidirectional(pair[0].id, pair[1].id);
        }

        let swarm = builder
            .build()
            .map_err(|e| format!("Schwarm-Aufbau fehlgeschlagen (Vorlage '{template}'): {e}"))?;
        let handle = swarm
            .start()
            .map_err(|e| format!("Schwarm-Start fehlgeschlagen (Vorlage '{template}'): {e}"))?;

        // Auch die Schwarm-Ebene (wer schickte wem was, Vorschläge, Stimmen)
        // spiegelt derselbe Forwarder wie beim dynamischen `swarm`-Tool auf den
        // Agent-Bus — sonst wäre ein Schwarm-Work-Item im Betrachter blind.
        // Das ABONNEMENT muss vor `send_initial` stehen (frühere Ereignisse
        // sieht ein neuer Subscriber nicht), der Thread darf danach starten —
        // so braucht der Fehlerpfad kein eigenes Aufräumen.
        let swarm_events = handle.events();

        let task = pkg.render();
        if let Err(e) = handle.send_initial(members[0].id, &task) {
            let _ = handle.stop();
            return Err(format!("Initialaufgabe nicht zustellbar: {e}"));
        }

        let done = Arc::new(AtomicBool::new(false));
        let forwarder = {
            let bus = bus.clone();
            let done = done.clone();
            std::thread::Builder::new()
                .name(format!("work-{}-swarm-events", pkg.item.id))
                .spawn(move || forward_swarm_events(swarm_events, bus, -1, done))
        };

        // `join_with_cancel`, nicht `join`: derselbe Stop-Knopf, den der
        // Runner auch dem Einzelagenten gibt, muss einen laufenden Schwarm
        // genauso beenden können (Ctrl-C während `agentkit work run`).
        // Im EIGENEN Thread, damit dieser Frame den Bus leer lesen kann,
        // während der Schwarm arbeitet (siehe `on_event`-Doku oben).
        let cancel = self.cancel.clone();
        let joiner = std::thread::Builder::new()
            .name(format!("work-{}-swarm-join", pkg.item.id))
            .spawn(move || handle.join_with_cancel(&cancel))
            .map_err(|e| format!("Thread-Start für den Schwarm-Abschluss: {e}"))?;

        while !joiner.is_finished() {
            match member_events.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => on_event(&ev),
                // Leerlauf ist der Normalfall (die Mitglieder hängen im
                // LLM-Stream); `Disconnected` kann nicht eintreten, solange
                // dieser Frame `bus` hält.
                Err(_) => continue,
            }
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(forwarder) = forwarder {
            let _ = forwarder.join();
        }
        // Nachzügler zwischen letztem Durchlauf und Abschluss noch weiterreichen
        // — erst NACH dem Forwarder-Join, damit auch dessen letzte Zeilen dabei sind.
        while let Ok(ev) = member_events.try_recv() {
            on_event(&ev);
        }
        let result = joiner
            .join()
            .map_err(|_| format!("Schwarm-Abschluss abgestürzt (Vorlage '{template}')"))?;
        // Das vollständige Ergebnis als eigener Datensatz — derselbe Abschluss,
        // den `dynamic::run_swarm` für den Tool-Aufruf schreibt. Ohne ihn hätte
        // ein Schwarm-WORK-ITEM im Betrachter kein Ende (keine Abstimmung, keine
        // Dead Letters, keine Turn-Bilanz).
        match serde_json::to_value(&result) {
            Ok(payload) => on_event(&AgentEvent::structured(
                agentkit_swarm::SWARM_RESULT_KIND,
                payload,
                -1,
                "",
            )),
            Err(e) => eprintln!("[WARN] Schwarm-Ergebnis nicht serialisierbar: {e}"),
        }
        match result.reason {
            // Konsens ist die Antwort des Versuchs — die Zahl der
            // Zustimmungen hängt an, damit sie in der Zusammenfassung
            // landet, statt nur intern zu bleiben.
            CompletionReason::Consensus {
                proposal,
                approvals,
            } => Ok(format!(
                "{}\n\n(Schwarm-Konsens: {approvals} Zustimmung(en))",
                proposal.content
            )),
            // Beide Limits laufen über denselben Sentinel wie ein
            // Einzelagent am Schrittlimit (`runner::record_failure` mit
            // `FailureKind::MaxSteps`) — ein bindender Verhaltenskontrakt des
            // Kerns, den dieser Executor nutzt statt eine eigene
            // Fehlerklassifikation zu erfinden.
            CompletionReason::MessageLimitReached | CompletionReason::MaxRuntimeReached => {
                Ok("(max_steps erreicht)".to_string())
            }
            // Leerlauf ist der Hänge-Schutz des Schwarms: die Mitglieder sind
            // verstummt, ohne je etwas vorzuschlagen. Für den Versuch heißt das
            // schlicht "keine Antwort" — nicht "Limit erreicht", denn kein
            // Budget war erschöpft. Dieser Sentinel führt zu
            // `FailureKind::InvalidOutput` und damit zu einem Wiederholungs-
            // versuch, was hier genau richtig ist.
            CompletionReason::Idle => Ok("(keine Antwort)".to_string()),
            CompletionReason::Stopped => Ok("(abgebrochen)".to_string()),
            CompletionReason::ActorFailure { agent, error } => Err(format!(
                "Schwarm-Mitglied '{agent}' abgestürzt (Vorlage '{template}'): {error}"
            )),
        }
    }
}

/// Wählt je Versuch zwischen dem Einzelagenten und dem Schwarm-Executor,
/// anhand von `pkg.item.executor` — die einzige Stelle, die beide Fälle
/// kennt. `agentkit_work` selbst trifft diese Entscheidung nicht (siehe
/// Moduldoku).
pub struct DispatchingExecutor {
    pub single: CodingAgentExecutor,
    /// `None`, wenn kein Schwarm verfügbar ist (praktisch: `--no-swarm` des
    /// Frontends) — ein Item mit `ExecutorKind::Swarm` scheitert dann NICHT,
    /// es degradiert auf den Einzelagenten (§13).
    pub swarm: Option<SwarmWorkExecutor>,
}

impl AgentExecutor for DispatchingExecutor {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        match (&pkg.item.executor, &self.swarm) {
            (ExecutorKind::Swarm { .. }, Some(swarm_executor)) => {
                swarm_executor.execute(pkg, ctx, store, on_event)
            }
            (ExecutorKind::Swarm { template }, None) => {
                // Ehrliche Degradation statt Abbruch. Die Note-Naht dafür ist
                // `on_event`: der Runner reicht jedes `AgentEvent` unverändert
                // als `WorkProgress::Agent` an den Aufrufer weiter
                // (`runner::run_attempt`), unabhängig von der CLI-Anzeigeoption
                // `--steps` — ein Aufrufer, der die Fortschritts-Events direkt
                // auswertet (wie der Test dieses Moduls), sieht die Meldung
                // immer. `agentkit_work` musste dafür nichts Neues anbieten:
                // dieser Kanal existiert schon seit Phase 3.
                on_event(&AgentEvent::new(
                    TOOL_RESULT,
                    EventData::ToolResult {
                        name: "work_dispatch".to_string(),
                        result: format!(
                            "Schwarm-Vorlage '{template}' angefordert, aber kein Schwarm \
                             verfügbar ('--no-swarm') — Einzelagent übernimmt."
                        ),
                    },
                ));
                self.single.execute(pkg, ctx, store, on_event)
            }
            (ExecutorKind::SingleAgent, _) => self.single.execute(pkg, ctx, store, on_event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ist ein Wall-Time-Budget gesetzt, gilt die vom Runner errechnete
    /// RESTLAUFZEIT — nicht die feste Obergrenze.
    #[test]
    fn max_runtime_nutzt_die_restlaufzeit_wenn_gesetzt() {
        assert_eq!(swarm_item_max_runtime_secs(Some(45)), 45);
    }

    /// Ohne Budget gilt die dokumentierte Obergrenze, damit ein Schwarm-Item
    /// nie unbegrenzt läuft (Befund 2 der Handprobe).
    #[test]
    fn max_runtime_faellt_ohne_budget_auf_die_konstante_zurueck() {
        assert_eq!(
            swarm_item_max_runtime_secs(None),
            SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS
        );
    }

    /// Eine bereits (fast) aufgebrauchte Restlaufzeit wird auf mindestens
    /// eine Sekunde angehoben statt `Duration::ZERO` zu erzeugen.
    #[test]
    fn max_runtime_klemmt_eine_erschoepfte_restlaufzeit_auf_eine_sekunde() {
        assert_eq!(swarm_item_max_runtime_secs(Some(0)), 1);
    }
}
