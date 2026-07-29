//! Das `swarm`-Tool — ein Agent baut sich seinen Schwarm zur Laufzeit selbst.
//!
//! Bisher war ein Schwarm eine Compile-Zeit-Entscheidung: der Aufrufer baut
//! jeden [`Agent`] in Rust und verdrahtet die Topologie. Hier kommt beides aus
//! dem Modell: der Orchestrator beschreibt in EINEM Tool-Aufruf, welche
//! Mitglieder er braucht (System-Prompt, Tool-Teilmenge, Skills, Strategie),
//! wer mit wem reden darf und wann der Schwarm fertig ist. Das Tool baut daraus
//! einen frischen Schwarm, lässt ihn bis zum Abschluss laufen und gibt dessen
//! Ergebnis als Tool-Result zurück.
//!
//! Strukturell ist das die Schwester von agentkits `roles.rs::add_task_tool`:
//! eine freie Funktion, kein neuer Typ-Baum, und pro Aufruf frische Agenten aus
//! dem geteilten LLM. Der Unterschied ist die Ebene — statt EINES Sub-Agenten
//! entsteht ein Netz gleichrangiger Agenten mit Mailboxen und Konsens.
//!
//! Zwei Invarianten, die den Rest des Designs tragen:
//!
//! - **Keine Rekursion.** Die Registry eines Mitglieds wird von Grund auf aus
//!   [`CodingTools`] gebaut und enthält weder `swarm` noch `task` — genau wie
//!   Sub-Agenten in agentkit nie das `task`-Tool bekommen.
//! - **Der Lauf gehört dem Orchestrator.** Bus und Stop-Knopf kommen aus dessen
//!   [`RunHandle`], nicht aus der Registrierzeit: die Turns der Mitglieder
//!   landen live im selben Event-Strom (getaggt mit `source` = Agent-ID), und
//!   Esc im TUI beendet den Schwarm.

