//! Gemeinsame Anwendungs-Bausteine für die Frontends (`bin/agentkit`, `bin/tui`).
//!
//! Beide Frontends bauen denselben vollen Coding-Agenten und wollen dieselben
//! Hilfen — `.env` laden, den Plan rendern, PLAN-Events auf den aktiven Bus schicken.
//! Damit das nicht doppelt gepflegt werden muss, liegt es hier. Der einzige echte
//! Unterschied zwischen den Frontends ist der **Freigabe-Callback** (`approve`): das
//! CLI fragt über stdin, das TUI über einen Dialog — er wird daher hereingereicht.

use std::sync::Arc;

use serde_json::Value;

use crate::coding::{ApproveFn, CodingTools};
use crate::events::{AgentEvent, EventData};
use crate::llm::Llm;
use crate::memory::{content, count_tokens_text, one_line, role, truncate, ShortTermMemory};
use crate::planning::Step;
use crate::roles::{add_task_tool, builtin_roles, load_roles_from_dir, merge_roles, AgentRole};
use crate::{
    Agent, LongTermMemory, McpHub, Plan, RunHandle, Skills, Strategy, ToolRegistry, CODING_SYSTEM,
    PLAN, SKILL_SYSTEM, SUBAGENT_SYSTEM,
};

/// Plattform-Hinweis für `run_shell`, an den Coding-System-Prompt angehängt — so
/// fummelt das Modell nicht erst mit der falschen Shell-Syntax (Bash-Heredocs auf
/// Windows etc.) herum.
#[cfg(windows)]
const SHELL_HINT: &str = "\n\nrun_shell nutzt PowerShell (Windows): verwende \
PowerShell-Syntax, KEINE Bash-Heredocs (`<<'EOF'`). Mehrzeilige Skripte am besten \
mit write_file in eine Datei schreiben und dann ausführen (z. B. `python script.py`).";

#[cfg(not(windows))]
const SHELL_HINT: &str = "\n\nrun_shell nutzt bash.";

/// Lädt eine `.env` aus dem aktuellen Verzeichnis — nur Variablen, die noch nicht
/// gesetzt sind (wie die Python-CLI, ohne zusätzliche Abhängigkeit).
pub fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if std::env::var(k).is_err() {
                std::env::set_var(k, v);
            }
        }
    }
}

