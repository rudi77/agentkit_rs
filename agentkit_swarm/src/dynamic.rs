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
use agentkit::coding::{CodingTools, CODING_TOKEN_BUDGET, READ_ONLY_TOOLS};
use agentkit::events::{AgentEvent, EventBus, EventData, TOOL_RESULT};
use agentkit::llm::Llm;
use agentkit::{
    is_likely_destructive, parse_tools_field, strategy_from_str, truncate, Agent, AgentRole,
    McpHub, RunHandle, Skills, Strategy, ToolRegistry, SKILL_SYSTEM,
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
    pub max_messages: usize,
    pub max_runtime_s: u64,
    /// Loop-Schritte je Mitglied und Nachricht.
    pub max_steps: usize,
    pub mailbox_capacity: usize,
}

impl Default for SwarmLimits {
    fn default() -> Self {
        SwarmLimits {
            max_agents: 6,
            max_messages: 60,
            max_runtime_s: 900,
            max_steps: 40,
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
}

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
swarm_reply. Schlage den Abschluss erst vor, wenn das Ergebnis inhaltlich steht, und lege \
das vollständige Ergebnis in den Vorschlag (nur dieser Text wird zurückgegeben). Stimmst du \
einem Vorschlag zu, nutze swarm_vote mit der vorschlag_id aus der Nachricht.";

/// Generischer Mitglieds-Prompt, wenn weder `system` noch `rolle` gesetzt sind.
const GENERIC_MEMBER: &str =
    "Erledige deinen Teil des Schwarm-Auftrags gründlich und knapp und arbeite \
konstruktiv mit den anderen Mitgliedern zusammen.";

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
            for endpoint in [a, b] {
                if !ids.contains(endpoint) {
                    return Err(format!(
                        "Verbindung verweist auf unbekannte Agent-ID '{endpoint}'"
                    ));
                }
            }
            if a == b {
                return Err(format!("Verbindung von '{a}' auf sich selbst"));
            }
            out.push((a.clone(), b.clone()));
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
fn member_system(spec: &AgentSpec, role: Option<&AgentRole>, peers: &[String]) -> String {
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
    format!(
        "{MEMBER_PROTOCOL}\n\nDeine Agent-ID ist '{}'. Du kannst direkt reden mit: {nachbarn}.\n\n{own}",
        spec.id
    )
}

/// Baut EIN Mitglied: Tool-Teilmenge + MCP + optionale Skills, System-Prompt aus
/// Protokoll und Rolle. Die Registry entsteht von Grund auf aus [`CodingTools`] —
/// dadurch enthält sie strukturell weder `swarm` noch `task` (keine Rekursion).
fn build_member(spec: &AgentSpec, peers: &[String], cfg: &SwarmToolConfig) -> Agent {
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

    let mut system = member_system(spec, role, peers);
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

    Agent::builder(cfg.llm.clone())
        .tools(reg)
        .system(&system)
        .strategy(strategy)
        .max_steps(cfg.limits.max_steps)
        // Wie jeder Coding-Agent: der Builder-Default (8000) würde nach zwei
        // großen Tool-Ergebnissen naiv (und verlustbehaftet) kompaktieren.
        .token_budget(CODING_TOKEN_BUDGET)
        .build()
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
        CompletionReason::ActorFailure { .. } => "actor_fehler",
        CompletionReason::Stopped => "abgebrochen",
    }
}

/// Das Tool-Ergebnis für den Orchestrator: deutsches JSON, weiche Fehler als
/// Werte. `SwarmResult` selbst bleibt serde-frei — ein `Serialize` dort hätte
/// heute genau einen Nutzer und würde die Laufzeit-Typen an ein Format binden.
fn render_result(result: &SwarmResult) -> String {
    let mut out = json!({
        "status": reason_label(&result.reason),
        "nachrichten": result.messages_sent,
        "unzustellbar": result.dead_letters.len(),
        "turns": result.turns.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
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
                "Das Nachrichtenlimit war erschöpft, bevor ein Vorschlag angenommen wurde. \
                 Fasse zusammen, was du hast, oder starte einen kleineren Schwarm."
            );
        }
        CompletionReason::MaxRuntimeReached => {
            out["hinweis"] = json!(
                "Die Laufzeitgrenze wurde erreicht, ohne dass ein Vorschlag angenommen wurde. \
                 Enger fassen oder weniger Mitglieder."
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
                "description": "Wie viele Nachbarn einem Abschluss-Vorschlag zustimmen müssen. Default und Obergrenze sind die Nachbarn des am schwächsten verbundenen Mitglieds — nur wer einen Vorschlag sieht, kann über ihn abstimmen. Bei 'mesh' sind das alle anderen, bei 'kette'/'stern' entsprechend weniger."
            },
            "max_nachrichten": {"type": "integer", "description": "Obergrenze der Zustellungen im Schwarm."},
            "max_laufzeit_s": {"type": "integer", "description": "Obergrenze der Laufzeit in Sekunden."}
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
    max_runtime_s: u64,
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
    let start_agent = match &spec.start_agent {
        Some(s) if !ids.contains(s) => return Err(format!("unbekannter 'start_agent' '{s}'.")),
        Some(s) => s.clone(),
        None => ids[0].clone(),
    };

    let edges = resolve_edges(spec, &ids)?;

    Ok(Geprueft {
        auftrag,
        quorum: quorum(spec.erforderliche_zustimmungen, &ids, &edges),
        edges,
        ids,
        start_agent,
        max_messages: spec
            .max_nachrichten
            .unwrap_or(limits.max_messages)
            .clamp(1, limits.max_messages),
        max_runtime_s: spec
            .max_laufzeit_s
            .unwrap_or(limits.max_runtime_s)
            .clamp(1, limits.max_runtime_s),
    })
}

/// Wie viele Zustimmungen ein Abschluss-Vorschlag braucht.
///
/// Die Obergrenze ist NICHT `n-1`: `swarm_propose` stellt den Vorschlag nur den
/// direkten Nachbarn des Vorschlagenden zu, abstimmen kann also nur, wer ihn auch
/// sieht. In einer Kette a–b–c erreicht ein Vorschlag von `a` nur `b` — ein Quorum
/// von 2 wäre unerfüllbar und der Schwarm liefe stumm bis zur Laufzeitgrenze
/// (Default 900 s). Maßgeblich ist deshalb der kleinste Knotengrad: so viele
/// Stimmen kann JEDES Mitglied einsammeln, ganz gleich wer vorschlägt.
///
/// Ein Solo-Schwarm landet bei 0 und schließt sofort beim Vorschlag ab.
fn quorum(gewuenscht: Option<usize>, ids: &[String], edges: &[(String, String)]) -> usize {
    let erreichbar = ids
        .iter()
        .map(|id| peers_of(edges, id).len())
        .min()
        .unwrap_or(0);
    gewuenscht.unwrap_or(erreichbar).min(erreichbar)
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
        .max_runtime(Duration::from_secs(geprueft.max_runtime_s))
        // Ohne laufenden Bus (z. B. `Agent::run`) ein frischer, ungehörter Bus.
        .agent_bus(cfg.run.bus().unwrap_or_default());

    for (spec_agent, id) in spec.agenten.iter().zip(&geprueft.ids) {
        let peers = peers_of(&geprueft.edges, id);
        builder = builder.agent(id, build_member(spec_agent, &peers, cfg));
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