use crate::completion::{CompletionPolicy, CompletionReason, SwarmResult};
use crate::events::SwarmEvent;
use crate::runtime::{SwarmBuilder, DEFAULT_MAX_HOPS};
use agentkit::coding::{CodingTools, HELPER_TOKEN_BUDGET, READ_ONLY_TOOLS};
use agentkit::events::{AgentEvent, EventBus, EventData, TOOL_RESULT};
use agentkit::llm::Llm;
use agentkit::{
    is_likely_destructive, parse_tools_field, strategy_from_str, truncate, Agent, AgentRole,
    McpHub, RunHandle, Skills, Strategy, ToolRegistry, SKILL_SYSTEM, SUBAGENT_MAX_STEPS,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Harte Obergrenzen für einen dynamisch erzeugten Schwarm. Das Modell darf
/// darunter bleiben, nie darüber — ein Schwarm sind N parallele Agent-Loops,
/// und die einzige Bremse gegen eine entgleiste Spezifikation ist diese Grenze.
#[derive(Clone, Debug)]
pub struct SwarmLimits {
    pub max_agents: usize,
    /// HARTE Obergrenze der Zustellungen — nicht der Regelwert. Den Default
    /// rechnet [`MESSAGES_PER_AGENT`] aus der Mitgliederzahl aus.
    pub max_messages: usize,
    /// Obergrenze der Laufzeit. `None` = KEINE — ein Schwarm darf beliebig lange
    /// arbeiten, solange er arbeitet. Der Hänge-Schutz ist der Leerlauf
    /// ([`max_idle_s`](Self::max_idle_s)), nicht die Wanduhr: die knappe
    /// Ressource ist das Modell-Kontingent, nicht die Zeit. Ein vom Modell
    /// gesetztes `max_laufzeit_s` gilt trotzdem und wird hierauf gedeckelt.
    pub max_runtime_s: Option<u64>,
    /// Nach so langer Untätigkeit endet der Schwarm ([`CompletionReason::Idle`]).
    pub max_idle_s: u64,
    /// Abstimmungsfrist in Sekunden nach Erreichen des Quorums; `0` = sofort
    /// entscheiden (siehe `runtime::DEFAULT_VOTE_WINDOW`).
    pub vote_window_s: u64,
    /// Loop-Schritte je Mitglied und Nachricht.
    pub max_steps: usize,
    pub mailbox_capacity: usize,
}

/// Nachrichten-Budget je Mitglied; daraus entsteht der Default von
/// `max_nachrichten`.
///
/// Warum nicht der frühere feste Wert (60): eine Zustellung ist die Einheit,
/// nicht eine Konversation — ein `swarm_broadcast` kostet eine Einheit PRO
/// Nachbar. Im Mesh zahlt also jeder Turn n-1. Bei vier Mitgliedern reichten 60
/// Zustellungen für rund 20 Turns im GANZEN Schwarm (fünf pro Mitglied), und der
/// Schwarm starb am Limit, bevor irgendjemand fertig recherchiert hatte. Mit der
/// Mitgliederzahl zu skalieren hält den Spielraum pro Mitglied konstant, statt
/// ihn mit jedem zusätzlichen Mitglied zu dritteln.
pub const MESSAGES_PER_AGENT: usize = 120;

impl Default for SwarmLimits {
    fn default() -> Self {
        SwarmLimits {
            max_agents: 6,
            max_messages: 2_000,
            max_runtime_s: None,
            max_idle_s: 300,
            vote_window_s: 20,
            // Dieselbe Luft wie ein Sub-Agent — eine Quelle, kein zweites Literal.
            max_steps: SUBAGENT_MAX_STEPS,
            mailbox_capacity: crate::DEFAULT_MAILBOX_CAPACITY,
        }
    }
}

/// Was der Schwarm-Baukasten von seinem Frontend braucht. Entspricht der
/// Parameterliste von agentkits `add_task_tool`, nur als Struct — die Liste
/// wäre sonst zehn Positionen lang.
pub struct SwarmToolConfig {
    /// Lauf-Kontext des Orchestrators (Bus + Stop-Knopf zur Laufzeit).
    pub run: RunHandle,
    /// Geteiltes LLM aller Mitglieder.
    pub llm: Arc<dyn Llm>,
    /// Die Sandbox-Tools des Orchestrators — dieselbe Instanz für alle Mitglieder;
    /// darin stecken Workspace, Freigabe-Callback und Shell-Timeout.
    pub coding: CodingTools,
    /// Skills-Verzeichnis des Frontends; Mitglieder fordern es per `skills: true` an.
    pub skills: Option<Skills>,
    /// Vordefinierte Rollen (`--agents DIR`) — im Spec per Name referenzierbar.
    pub roles: Vec<AgentRole>,
    /// Geteilter MCP-Hub; Mitglieder bekommen die beim Bau aktiven Server-Tools.
    pub mcp: Arc<McpHub>,
    /// `--dry-run` des Orchestrators: gilt für die fertige Registry jedes
    /// Mitglieds. Ein Schwarm darf nicht schreiben dürfen, was sein Erzeuger nicht darf.
    pub dry_run: bool,
    pub limits: SwarmLimits,
    /// Zusätzliche Tools für JEDES Mitglied, mit dessen Agent-ID aufgerufen.
    ///
    /// Der Schwarm weiß nicht, was darin registriert wird — er kennt genauso wenig
    /// den Wissensgraphen wie agentkit den Schwarm kennt. Nötig ist die Naht, weil
    /// Mitglieder ihre Registry hier gebaut bekommen und die Tools des
    /// Orchestrators deshalb nicht erben: ohne sie hätte der Erzeuger eines
    /// Schwarms Fähigkeiten, die seine Mitglieder nicht haben.
    ///
    /// Die Agent-ID geht mit, damit ein Tool den echten Autor kennt — dasselbe
    /// Prinzip wie bei `swarm_send`, wo `from` immer aus dem Kontext kommt.
    pub extra_member_tools: Option<ExtraMemberTools>,
    /// Token-Budget für einen verwalteten Kontext JE MITGLIED (ctxman), oder
    /// `None`. Kommt aus `ExtraToolCtx::helper_ctx_budget` des Erzeugers: ein
    /// Schwarm, dessen Orchestrator sein Kontext-Management hat, dessen
    /// Mitglieder aber nicht, hat das Problem nur verschoben. Ohne Feature
    /// `ctxman` wird der Wert ignoriert.
    pub helper_ctx_budget: Option<u32>,
}

/// Siehe [`SwarmToolConfig::extra_member_tools`].
pub type ExtraMemberTools = Arc<dyn Fn(&mut ToolRegistry, &str) + Send + Sync>;

/// Prompt-Fragment für den Orchestrator — anzuhängen, wenn das `swarm`-Tool
/// registriert ist (Gegenstück zu agentkits `SUBAGENT_SYSTEM`).
pub const SWARM_SYSTEM: &str = "Für Aufgaben, die mehrere gleichrangige Perspektiven \
brauchen, die MITEINANDER reden müssen (Entwurf ↔ Kritik ↔ Test, kontroverse Bewertungen, \
Aushandeln eines gemeinsamen Ergebnisses), kannst du mit dem Tool 'swarm' zur Laufzeit einen \
Agenten-Schwarm erzeugen: du legst Mitglieder, System-Prompts, Tool-Zugriff und Topologie \
selbst fest. Der Aufruf blockiert, bis der Schwarm per Konsens (oder an einem Limit) endet, \
und liefert dir das Ergebnis; DU fasst es anschließend für den Nutzer zusammen.\n\
Wähle bewusst:\n\
- Eine abgegrenzte Teilaufgabe, die EIN Spezialist allein erledigt -> nimm 'task', nicht 'swarm'.\n\
- Etwas, das du selbst schnell erledigst -> mach es selbst.\n\
- Erst wenn die Agenten einander brauchen (Antworten, Kritik, Abstimmung), lohnt 'swarm'.\n\
Halte den Schwarm klein (2-4 Mitglieder), gib jedem Mitglied eine klare, unterschiedliche \
Rolle und formuliere im 'auftrag', woran der Schwarm erkennt, dass er fertig ist. \
Schreibzugriff bekommen Mitglieder nur, wenn du ihn ausdrücklich anforderst — mehrere \
schreibende Mitglieder teilen sich EINEN Workspace.";

/// Fester Teil des System-Prompts jedes Mitglieds: das Schwarm-Protokoll. Der
/// rollenspezifische Teil (vom Modell oder aus einer Rolle) kommt dahinter.
const MEMBER_PROTOCOL: &str = "Du bist Mitglied eines Agenten-Schwarms. Eingehende \
Nachrichten erreichen dich im Format [SWARM MESSAGE]; deine Werkzeuge dafür:\n\
- swarm_peers: mit wem du direkt reden darfst\n\
- swarm_send: Nachricht an einen Nachbarn (fire-and-forget; die Antwort kommt später als \
neue Nachricht bei dir an)\n\
- swarm_reply: dem Absender der Nachricht antworten, die du gerade bearbeitest\n\
- swarm_broadcast: an alle Nachbarn\n\
- swarm_propose: den Schwarm-Auftrag als erledigt vorschlagen (mit dem ERGEBNIS als Text)\n\
- swarm_vote: über einen fremden Vorschlag abstimmen\n\
Regeln: Antworte nie ins Leere — wer etwas von dir wollte, bekommt eine Antwort per \
swarm_reply. AUSNAHME ist die Initialaufgabe (Absender 'runtime'): die kommt von der Laufzeit, \
dort gibt es niemanden zum Antworten — verteile sie stattdessen mit swarm_send/swarm_broadcast \
an deine Nachbarn. Schlage den Abschluss erst vor, wenn das Ergebnis inhaltlich steht, und lege \
das vollständige Ergebnis in den Vorschlag (nur dieser Text wird zurückgegeben). Stimmst du \
einem Vorschlag zu, nutze swarm_vote mit der vorschlag_id aus der Nachricht. Meldet ein Send \
'limit_erreicht', ist das Nachrichtenbudget aufgebraucht: schließe dann mit swarm_propose ab, \
Vorschläge und Stimmen zählen nicht gegen das Limit.\n\
Sparsam arbeiten: Finde Stellen mit glob_files/grep und lies mit read_file nur, was du \
wirklich brauchst — dein Gedächtnis bleibt über ALLE Nachrichten hinweg erhalten, jede \
gelesene Datei belastet also jeden weiteren Zug. Und was du verschickst, landet im Kontext \
deiner Nachbarn: schicke Befunde KOMPAKT (Pfade mit Zeilen, Kernaussagen), niemals ganze \
Dateiinhalte.";

/// Der Ergebnis-Schema-Block für den Mitglieds-Prompt, oder leer.
///
/// Warum das mehr bringt als eine Ermahnung: ein Mitglied hat von sich aus nur
/// SEINEN Teil. In einem echten Lauf reichten zwei Mitglieder je ihre eigene
/// Perspektive als Abschluss ein — nicht aus Nachlässigkeit, sondern weil keines
/// wusste, wie ein vollständiges Ergebnis aussieht. Ein deklariertes Schema
/// macht die Lücke sichtbar: wer ein Feld nicht füllen kann, weiß, dass er
/// jemanden fragen muss.
fn ergebnis_schema_block(schema: Option<&Value>) -> String {
    let Some(schema) = schema else {
        return String::new();
    };
    // Ein String kommt roh, alles andere als lesbares JSON.
    let text = match schema.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string()),
    };
    if text.trim().is_empty() {
        return String::new();
    }
    format!(
        "

Das Gesamtergebnis des Schwarms hat diese Gestalt — ein Abschluss-Vorschlag muss sie VOLLSTÄNDIG ausfüllen, auch die Teile, die andere Mitglieder beigetragen haben:
{text}"
    )
}