/// Plan-Schritte rendern: `[ ]/[~]/[x] N. Beschreibung`, mit `sep` verbunden
/// (CLI nutzt `"\n"`, das TUI `"  "` für eine Zeile).
pub fn render_steps(steps: &[Step], sep: &str) -> String {
    if steps.is_empty() {
        return "(noch kein Plan)".to_string();
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mark = match s.status.as_str() {
                "in_progress" => "[~]",
                "done" => "[x]",
                _ => "[ ]",
            };
            format!("{mark} {}. {}", i + 1, s.step)
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// Ein [`Plan`], der nach jeder Aktualisierung ein PLAN-Event auf den jeweils
/// aktiven Bus des [`RunHandle`] schickt — die kanonische Verdrahtung beider Frontends.
/// Das Event trägt die strukturierte Schrittliste; das jeweilige Frontend rendert sie
/// (CLI mehrzeilig via `"\n"`, TUI einzeilig via `"  "`).
pub fn plan_with_bus_updates(run: &RunHandle) -> Plan {
    let run = run.clone();
    Plan::with_on_update(move |steps| {
        if let Some(bus) = run.bus() {
            bus.publish(AgentEvent::new(PLAN, EventData::Plan(steps.to_vec())));
        }
    })
}

/// Was ein Frontend-Tool beim Bau des Coding-Agenten vorfindet — dieselben
/// Bausteine, die [`add_task_tool`] als Parameterliste bekommt, nur gebündelt.
///
/// Ein Frontend kann seine Tools NICHT fertig gebaut übergeben: `run`, `approve`
/// und (im TUI) auch `llm` entstehen erst hier, und eine [`ToolRegistry`] lässt
/// sich nicht mit einer zweiten verschmelzen. Deshalb der Callback.
pub struct ExtraToolCtx<'a> {
    /// Lauf-Kontext des Haupt-Agenten: liefert zur Laufzeit den aktiven
    /// [`crate::EventBus`] und den Stop-Knopf. Tools, die ihn brauchen, müssen
    /// VOR dem Build registriert werden — genau das ist hier der Fall.
    pub run: &'a RunHandle,
    pub llm: &'a Arc<dyn Llm>,
    /// Die Sandbox-Tools des Haupt-Agenten (Workspace, Freigabe-Callback und
    /// Shell-Timeout stecken darin). Dieselbe Instanz weitergeben, statt eine
    /// zweite zu bauen — sonst driften Workspace und Regeln auseinander.
    pub coding: &'a CodingTools,
    /// Geteilter MCP-Hub; `register_enabled` liefert die GERADE aktiven Server-Tools.
    pub mcp: &'a Arc<McpHub>,
    /// Skills-Verzeichnis, falls das Frontend eines gesetzt hat (`--skills`).
    pub skills: Option<&'a Skills>,
    /// Die aktiven Sub-Agent-Rollen (eingebaute + `--agents DIR`).
    pub roles: &'a [AgentRole],
    /// `--dry-run`: zerstörerische Tools müssen auch in den Registries gebaut
    /// werden, die das Frontend-Tool selbst erzeugt (siehe [`add_task_tool`]).
    pub dry_run: bool,
}

/// Erweiterungspunkt für Frontends, die eigene Fähigkeiten mitbringen — heute das
/// `swarm`-Tool aus `agentkit-swarm`, das der Agent-Kern bewusst nicht kennen darf
/// (die Abhängigkeit läuft nur in eine Richtung).
///
/// `Arc` statt Referenz, damit [`crate::tui::TuiConfig`] ohne Lifetime auskommt.
pub type ExtraTools = Arc<dyn Fn(&mut ToolRegistry, &ExtraToolCtx) + Send + Sync>;

/// Konfiguration des vollen Coding-Agenten (gemeinsam von CLI und TUI befüllt).
pub struct CodingAgentConfig<'a> {
    pub workspace: &'a str,
    pub strategy: Strategy,
    pub max_steps: usize,
    pub skills: Option<&'a str>,
    pub agents: Option<&'a str>,
    pub memory: Option<&'a str>,
    pub subagents: bool,
    /// Zusätzlicher, agenten-spezifischer System-Prompt (z. B. je Pipe-Stage aus
    /// `--system`/`--system-file`/`--profile`). Wird an den Coding-System-Prompt
    /// angehängt — steuert Persona/Format, ohne die Tool-Instruktionen zu verlieren.
    pub system: Option<&'a str>,
    /// Selbstverifikation vor der finalen Antwort (CLI `--verify`, siehe
    /// [`crate::agent::VERIFY_NUDGE`]).
    pub verify: bool,
    /// Timeout (Sekunden) für `run_shell` (CLI `--shell-timeout`, Default 120).
    /// Build-/Install-lastige Aufgaben (apt-get, Compiles) brauchen deutlich mehr.
    pub shell_timeout: u64,
    /// `--dry-run`: zerstörerische Tools werden zu No-Ops — für den Haupt-Agenten
    /// UND für jede abgeleitete Registry (Sub-Agenten, Schwarm-Mitglieder).
    pub dry_run: bool,
    /// Zusätzliche Tools des Frontends (siehe [`ExtraTools`]). `None` = keine.
    pub extra_tools: Option<ExtraTools>,
}

/// Dateiname der Projekt-Instruktionen im Workspace.
///
/// Bewusst NUR dieser eine, tool-eigene Name — kein Rückfall auf `CLAUDE.md`
/// o. Ä.: agentkit läuft auch in fremden Repos (die Benchmark-Pipeline lädt es
/// in jeden Task-Container), und eine dort zufällig vorhandene Datei würde
/// still den System-Prompt verändern und die Läufe unvergleichbar machen. Wer
/// eine andere Datei will, zeigt mit `--system-file` darauf.
pub const PROJECT_INSTRUCTIONS: &str = "AGENTKIT.md";