/// Generischer Mitglieds-Prompt, wenn weder `system` noch `rolle` gesetzt sind.
///
/// Bewusst so knapp gehalten wie `EXPLORER_SYS` bei den Sub-Agenten: ein
/// Schwarm-Mitglied ohne eigene Rolle hatte bisher keinerlei Sparsamkeits-Vorgabe
/// und las Repos in Gänze ein.
const GENERIC_MEMBER: &str =
    "Erledige deinen Teil des Schwarm-Auftrags gründlich und knapp und arbeite \
konstruktiv mit den anderen Mitgliedern zusammen. Liefere Ergebnisse als kompakte \
Zusammenfassung, nicht als Materialsammlung.";

// --------------------------------------------------------------- Spezifikation

/// Die Schwarm-Spezifikation, wie das Modell sie liefert.
#[derive(Debug, Deserialize)]
struct SwarmSpec {
    auftrag: String,
    agenten: Vec<AgentSpec>,
    #[serde(default)]
    topologie: Option<String>,
    #[serde(default)]
    verbindungen: Vec<Vec<String>>,
    #[serde(default)]
    start_agent: Option<String>,
    #[serde(default)]
    erforderliche_zustimmungen: Option<usize>,
    #[serde(default)]
    max_nachrichten: Option<usize>,
    #[serde(default)]
    max_laufzeit_s: Option<u64>,
    /// Gestalt des erwarteten GESAMTergebnisses — siehe [`ergebnis_schema_block`].
    #[serde(default)]
    ergebnis_schema: Option<Value>,
}

/// Ein Mitglied der Spezifikation.
#[derive(Debug, Deserialize)]
struct AgentSpec {
    id: String,
    #[serde(default)]
    rolle: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    tools: Option<String>,
    #[serde(default)]
    strategie: Option<String>,
    #[serde(default)]
    skills: bool,
}

/// Weicher Fehler ans Modell (kein `Err` — der Orchestrator soll die
/// Spezifikation selbst korrigieren, ohne dass ein ERROR-Event feuert).
fn soft(msg: impl std::fmt::Display) -> Result<String, String> {
    Ok(format!("ERROR: {msg}"))
}

// ------------------------------------------------------------------ Topologie

/// Löst `topologie`/`verbindungen` in konkrete Kantenpaare auf.
/// Explizite `verbindungen` gewinnen; ohne beides gilt `mesh`.
fn resolve_edges(spec: &SwarmSpec, ids: &[String]) -> Result<Vec<(String, String)>, String> {
    if !spec.verbindungen.is_empty() {
        let mut out = Vec::new();
        for pair in &spec.verbindungen {
            let [a, b] = pair.as_slice() else {
                return Err(format!(
                    "'verbindungen' erwartet Paare [von, nach], bekam {pair:?}"
                ));
            };
            // Wie die IDs selbst getrimmt vergleichen — `ids` sind es bereits.
            let (a, b) = (a.trim().to_string(), b.trim().to_string());
            for endpoint in [&a, &b] {
                if !ids.contains(endpoint) {
                    return Err(format!(
                        "Verbindung verweist auf unbekannte Agent-ID '{endpoint}'"
                    ));
                }
            }
            if a == b {
                return Err(format!("Verbindung von '{a}' auf sich selbst"));
            }
            out.push((a, b));
        }
        return Ok(out);
    }
    let preset = spec
        .topologie
        .as_deref()
        .unwrap_or("mesh")
        .trim()
        .to_lowercase();
    let mut out = Vec::new();
    match preset.as_str() {
        "mesh" | "vollvermascht" => {
            for (i, a) in ids.iter().enumerate() {
                for b in &ids[i + 1..] {
                    out.push((a.clone(), b.clone()));
                }
            }
        }
        "kette" => {
            for pair in ids.windows(2) {
                out.push((pair[0].clone(), pair[1].clone()));
            }
        }
        "stern" => {
            for b in &ids[1..] {
                out.push((ids[0].clone(), b.clone()));
            }
        }
        other => {
            return Err(format!(
                "unbekannte 'topologie' '{other}' — erlaubt: mesh, kette, stern \
                 (oder 'verbindungen' als Liste von Paaren)"
            ))
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ Mitglieder bauen

/// Tool-Teilmenge eines Mitglieds. Rangfolge: explizites `tools` > Rolle >
/// **read_only als Default** — bewusst enger als bei agentkit-Rollen (dort heißt
/// „nichts angegeben" = alle Tools): ein vom Modell erfundener Agent bekommt
/// Schreibrechte nur, wenn sie ausdrücklich verlangt wurden.
fn member_tools(spec: &AgentSpec, role: Option<&AgentRole>) -> Option<Vec<String>> {
    match spec.tools.as_deref().map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("alle") || s.eq_ignore_ascii_case("all") => None,
        Some(s) if !s.is_empty() => parse_tools_field(Some(s)),
        // Eine benannte Rolle bringt ihre eigene Teilmenge mit (auch „alle").
        _ => match role {
            Some(r) => r.tools.clone(),
            None => Some(READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect()),
        },
    }
}

/// System-Prompt eines Mitglieds: Protokoll + Identität/Nachbarn + Rolle.
///
/// `id` ist die GEPRÜFTE (getrimmte) Kennung, nicht `spec.id`: unter der ist das
/// Mitglied im Schwarm registriert, und genau die erwarten `swarm_send` & Co. Ein
/// `" architekt "` aus der Spezifikation hätte sonst einen Prompt ergeben, der
/// eine andere ID nennt als `swarm_peers` liefert.
fn member_system(
    spec: &AgentSpec,
    id: &str,
    role: Option<&AgentRole>,
    peers: &[String],
    ergebnis_schema: Option<&Value>,
) -> String {
    let own = spec
        .system
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| role.map(|r| r.system.clone()))
        .unwrap_or_else(|| GENERIC_MEMBER.to_string());
    let nachbarn = if peers.is_empty() {
        "niemandem (du kannst nur empfangen sowie vorschlagen/abstimmen)".to_string()
    } else {
        peers.join(", ")
    };
    let schema = ergebnis_schema_block(ergebnis_schema);
    format!(
        "{MEMBER_PROTOCOL}{schema}\n\nDeine Agent-ID ist '{id}'. Du kannst direkt reden mit: {nachbarn}.\n\n{own}"
    )
}

/// Baut EIN Mitglied: Tool-Teilmenge + MCP + optionale Skills, System-Prompt aus
/// Protokoll und Rolle. Die Registry entsteht von Grund auf aus [`CodingTools`] —
/// dadurch enthält sie strukturell weder `swarm` noch `task` (keine Rekursion).
fn build_member(
    spec: &AgentSpec,
    id: &str,
    peers: &[String],
    cfg: &SwarmToolConfig,
    ergebnis_schema: Option<&Value>,
) -> Agent {
    let role = spec
        .rolle
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .and_then(|name| cfg.roles.iter().find(|r| r.name == name));

    let mut reg = ToolRegistry::new();
    match member_tools(spec, role) {
        None => cfg.coding.register(&mut reg, None),
        Some(names) => {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            cfg.coding.register(&mut reg, Some(&refs));
        }
    }
    cfg.mcp.register_enabled(&mut reg);
    // Frontend-Tools des Erzeugers (heute: der Wissensgraph) — VOR `dry_run_blocking`,
    // damit auch sie geblockt werden, wenn der Orchestrator trocken läuft.
    if let Some(extra) = &cfg.extra_member_tools {
        extra(&mut reg, id);
    }

    let mut system = member_system(spec, id, role, peers, ergebnis_schema);
    if spec.skills {
        if let Some(skills) = &cfg.skills {
            skills.register(&mut reg);
            system.push_str("\n\n");
            system.push_str(SKILL_SYSTEM);
        }
    }
    if cfg.dry_run {
        reg = reg.dry_run_blocking(is_likely_destructive);
    }

    let strategy = match spec
        .strategie
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => strategy_from_str(s),
        None => role.map(|r| r.strategy).unwrap_or(Strategy::React),
    };

    // Eigener, nicht persistenter ctxman-Kontext je Mitglied. Wichtiger als beim
    // Sub-Agenten: das Gedächtnis eines Mitglieds bleibt über ALLE Nachrichten
    // erhalten, sein Kontext wächst also über den ganzen Schwarm-Lauf.
    #[cfg(feature = "ctxman")]
    let kontext = cfg
        .helper_ctx_budget
        .and_then(|b| agentkit::ManagedContext::ephemeral(b, cfg.llm.clone()).ok());
    // `expand_context_ref` muss VOR dem Bau in die Registry — der Agent kopiert sie.
    #[cfg(feature = "ctxman")]
    if let Some(ctx) = &kontext {
        let _ = ctx.set_system(&system);
        ctx.register_tool(&mut reg);
    }

    #[allow(unused_mut)]
    let mut agent = Agent::builder(cfg.llm.clone())
        .tools(reg)
        .system(&system)
        .strategy(strategy)
        .max_steps(cfg.limits.max_steps)
        .token_budget(HELPER_TOKEN_BUDGET)
        .build();
    #[cfg(feature = "ctxman")]
    {
        agent.context = kontext;
    }
    agent
}

// ------------------------------------------------------ Schwarm-Verkehr im TUI