/// Liest die Projekt-Instruktionen aus dem Workspace, falls vorhanden.
/// Leere Dateien zählen als nicht vorhanden.
pub fn load_project_instructions(workspace: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(std::path::Path::new(workspace).join(PROJECT_INSTRUCTIONS)).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Baut den vollen Coding-Agenten: Sandbox-Tools (inkl. glob/grep), optional Skills
/// und Langzeitgedächtnis, Plan (mit PLAN-Events), Rollen + `task`-Tool sowie die
/// optionalen [`ExtraTools`] des Frontends.
///
/// `approve` ist der frontend-spezifische Freigabe-Callback (stdin bzw. TUI-Dialog);
/// die Coding-Tools fragen ihn IMMER (`approval = true`) — die Policy (nachfragen,
/// auto, `--yes`) steckt im Callback selbst. Gibt neben dem Agenten Plan, Skills und
/// die aktiven Rollen zurück (für Slash-Befehle wie `/plan`, `/skills`, `/agents`).
///
/// `mcp` ist der (ggf. leere) [`McpHub`]: dessen AKTIVE Server-Tools werden in den
/// Haupt-Agenten eingeklinkt; dieselbe (geteilte) Referenz geht ans `task`-Tool, damit
/// Sub-Agenten beim Spawnen die gerade aktiven MCP-Tools erhalten. Zusätzlich gibt die
/// Funktion die **MCP-freie Basis-Registry** des Haupt-Agenten zurück — Frontends, die
/// MCP zur Laufzeit umschalten (REPL/TUI), bauen `agent.tools` daraus neu auf
/// (`base.clone()` + `mcp.register_enabled`).
pub fn build_coding_agent(
    llm: Arc<dyn Llm>,
    cfg: &CodingAgentConfig,
    approve: ApproveFn,
    mcp: Arc<McpHub>,
) -> (
    Agent,
    Plan,
    Option<Skills>,
    Vec<AgentRole>,
    ToolRegistry,
    CodingTools,
) {
    let run = RunHandle::new();

    // Coding-Tools EINMAL bauen (legt den Workspace an) und an alles weiterreichen,
    // was eigene Registries erzeugt (`task`, `swarm`) — sonst bekäme jeder Pfad
    // seine eigene Sandbox-Instanz.
    // Mit dem Lauf-Kontext verdrahtet: run_shell/git beenden ihren Kindprozess
    // sofort, wenn der TOP-LEVEL-Stop-Knopf des laufenden Auftrags gedrückt wird
    // (Sub-Agenten bekommen genau diesen Cancel durchgereicht). Der schwarm-
    // INTERNE Member-Abbruch (Konsens/Limit) setzt nur die Actor-Cancels — ein
    // dort laufender Shell-Befehl wartet weiter auf seinen Timeout.
    let coding = CodingTools::with_approve(cfg.workspace, true, approve.clone(), cfg.shell_timeout)
        .with_run_handle(run.clone());
    let mut tools = ToolRegistry::new();
    coding.register(&mut tools, None);

    // Human-in-the-Loop braucht KEIN Spezial-Werkzeug: In REPL/TUI beendet der Agent einfach
    // seinen Zug mit einer Rückfrage; die Antwort des Menschen kommt als nächste Nachricht, und
    // der Agent macht mit vollem Gesprächsverlauf weiter (die Kurzzeit-Memory bleibt über die
    // Züge erhalten). So bleibt die Schleife die eine Schleife — ohne blockierende Sonderpfade.

    let skills = cfg.skills.map(Skills::new);
    let long_term = cfg.memory.map(LongTermMemory::new);

    let mut system = String::from(CODING_SYSTEM);
    system.push_str(SHELL_HINT);
    if skills.is_some() {
        system.push_str("\n\n");
        system.push_str(SKILL_SYSTEM);
    }
    if cfg.subagents {
        system.push_str("\n\n");
        system.push_str(SUBAGENT_SYSTEM);
    }
    // Projekt-Instruktionen aus dem Workspace: gelten für jede Sitzung in
    // diesem Projekt, ohne Flag. Vor dem `--system`-Zusatz, damit ein
    // ausdrücklich übergebener Prompt weiterhin das letzte Wort hat.
    if let Some(projekt) = load_project_instructions(cfg.workspace) {
        system.push_str("\n\n## Projekt-Instruktionen (");
        system.push_str(PROJECT_INSTRUCTIONS);
        system.push_str(")\n\n");
        system.push_str(&projekt);
    }
    // Agenten-spezifischer Zusatz (Pipe-Stage-Persona/Format) ganz am Ende, damit er
    // die generischen Coding-Instruktionen bewusst überschreiben/verfeinern kann.
    if let Some(extra) = cfg.system.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\n## Agenten-spezifische Instruktionen\n\n");
        system.push_str(extra);
    }

    let mut roles = builtin_roles();
    if let Some(dir) = cfg.agents {
        roles = merge_roles(roles, load_roles_from_dir(dir));
    }

    let plan = plan_with_bus_updates(&run);

    // `task`-Tool VOR dem Build registrieren (die Registry wird beim Build kopiert);
    // es teilt sich über `run` den Lauf-Kontext mit dem fertigen Agenten.
    if cfg.subagents {
        add_task_tool(
            &mut tools,
            run.clone(),
            llm.clone(),
            coding.clone(),
            roles.clone(),
            mcp.clone(),
            cfg.dry_run,
        );
    }

    // Frontend-eigene Tools an derselben Stelle: vor dem Build (wegen `run`) und
    // vor `mcp.apply` — dadurch sind sie automatisch Teil der MCP-freien
    // Basis-Registry und überleben ein Neuverdrahten (REPL `/mcp`, TUI F2).
    if let Some(extra) = &cfg.extra_tools {
        extra(
            &mut tools,
            &ExtraToolCtx {
                run: &run,
                llm: &llm,
                coding: &coding,
                mcp: &mcp,
                skills: skills.as_ref(),
                roles: &roles,
                dry_run: cfg.dry_run,
            },
        );
    }

    let mut builder = Agent::builder(llm)
        .tools(tools)
        .system(&system)
        .strategy(cfg.strategy)
        .plan(plan.clone())
        .max_steps(cfg.max_steps)
        .token_budget(crate::CODING_TOKEN_BUDGET)
        .verify_before_final(cfg.verify)
        .run_handle(run);
    if let Some(s) = skills.clone() {
        builder = builder.skills(s);
    }
    if let Some(lt) = long_term {
        builder = builder.long_term(lt);
    }
    let mut agent = builder.build();

    // Aktive MCP-Tools einklinken; `mcp_base` (Coding + Plan + Skills + task, OHNE MCP)
    // ist die Grundlage, aus der ein Frontend beim Umschalten neu verdrahtet.
    let mut mcp_base = mcp.apply(&mut agent);

    // `--dry-run` zuletzt und HIER, nicht im Frontend: sonst gilt es nur auf dem
    // Weg, der es selbst anwendet (das CLI tat das nur im One-shot, im REPL also
    // gar nicht), und `McpHub::rewire` baut `agent.tools` aus `mcp_base` neu auf
    // und würfe die Sperre wieder weg. Beide Registries brauchen sie deshalb.
    if cfg.dry_run {
        agent.tools = agent.tools.dry_run_blocking(crate::is_likely_destructive);
        mcp_base = mcp_base.dry_run_blocking(crate::is_likely_destructive);
    }

    (agent, plan, skills, roles, mcp_base, coding)
}