/// Übersetzt ein [`SwarmEvent`] in eine Zeile für den Agent-Event-Strom:
/// `(Agent-ID als source, Text)`. `None` = für den Nutzer uninteressant.
///
/// Bewusst KEIN neuer AgentEvent-Typ: ein neuer Typ zöge Änderungen im
/// CLI-Renderer und im TUI nach sich, obwohl ein Tool-Ergebnis die Information
/// genauso trägt (im TUI: `[coder] ↳ schwarm: …`).
fn swarm_event_line(event: &SwarmEvent) -> Option<(String, String)> {
    // Kurz halten: die Zeilen laufen live durchs Transcript.
    let kurz = |s: &str| truncate(&s.replace('\n', " "), 240);
    match event {
        SwarmEvent::MessageQueued { message } => Some((
            message.from.clone(),
            format!(
                "{} → {} ({}): {}",
                message.from,
                match &message.to {
                    crate::Recipient::Agent(id) => id.clone(),
                    crate::Recipient::Broadcast => "alle".to_string(),
                },
                message.kind.as_str(),
                kurz(&message.content)
            ),
        )),
        SwarmEvent::MessageRejected { message, result } => Some((
            message.from.clone(),
            format!(
                "nicht zugestellt ({}): {} → {:?}",
                result.status_str(),
                message.from,
                message.to
            ),
        )),
        SwarmEvent::TurnStarted { agent, message_id } => {
            Some((agent.clone(), format!("{agent} bearbeitet {message_id}")))
        }
        SwarmEvent::ProposalCreated { message } => Some((
            message.from.clone(),
            format!(
                "Abschluss-Vorschlag {} von {}: {}",
                message.id,
                message.from,
                kurz(&message.content)
            ),
        )),
        SwarmEvent::VoteSubmitted { message } => Some((
            message.from.clone(),
            format!(
                "{} stimmt über {} ab: {}",
                message.from,
                message.correlation_id.as_deref().unwrap_or("?"),
                kurz(&message.content)
            ),
        )),
        SwarmEvent::ActorFailed { agent, error } => Some((
            agent.clone(),
            format!("Agent '{agent}' abgestürzt: {error}"),
        )),
        SwarmEvent::SwarmCompleted { reason } => Some((
            String::new(),
            format!("Schwarm beendet: {}", reason_label(reason)),
        )),
        // Lifecycle-Rauschen; die Turns der Mitglieder sieht der Nutzer ohnehin
        // über deren eigene AgentEvents.
        SwarmEvent::ActorStarted { .. }
        | SwarmEvent::ActorStopped { .. }
        | SwarmEvent::MessageDequeued { .. }
        | SwarmEvent::TurnCompleted { .. } => None,
    }
}

/// Spiegelt Schwarm-Events auf den Agent-Bus, bis `done` gesetzt ist und die
/// Queue leer läuft. `recv_timeout`-Takt wie überall im Crate: der Abschluss
/// darf nicht davon abhängen, dass der letzte `SwarmEventBus`-Klon fällt.
fn forward_swarm_events(
    rx: std::sync::mpsc::Receiver<SwarmEvent>,
    bus: EventBus,
    task_id: i64,
    done: Arc<AtomicBool>,
) {
    let publish = |event: &SwarmEvent| {
        if let Some((source, text)) = swarm_event_line(event) {
            bus.publish(AgentEvent::with_meta(
                TOOL_RESULT,
                EventData::ToolResult {
                    name: "schwarm".to_string(),
                    result: text,
                },
                task_id,
                source,
            ));
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => publish(&event),
            Err(_) if done.load(Ordering::SeqCst) => break,
            Err(_) => continue,
        }
    }
    // Nachzügler zwischen letztem Empfang und Abschluss noch mitnehmen.
    while let Ok(event) = rx.try_recv() {
        publish(&event);
    }
}

// ---------------------------------------------------------------- Ergebnis

fn reason_label(reason: &CompletionReason) -> &'static str {
    match reason {
        CompletionReason::Consensus { .. } => "konsens",
        CompletionReason::MessageLimitReached => "nachrichtenlimit",
        CompletionReason::MaxRuntimeReached => "laufzeitlimit",
        CompletionReason::Idle => "leerlauf",
        CompletionReason::ActorFailure { .. } => "actor_fehler",
        CompletionReason::Stopped => "abgebrochen",
    }
}

/// Das Tool-Ergebnis für den Orchestrator: deutsches JSON, weiche Fehler als
/// Werte. `SwarmResult` selbst bleibt serde-frei — ein `Serialize` dort hätte
/// heute genau einen Nutzer und würde die Laufzeit-Typen an ein Format binden.
fn render_result(result: &SwarmResult) -> String {
    // Die Abstimmung gehört ins Ergebnis, nicht nur der Text des Gewinners: in
    // einem echten Lauf reichten ZWEI Mitglieder konkurrierende Vorschläge ein
    // und ein drittes — das fleißigste — stimmte nie ab. Aus dem Ergebnis war
    // davon nichts zu sehen, und aus dem Trace ebenso wenig.
    let vorschlaege: Vec<Value> = result
        .proposals
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "von": p.from,
                "zustimmungen": p.approvals,
                "angenommen": p.accepted,
            })
        })
        .collect();
    let mut out = json!({
        "status": reason_label(&result.reason),
        "nachrichten": result.messages_sent,
        "unzustellbar": result.dead_letters.len(),
        "turns": result.turns.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "abstimmung": {
            "erforderlich": result.required_approvals,
            "vorschlaege": vorschlaege,
        },
    });
    match &result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            out["ergebnis"] = json!(proposal.content);
            out["zustimmungen"] = json!(approvals);
        }
        CompletionReason::MessageLimitReached => {
            out["hinweis"] = json!(
                "Das Nachrichtenlimit war erschöpft und die bereits zugestellte Arbeit ist \
                 abgearbeitet, ohne dass ein Vorschlag angenommen wurde. Fasse zusammen, was \
                 du hast, oder starte einen kleineren Schwarm bzw. einen mit mehr \
                 'max_nachrichten'."
            );
        }
        CompletionReason::MaxRuntimeReached => {
            out["hinweis"] = json!(
                "Die Laufzeitgrenze wurde erreicht, ohne dass ein Vorschlag angenommen wurde. \
                 Enger fassen oder weniger Mitglieder."
            );
        }
        CompletionReason::Idle => {
            out["hinweis"] = json!(
                "Der Schwarm hat eine Weile nichts mehr getan, ohne dass ein Vorschlag                  angenommen wurde — die Mitglieder haben aufgehört zu arbeiten, statt                  mit swarm_propose abzuschließen. Fasse zusammen, was du hast, oder                  formuliere den Auftrag so, dass klar ist, woran der Schwarm sein Ende                  erkennt."
            );
        }
        CompletionReason::ActorFailure { agent, error } => {
            out["hinweis"] = json!(format!(
                "Der Agent '{agent}' ist abgestürzt ({error}); der Schwarm wurde beendet."
            ));
        }
        CompletionReason::Stopped => {
            out["hinweis"] = json!("Der Lauf wurde abgebrochen (Stop-Knopf des Nutzers).");
        }
    }
    out.to_string()
}

// ------------------------------------------------------------------ Das Tool

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "auftrag": {
                "type": "string",
                "description": "Die Mission des Schwarms — inklusive der Frage, woran der Schwarm erkennt, dass er fertig ist."
            },
            "agenten": {
                "type": "array",
                "description": "Die Mitglieder (2-4 sind der Regelfall). Gib jedem eine klar unterschiedliche Aufgabe.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Kurze eindeutige ID, z. B. 'architekt'."},
                        "system": {"type": "string", "description": "System-Prompt dieses Mitglieds (wer es ist, was es beiträgt)."},
                        "rolle": {"type": "string", "description": "Optional: Name einer vordefinierten Rolle statt eines eigenen System-Prompts."},
                        "tools": {"type": "string", "description": "Tool-Zugriff: 'read_only' (Default), 'alle' oder eine Komma-Liste von Tool-Namen."},
                        "strategie": {"type": "string", "enum": ["react", "plan", "plain"], "description": "Strategie dieses Mitglieds (Default: react)."},
                        "skills": {"type": "boolean", "description": "Skills-Werkzeuge dazugeben (nur wirksam, wenn das Frontend Skills konfiguriert hat)."}
                    },
                    "required": ["id"]
                }
            },
            "topologie": {
                "type": "string",
                "enum": ["mesh", "kette", "stern"],
                "description": "Wer mit wem reden darf. mesh = alle mit allen (Default), kette = der Reihe nach, stern = das erste Mitglied mit allen anderen."
            },
            "verbindungen": {
                "type": "array",
                "description": "Alternative zu 'topologie': explizite Paare, z. B. [[\"a\",\"b\"],[\"b\",\"c\"]]. Verbindungen gelten in beide Richtungen.",
                "items": {"type": "array", "items": {"type": "string"}}
            },
            "start_agent": {"type": "string", "description": "Wer den Auftrag zuerst bekommt (Default: das erste Mitglied)."},
            "erforderliche_zustimmungen": {
                "type": "integer",
                "description": "Wie viele Nachbarn einem Abschluss-Vorschlag zustimmen müssen. Obergrenze sind die Nachbarn des am schwächsten verbundenen Mitglieds — nur wer einen Vorschlag sieht, kann über ihn abstimmen. Default ist die Mehrheit davon; setze den Wert nur, wenn du Einstimmigkeit brauchst."
            },
            "max_nachrichten": {"type": "integer", "description": "Obergrenze der Zustellungen im Schwarm (Default: 40 je Mitglied). Ein Broadcast kostet eine Zustellung pro Nachbar."},
            "ergebnis_schema": {
                "description": "Gestalt des erwarteten GESAMTergebnisses (JSON-Objekt oder Beschreibungstext). Steht im Prompt JEDES Mitglieds. Dringend empfohlen, sobald der Auftrag mehrere Perspektiven verlangt: ohne Schema kennt ein Mitglied nur seinen eigenen Teil und schlägt genau den als Abschluss vor — konkurrierende Teilvorschläge statt einer gemeinsamen Synthese."
            },
            "max_laufzeit_s": {"type": "integer", "description": "Harte Obergrenze der Laufzeit in Sekunden. Normalerweise WEGLASSEN — dann arbeitet der Schwarm so lange, wie er produktiv ist, und endet über Konsens, Nachrichtenbudget oder Leerlauf. Nur setzen, wenn der Lauf wirklich nach einer festen Zeit vorbei sein muss."}
        },
        "required": ["auftrag", "agenten"]
    })
}

/// Registriert das `swarm`-Tool in `registry`.
///
/// Jeder Aufruf baut aus der Spezifikation des Modells einen FRISCHEN Schwarm,
/// startet ihn, spiegelt seinen Verkehr in den Event-Strom des Orchestrators und
/// blockiert bis zum Abschluss (Konsens, Limit, Absturz oder Abbruch). Danach
/// existiert der Schwarm nicht mehr — es gibt keinen Zustand über Aufrufe hinweg.
///
/// Muss VOR `AgentBuilder::build()` registriert werden und derselbe
/// [`RunHandle`] muss an den Builder gehen; sonst sieht das Tool zur Laufzeit
/// weder Bus noch Stop-Knopf (siehe agentkits `RunHandle`-Vertrag).
pub fn add_swarm_tool(registry: &mut ToolRegistry, cfg: SwarmToolConfig) {
    let cfg = Arc::new(cfg);
    // Fortlaufende Nummer je Aufruf: eindeutiger Schwarm-Name und `task_id`,
    // damit Consumer parallele Schwärme auseinanderhalten können.
    let seq = Arc::new(AtomicI64::new(1));

    registry.add(
        "swarm",
        "Erzeugt zur Laufzeit einen Agenten-Schwarm: du legst Mitglieder, System-Prompts, \
         Tool-Zugriff und Topologie fest. Die Mitglieder reden peer-to-peer miteinander und \
         schließen per Abstimmung ab. Der Aufruf blockiert bis zum Ergebnis. Für eine \
         abgegrenzte Teilaufgabe an EINEN Spezialisten nimm stattdessen 'task'.",
        schema(),
        move |args: Value| run_swarm(&cfg, &seq, args),
    );
}