/// Kurzinfo über den angeklinkten [`crate::ManagedContext`] — Grundlage der
/// Startmeldung beider Frontends (stderr bzw. Verlaufs-Notiz).
#[cfg(feature = "ctxman")]
pub struct AttachedContextInfo {
    /// true ⇔ aus Snapshot fortgesetzt (eingefrorene Policy gilt; `--ctx-policy`/
    /// `--ctx-budget` wirken erst auf eine neue Session).
    pub resumed: bool,
    /// Effektiver Tokenizer-Name ("heuristic", "o200k", "cl100k").
    pub tokenizer: String,
}

/// Klinkt ctxman als Context-Manager in einen fertig gebauten Agenten ein
/// (CLI `--ctx DIR`, TUI `--ctx DIR`; Feature `ctxman`): lädt bzw. startet den
/// Snapshot, übernimmt den System-Prompt des Agenten als Static-Region und
/// registriert das `expand_context_ref`-Tool — auch in der MCP-freien
/// Basis-Registry, damit es ein MCP-Neuverdrahten (REPL `/mcp on|off`, TUI F2)
/// überlebt. Gemeinsamer Pfad beider Frontends; die Meldung macht der Aufrufer
/// aus der zurückgegebenen [`AttachedContextInfo`].
#[cfg(feature = "ctxman")]
pub fn attach_managed_context(
    agent: &mut Agent,
    mcp_base: &mut ToolRegistry,
    cfg: crate::ManagedContextConfig,
    llm: Arc<dyn Llm>,
) -> Result<AttachedContextInfo, String> {
    let ctx = crate::ManagedContext::new(cfg, llm)?;
    let info = AttachedContextInfo {
        resumed: ctx.resumed(),
        tokenizer: ctx.tokenizer().to_string(),
    };
    let system = agent
        .memory
        .messages
        .iter()
        .find(|m| m["role"] == "system")
        .and_then(|m| m["content"].as_str())
        .map(str::to_string);
    if let Some(sys) = system {
        let _ = ctx.set_system(&sys);
    }
    ctx.register_tool(&mut agent.tools);
    ctx.register_tool(mcp_base);
    agent.context = Some(ctx);
    Ok(info)
}