/// Eine geprüfte Spezifikation: alles, was der Bau noch braucht, in gültiger Form.
struct Geprueft {
    auftrag: String,
    ids: Vec<String>,
    edges: Vec<(String, String)>,
    start_agent: String,
    quorum: usize,
    max_messages: usize,
    max_runtime_s: Option<u64>,
}

/// Prüft die Spezifikation und deckelt sie auf die [`SwarmLimits`]. Fehler sind
/// Texte fürs Modell (der Aufrufer macht `soft(...)` daraus), keine Panics.
fn pruefe(spec: &SwarmSpec, limits: &SwarmLimits) -> Result<Geprueft, String> {
    let auftrag = spec.auftrag.trim().to_string();
    if auftrag.is_empty() {
        return Err("'auftrag' fehlt — beschreibe die Mission des Schwarms.".to_string());
    }
    let ids: Vec<String> = spec
        .agenten
        .iter()
        .map(|a| a.id.trim().to_string())
        .collect();
    if ids.is_empty() {
        return Err(
            "'agenten' ist leer — ein Schwarm braucht mindestens ein Mitglied.".to_string(),
        );
    }
    if ids.len() > limits.max_agents {
        return Err(format!(
            "{} Mitglieder überschreiten das Limit von {} — fasse den Schwarm enger.",
            ids.len(),
            limits.max_agents
        ));
    }
    let mut gesehen = HashSet::new();
    for id in &ids {
        if id.is_empty() {
            return Err(
                "leere Agent-ID — jedes Mitglied braucht eine kurze eindeutige 'id'.".to_string(),
            );
        }
        if id == crate::RUNTIME_SENDER {
            return Err(format!("'{id}' ist als Agent-ID reserviert."));
        }
        if !gesehen.insert(id.clone()) {
            return Err(format!("doppelte Agent-ID '{id}'."));
        }
    }
    let start_agent = match spec.start_agent.as_deref().map(str::trim) {
        Some(s) if !ids.iter().any(|id| id == s) => {
            return Err(format!("unbekannter 'start_agent' '{s}'."))
        }
        Some(s) => s.to_string(),
        None => ids[0].clone(),
    };

    let edges = resolve_edges(spec, &ids)?;

    Ok(Geprueft {
        auftrag,
        quorum: quorum(spec.erforderliche_zustimmungen, &ids, &edges),
        // Default aus der Mitgliederzahl, gedeckelt auf die harte Obergrenze.
        max_messages: spec
            .max_nachrichten
            .unwrap_or(ids.len() * MESSAGES_PER_AGENT)
            .clamp(1, limits.max_messages),
        edges,
        ids,
        start_agent,
        // Ohne Angabe KEINE Laufzeitgrenze (siehe `SwarmLimits::max_runtime_s`).
        max_runtime_s: match (spec.max_laufzeit_s, limits.max_runtime_s) {
            (Some(s), Some(deckel)) => Some(s.clamp(1, deckel)),
            (Some(s), None) => Some(s.max(1)),
            (None, deckel) => deckel,
        },
    })
}

/// Wie viele Zustimmungen ein Abschluss-Vorschlag braucht.
///
/// Die OBERGRENZE ist NICHT `n-1`: `swarm_propose` stellt den Vorschlag nur den
/// direkten Nachbarn des Vorschlagenden zu, abstimmen kann also nur, wer ihn auch
/// sieht. In einer Kette a–b–c erreicht ein Vorschlag von `a` nur `b` — ein Quorum
/// von 2 wäre unerfüllbar und der Schwarm liefe stumm bis zur Laufzeitgrenze
/// (Default 900 s). Maßgeblich ist deshalb der kleinste Knotengrad: so viele
/// Stimmen kann JEDES Mitglied einsammeln, ganz gleich wer vorschlägt.
///
/// Der DEFAULT ist die Mehrheit davon, nicht das Maximum. Im Mesh war der
/// Knotengrad `n-1`, das Default-Quorum also Einstimmigkeit: ein einziges
/// Mitglied, das sich enthält oder gerade in einem langen Turn steckt, machte den
/// Konsens unmöglich — der Schwarm lief zwangsläufig in ein Limit statt in ein
/// Ergebnis. Eine Mehrheit der Stimmberechtigten ist der Konsens, den die Policy
/// meint; wer Einstimmigkeit will, fordert sie über `erforderliche_zustimmungen`
/// ausdrücklich an.
///
/// Ein Solo-Schwarm landet bei 0 und schließt sofort beim Vorschlag ab.
fn quorum(gewuenscht: Option<usize>, ids: &[String], edges: &[(String, String)]) -> usize {
    let erreichbar = ids
        .iter()
        .map(|id| edges.iter().filter(|(a, b)| a == id || b == id).count())
        .min()
        .unwrap_or(0);
    gewuenscht.unwrap_or(erreichbar.div_ceil(2)).min(erreichbar)
}

/// Baut den Schwarm aus der geprüften Spezifikation und startet ihn.
fn starte(
    spec: &SwarmSpec,
    geprueft: &Geprueft,
    cfg: &SwarmToolConfig,
    name: &str,
) -> Result<crate::SwarmHandle, String> {
    let mut builder = SwarmBuilder::new(name)
        .completion(CompletionPolicy::Consensus {
            required_approvals: geprueft.quorum,
        })
        .mailbox_capacity(cfg.limits.mailbox_capacity)
        .max_messages(geprueft.max_messages)
        .max_hops(DEFAULT_MAX_HOPS)
        .max_idle(Duration::from_secs(cfg.limits.max_idle_s))
        .vote_window(Duration::from_secs(cfg.limits.vote_window_s))
        // Ohne laufenden Bus (z. B. `Agent::run`) ein frischer, ungehörter Bus.
        .agent_bus(cfg.run.bus().unwrap_or_default());

    if let Some(sekunden) = geprueft.max_runtime_s {
        builder = builder.max_runtime(Duration::from_secs(sekunden));
    }

    for (spec_agent, id) in spec.agenten.iter().zip(&geprueft.ids) {
        let peers = peers_of(&geprueft.edges, id);
        builder = builder.agent(
            id,
            build_member(spec_agent, id, &peers, cfg, spec.ergebnis_schema.as_ref()),
        );
    }
    for (a, b) in &geprueft.edges {
        builder = builder.connect_bidirectional(a, b);
    }

    builder
        .build()
        .map_err(|e| format!("Schwarm-Aufbau fehlgeschlagen: {e}"))?
        .start()
        .map_err(|e| format!("Schwarm-Start fehlgeschlagen: {e}"))
}