/// Baut das separate Compaction-LLM (`--ctx-compaction-model NAME`) aus derselben
/// Umgebung wie das Agent-LLM: bei Azure ist NAME der Deployment-Name, sonst der
/// OpenAI-Modellname (auch für lokale OpenAI-kompatible Server via
/// `OPENAI_BASE_URL`). Fehler, wenn keine Provider-Umgebung vorhanden ist —
/// die Aufrufer fallen dann sichtbar auf das Agent-LLM zurück.
#[cfg(all(feature = "ctxman", feature = "openai"))]
pub fn compaction_llm_from_env(name: &str) -> Result<Arc<dyn Llm>, String> {
    // Platzhalter (`<…>`) zählen wie in `config.rs` als unbesetzt.
    let get = |k: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.trim().is_empty() && !v.contains('<'))
    };
    if let (Some(endpoint), Some(key)) = (get("AZURE_OPENAI_ENDPOINT"), get("AZURE_OPENAI_API_KEY"))
    {
        let version = get("AZURE_OPENAI_API_VERSION").unwrap_or_else(|| "2024-10-21".to_string());
        return Ok(Arc::new(crate::OpenAiLlm::azure(
            &endpoint, &key, name, &version,
        )));
    }
    if let Some(base) = get("OPENAI_BASE_URL") {
        let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        return Ok(Arc::new(crate::OpenAiLlm::compatible(&base, &key, name)));
    }
    if let Some(key) = get("OPENAI_API_KEY") {
        return Ok(Arc::new(crate::OpenAiLlm::openai(&key, name)));
    }
    Err("keine LLM-Provider-Umgebung (AZURE_OPENAI_*/OPENAI_*) gefunden".to_string())
}

/// Ohne Feature `openai` gibt es kein zweites HTTP-LLM — sichtbarer Fehler statt
/// stillem Ignorieren des Flags.
#[cfg(all(feature = "ctxman", not(feature = "openai")))]
pub fn compaction_llm_from_env(_name: &str) -> Result<Arc<dyn Llm>, String> {
    Err("Binary ohne Feature `openai` gebaut — kein separates Compaction-LLM möglich".to_string())
}

// -------------------------------------------------------------- Kontext-Report

/// Ein Abschnitt der Kontext-Belegung (System-Prompt, Tool-Schemas, Nachrichten …)
/// für die `/context`-Anzeige der Frontends.
pub struct ContextSegment {
    /// Anzeige-Name des Abschnitts, z. B. "System-Prompt".
    pub label: String,
    /// Token-Schätzung des Abschnitts.
    pub tokens: usize,
    /// Anzahl der Einträge (Nachrichten, Tools, Segmente).
    pub count: usize,
    /// Zusatz-Hinweis, z. B. "1 ausgelagert" (nur mit ctxman).
    pub note: Option<String>,
}

/// Momentaufnahme der Kontext-Belegung eines Agenten: Abschnitte in
/// Anzeige-Reihenfolge plus Summe und Budget. Die Tokens sind Schätzwerte —
/// ohne ctxman die Zeichen/4-Heuristik ([`count_tokens_text`]), mit ctxman die
/// beim Append gezählten Segment-Tokens.
pub struct ContextReport {
    pub segments: Vec<ContextSegment>,
    /// Summe der Tokens aller Abschnitte.
    pub total: usize,
    /// Token-Budget: ohne ctxman [`Agent::token_budget`], sonst das Policy-Budget.
    pub budget: usize,
    /// true ⇔ ctxman verwaltet den Kontext (Feature `ctxman`, `--ctx`).
    pub managed: bool,
}