/// Der Tool-Rumpf: prüfen, starten, Verkehr spiegeln, auf das Ende warten, berichten.
fn run_swarm(cfg: &SwarmToolConfig, seq: &AtomicI64, args: Value) -> Result<String, String> {
    let spec: SwarmSpec = match serde_json::from_value(args) {
        Ok(spec) => spec,
        Err(e) => return soft(format!("Spezifikation nicht lesbar: {e}")),
    };
    let geprueft = match pruefe(&spec, &cfg.limits) {
        Ok(geprueft) => geprueft,
        Err(e) => return soft(e),
    };

    let nummer = seq.fetch_add(1, Ordering::SeqCst);
    let handle = match starte(&spec, &geprueft, cfg, &format!("swarm-{nummer}")) {
        Ok(handle) => handle,
        Err(e) => return soft(e),
    };

    // Vor `send_initial` abonnieren — frühere Events sieht ein neuer Subscriber nicht.
    let done = Arc::new(AtomicBool::new(false));
    let forwarder = cfg.run.bus().map(|bus| {
        let rx = handle.events();
        let done = done.clone();
        std::thread::Builder::new()
            .name(format!("swarm-{nummer}-events"))
            .spawn(move || forward_swarm_events(rx, bus, nummer, done))
    });

    if let Err(e) = handle.send_initial(&geprueft.start_agent, &geprueft.auftrag) {
        done.store(true, Ordering::SeqCst);
        let _ = handle.stop();
        return soft(format!("Initialaufgabe nicht zustellbar: {e}"));
    }

    let result = match cfg.run.cancel() {
        Some(cancel) => handle.join_with_cancel(&cancel),
        None => handle.join(),
    };
    done.store(true, Ordering::SeqCst);
    if let Some(Ok(forwarder)) = forwarder {
        let _ = forwarder.join();
    }
    Ok(render_result(&result))
}

/// Die Nachbarn eines Agenten aus der aufgelösten (ungerichteten) Kantenliste.
fn peers_of(edges: &[(String, String)], id: &str) -> Vec<String> {
    let mut peers: Vec<String> = edges
        .iter()
        .filter_map(|(a, b)| match (a.as_str() == id, b.as_str() == id) {
            (true, false) => Some(b.clone()),
            (false, true) => Some(a.clone()),
            _ => None,
        })
        .collect();
    peers.sort();
    peers.dedup();
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh(n: usize) -> SwarmSpec {
        let agenten: Vec<Value> = (0..n).map(|i| json!({"id": format!("a{i}")})).collect();
        serde_json::from_value(json!({"auftrag": "x", "agenten": agenten})).unwrap()
    }

    /// Ein Broadcast kostet eine Zustellung PRO Nachbar — ein fester Default
    /// hätte im Mesh mit jedem zusätzlichen Mitglied weniger Turns je Mitglied
    /// erlaubt (siehe [`MESSAGES_PER_AGENT`]).
    #[test]
    fn default_budget_skaliert_mit_der_mitgliederzahl() {
        let limits = SwarmLimits::default();
        for n in [2usize, 4, 6] {
            let geprueft = pruefe(&mesh(n), &limits).unwrap();
            assert_eq!(geprueft.max_messages, n * MESSAGES_PER_AGENT, "n={n}");
            assert!(geprueft.max_messages <= limits.max_messages, "n={n}");
        }
    }

    /// Explizites `max_nachrichten` gewinnt, bleibt aber unter der harten Grenze.
    #[test]
    fn explizites_budget_wird_auf_die_harte_grenze_gedeckelt() {
        let limits = SwarmLimits::default();
        let mut spec = mesh(2);
        spec.max_nachrichten = Some(99_999);
        assert_eq!(
            pruefe(&spec, &limits).unwrap().max_messages,
            limits.max_messages
        );
    }

    /// Im Mesh ist der Knotengrad `n-1`; das Default-Quorum war damit
    /// Einstimmigkeit und ein einziges enthaltenes Mitglied verhinderte jeden
    /// Konsens. Default ist jetzt die Mehrheit der Stimmberechtigten.
    #[test]
    fn default_quorum_im_mesh_ist_die_mehrheit() {
        let limits = SwarmLimits::default();
        assert_eq!(pruefe(&mesh(4), &limits).unwrap().quorum, 2);
        assert_eq!(pruefe(&mesh(3), &limits).unwrap().quorum, 1);
        // Solo-Schwarm: niemand kann abstimmen, der Vorschlag schließt sofort ab.
        assert_eq!(pruefe(&mesh(1), &limits).unwrap().quorum, 0);
    }

    /// Einstimmigkeit bleibt anforderbar — gedeckelt auf die möglichen Stimmen.
    #[test]
    fn explizites_quorum_schlaegt_den_default() {
        let limits = SwarmLimits::default();
        let mut spec = mesh(4);
        spec.erforderliche_zustimmungen = Some(3);
        assert_eq!(pruefe(&spec, &limits).unwrap().quorum, 3);
        spec.erforderliche_zustimmungen = Some(99);
        assert_eq!(pruefe(&spec, &limits).unwrap().quorum, 3);
    }
}