impl ShortTermMemory {
    /// Rendert den Verlauf als lesbares Markdown (Züge als Überschriften,
    /// Tool-Aufrufe mit Argumenten, Ergebnisse in Code-Blöcken) — die
    /// Datengrundlage für `/export` in jedem Frontend.
    ///
    /// `full = false` kürzt Tool-Ergebnisse und den System-Prompt auf wenige
    /// Zeilen: für die Ausgabe im Terminal, wo ein Coding-Verlauf (einzelne
    /// Ergebnisse bis [`crate::memory::TRUNCATE_LIMIT`] Zeichen) sonst unlesbar
    /// durchrauscht. `full = true` schreibt alles — für den Export in eine Datei.
    pub fn to_markdown(&self, full: bool) -> String {
        /// So viele Zeichen eines Tool-Ergebnisses zeigt die Kurzfassung.
        const PREVIEW: usize = 400;

        let shorten = |s: &str| {
            if full {
                s.to_string()
            } else {
                truncate(s, PREVIEW)
            }
        };

        let mut out = String::from("# agentkit — Gesprächsverlauf\n");
        let mut turn = 0;
        for m in &self.messages {
            match role(m) {
                "system" => {
                    out.push_str("\n## System-Prompt\n\n");
                    out.push_str(&fence(&shorten(content(m))));
                }
                "user" => {
                    turn += 1;
                    out.push_str(&format!("\n## Zug {turn} · Du\n\n{}\n", content(m)));
                }
                "assistant" => {
                    if !content(m).is_empty() {
                        out.push_str(&format!("\n### Agent\n\n{}\n", content(m)));
                    }
                    for call in m
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let f = &call["function"];
                        let name = f["name"].as_str().unwrap_or("?");
                        let args = f["arguments"].as_str().unwrap_or("{}");
                        out.push_str(&format!("\n- **{name}**(`{}`)\n", one_line(args, 160)));
                    }
                }
                "tool" => {
                    out.push_str("\nErgebnis:\n\n");
                    out.push_str(&fence(&shorten(content(m))));
                }
                _ => {}
            }
        }
        out
    }
}

/// Text in einen Code-Block packen. Der Zaun ist immer länger als die längste
/// Backtick-Folge im Inhalt — sonst bräche ein Ergebnis, das selbst ```
/// enthält (z. B. eine gelesene Markdown-Datei), den Block mittendrin auf.
fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let bar = "`".repeat(longest.max(2) + 1);
    format!("{bar}\n{}\n{bar}\n", text.trim_end())
}

/// Token-Zahl mit Tausenderpunkten (deutsche Schreibweise).
pub fn fmt_tokens(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// Anteil in Prozent mit einer Nachkommastelle und Komma (deutsche Schreibweise).
pub fn fmt_pct(part: usize, whole: usize) -> String {
    let p = 100.0 * part as f64 / whole.max(1) as f64;
    format!("{p:.1} %").replace('.', ",")
}

/// "1 Eintrag" / "n Einträge" für die Legende.
pub fn fmt_count(n: usize) -> String {
    if n == 1 {
        "1 Eintrag".to_string()
    } else {
        format!("{n} Einträge")
    }
}

/// Baut den [`ContextReport`] aus dem aktuellen Zustand des Agenten. Mit aktivem
/// [`crate::ManagedContext`] liefern dessen Segmente die Zahlen (inklusive
/// ausgelagert/kompaktiert), sonst die `ShortTermMemory`-Spiegelung; die
/// Tool-Schemas kommen in beiden Fällen aus der Registry, denn sie gehen bei
/// jedem Modell-Call mit in den Kontext, stehen aber in keiner Message.
pub fn context_report(agent: &Agent) -> ContextReport {
    #[cfg(feature = "ctxman")]
    if let Some(ctx) = &agent.context {
        return ctxman_report(agent, ctx);
    }
    memory_report(agent)
}

/// Tool-Schemas als eigener Abschnitt (Tokens = serialisiertes JSON, Zeichen/4).
fn schema_segment(agent: &Agent) -> Option<ContextSegment> {
    let schemas = agent.tools.schemas()?;
    let json = serde_json::to_string(schemas).unwrap_or_default();
    Some(ContextSegment {
        label: "Tool-Schemas".to_string(),
        tokens: count_tokens_text(&json),
        count: schemas.len(),
        note: None,
    })
}

/// Summiert die Abschnitte zum fertigen Report.
fn finish_report(segments: Vec<ContextSegment>, budget: usize, managed: bool) -> ContextReport {
    let total = segments.iter().map(|s| s.tokens).sum();
    ContextReport {
        segments,
        total,
        budget,
        managed,
    }
}

/// Report aus der `ShortTermMemory` (Standard-Pfad ohne ctxman).
fn memory_report(agent: &Agent) -> ContextReport {
    // Feste Anzeige-Reihenfolge; leere Abschnitte fliegen am Ende raus.
    let labels = [
        "System-Prompt",
        "Verlaufs-Zusammenfassung",
        "User-Nachrichten",
        "Assistant-Antworten",
        "Tool-Aufrufe",
        "Tool-Ergebnisse",
        "Sonstiges",
    ];
    let mut sections: Vec<ContextSegment> = labels
        .iter()
        .map(|l| ContextSegment {
            label: l.to_string(),
            tokens: 0,
            count: 0,
            note: None,
        })
        .collect();
    let mut system_seen = false;
    for m in &agent.memory.messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        let tokens = count_tokens_text(content);
        let mut add = |idx: usize, tokens: usize, n: usize| {
            sections[idx].tokens += tokens;
            sections[idx].count += n;
        };
        match role {
            // Die erste System-Message ist der Prompt; jede weitere ist eine
            // Compaction-Notiz ("Bisheriger Verlauf (komprimiert)").
            "system" if !system_seen => {
                system_seen = true;
                add(0, tokens, 1);
            }
            "system" => add(1, tokens, 1),
            "user" => add(2, tokens, 1),
            "assistant" => {
                if !content.is_empty() {
                    add(3, tokens, 1);
                }
                if let Some(tc) = m.get("tool_calls") {
                    let n = tc.as_array().map_or(0, Vec::len);
                    add(4, count_tokens_text(&tc.to_string()), n);
                }
            }
            "tool" => add(5, tokens, 1),
            _ => add(6, tokens, 1),
        }
    }
    // Tool-Schemas direkt hinter dem System-Prompt einsortieren.
    let mut segments: Vec<ContextSegment> = Vec::new();
    let mut rest = sections.into_iter();
    segments.extend(rest.next().filter(|s| s.count > 0));
    segments.extend(schema_segment(agent));
    segments.extend(rest.filter(|s| s.count > 0));
    finish_report(segments, agent.token_budget, false)
}

/// Report aus den ctxman-Segmenten (Feature `ctxman`, `--ctx`).
#[cfg(feature = "ctxman")]
fn ctxman_report(agent: &Agent, ctx: &crate::ManagedContext) -> ContextReport {
    let (stats, budget) = ctx.segment_stats();
    let mut segments: Vec<ContextSegment> = stats
        .into_iter()
        .map(|st| {
            let label = match st.kind.as_str() {
                "system_prompt" => "System-Prompt".to_string(),
                "user_msg" => "User-Nachrichten".to_string(),
                "assistant_msg" => "Assistant-Antworten".to_string(),
                "tool_call" => "Tool-Aufrufe".to_string(),
                "tool_result" => "Tool-Ergebnisse".to_string(),
                "skill_content" => "Skill-Inhalte".to_string(),
                "task" => "Sub-Agent-Ergebnisse".to_string(),
                "mcp_resource" => "MCP-Ressourcen".to_string(),
                "ref_expansion" => "Ref-Expansionen".to_string(),
                "compaction_summary" => "Verlaufs-Zusammenfassung".to_string(),
                other => other.to_string(),
            };
            let mut notes = Vec::new();
            if st.externalized > 0 {
                notes.push(format!("{} ausgelagert", st.externalized));
            }
            if st.compacted > 0 {
                notes.push(format!("{} kompaktiert", st.compacted));
            }
            ContextSegment {
                label,
                tokens: st.tokens as usize,
                count: st.count,
                note: (!notes.is_empty()).then(|| notes.join(", ")),
            }
        })
        .collect();
    let pos = segments
        .iter()
        .position(|s| s.label == "System-Prompt")
        .map_or(0, |p| p + 1);
    if let Some(schemas) = schema_segment(agent) {
        segments.insert(pos, schemas);
    }
    finish_report(segments, budget as usize, true)
}
