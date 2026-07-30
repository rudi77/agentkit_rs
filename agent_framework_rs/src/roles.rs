//! Sub-Agent-Rollen — ein `task`-Tool im Stil von Claude Code.
//!
//! Der (Coding-)Agent bekommt EIN Tool — `task` — mit dem er eine Teilaufgabe an
//! einen eigenständigen Sub-Agenten delegiert (eigener Kontext, eigene Tool-Teilmenge).
//! Der Parameter `subagent_type` wählt die Rolle:
//!
//! - `general`  — voller Coding-Zugriff, für beliebige abgegrenzte Teilaufgaben.
//! - `explorer` — read-only Repo-Erkundung (list/glob/grep/read).
//! - `reviewer` — read-only Code-/Diff-Begutachtung.
//! - `tester`   — read-only + run_shell: führt Tests aus und berichtet.
//!
//! Eine Rolle ist reine Daten ([`AgentRole`]): ein System-Prompt + eine Tool-Teilmenge.
//! Eine neue Rolle = ein Eintrag mehr — oder eine **Markdown-Datei**
//! ([`load_roles_from_dir`], im CLI `--agents <ordner>`): je `.md` ein Custom-Agent,
//! Frontmatter = Metadaten, Body = System-Prompt — genau wie ein Skill.
//!
//! **Live-Trace:** Läuft der Orchestrator über einen EventBus, leitet das `task`-Tool
//! ALLE Events des Sub-Agenten in denselben Bus weiter — getaggt mit der Rolle als
//! `source`. Mehrere `task`-Aufrufe aus EINER Antwort laufen parallel.
//!
//! **Grenzen:** Sub-Agenten bekommen NUR ihre Coding-Tools (kein `task`-Tool) → genau
//! eine Ebene tief, keine Rekursion. Schreibfähige Sub-Agenten (`general`) teilen sich
//! den EINEN Workspace — parallele Schreiber können kollidieren.

use crate::agent::{Agent, RunHandle, Strategy};
use crate::coding::{CodingTools, HELPER_TOKEN_BUDGET, READ_ONLY_TOOLS};
use crate::llm::Llm;
use crate::mcp::McpHub;
use crate::skills::{body_after_frontmatter, parse_frontmatter};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

/// Loop-Schritte eines Sub-Agenten. Der Builder-Default (12) reicht für eine
/// abgegrenzte Teilaufgabe nicht — ein Explorer verbraucht allein fürs Suchen und
/// Lesen mehrere Schritte. Derselbe Wert wie `SwarmLimits::max_steps`, damit
/// delegierte Arbeit überall gleich viel Luft hat.
///
/// Großzügig bemessen: die Schranke ist ein Schutz gegen Endlosschleifen, kein
/// Zeitbudget. Bei 40 endete ein Explorer, der ein größeres Repo erkundet, mitten
/// in der Arbeit mit "(max_steps erreicht)" — und der Aufrufer bekam eine halbe
/// Antwort, ohne zu erfahren, dass sie halb war. Was den Verbrauch wirklich
/// begrenzt, ist das Token-Budget und das Kontingent des Providers.
pub const SUBAGENT_MAX_STEPS: usize = 200;

/// Strategie aus einem Frontmatter-/CLI-String (Default ReAct).
pub fn strategy_from_str(s: &str) -> Strategy {
    match s.trim().to_lowercase().as_str() {
        "plan" => Strategy::Plan,
        "plain" => Strategy::Plain,
        _ => Strategy::React,
    }
}

/// Eine vordefinierte Sub-Agent-Rolle: System-Prompt + erlaubte Tool-Teilmenge.
#[derive(Clone)]
pub struct AgentRole {
    /// Rollenname (z. B. "explorer").
    pub name: String,
    /// WANN diese Rolle nutzen (fürs Orchestrator-LLM, wandert ins Schema).
    pub description: String,
    /// System-Prompt des Sub-Agenten.
    pub system: String,
    /// Coding-Tool-Namen; `None` = alle Tools.
    pub tools: Option<Vec<String>>,
    pub strategy: Strategy,
}

impl AgentRole {
    fn new(name: &str, description: &str, system: &str, tools: Option<&[&str]>) -> Self {
        AgentRole {
            name: name.to_string(),
            description: description.to_string(),
            system: system.to_string(),
            tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
            strategy: Strategy::React,
        }
    }
}

// --------------------------------------------------------------- Rollen-Presets
const EXPLORER_SYS: &str =
    "Du bist ein Explorer-Sub-Agent. Erkunde das Projekt mit list_files/glob_files/\
grep/read_file, finde die für den Auftrag relevanten Dateien und Stellen und \
gib eine KOMPAKTE Zusammenfassung zurück: relevante Pfade (mit Zeilen), \
Kernfunktionen/-klassen und wie sie zusammenhängen. Du änderst NICHTS.";

const REVIEWER_SYS: &str =
    "Du bist ein Reviewer-Sub-Agent. Lies den genannten Code/Diff und begutachte ihn \
kritisch: Bugs, Grenzfälle, Risiken, Stil/Qualität. Liefere konkrete Findings mit \
Datei:Zeile und je einem kurzen Verbesserungsvorschlag. Du änderst NICHTS.";

const TESTER_SYS: &str =
    "Du bist ein Tester-Sub-Agent. Finde und führe die relevanten Tests/Befehle aus \
(z. B. 'pytest …') mit run_shell und berichte das Ergebnis: was lief, Pass/Fail \
und bei Fehlern die entscheidenden Fehlermeldungen. Du änderst KEINEN Code.";

pub const GENERAL_SUBAGENT_SYSTEM: &str =
    "Du bist ein fokussierter Sub-Agent. Erledige GENAU den übergebenen Auftrag \
eigenständig mit deinen Tools und gib am Ende ein knappes, in sich geschlossenes \
Ergebnis zurück — dein Aufrufer sieht nur diese finale Antwort, nicht deinen Verlauf.";

/// Hinweis für den Orchestrator-System-Prompt (wird angehängt, wenn `task` aktiv ist).
///
/// Steht bewusst HIER und nicht in [`crate::CODING_SYSTEM`]: mit `--no-subagents`
/// gibt es kein `task`-Tool, und ein Prompt, der ein fehlendes Werkzeug bewirbt,
/// wäre schlimmer als gar kein Hinweis. `app.rs` hängt den Block nur an, wenn
/// `cfg.subagents` gesetzt ist — dieselbe Bedingung, unter der
/// `coding::coding_system` die delegierende Orientierungsregel wählt. Die beiden
/// gehören zusammen: der Block hier nennt die Auslöser, die Orientierungsregel
/// dort sorgt dafür, dass ihnen keine gegenteilige Anweisung vorausgeht.
pub const SUBAGENT_SYSTEM: &str =
    "Du kannst Teilaufgaben an eigenständige Sub-Agenten delegieren — mit dem Tool \
'task'. Gib einen klaren 'prompt' (die Mission) und einen 'subagent_type' mit:\n\
- general: beliebige abgegrenzte Teilaufgabe (voller Coding-Zugriff)\n\
- explorer: Repo erkunden / relevante Stellen finden (read-only)\n\
- reviewer: Code oder Diff kritisch begutachten (read-only)\n\
- tester: Tests ausführen und Ergebnis berichten\n\
Optional kannst du mit 'system' einen eigenen System-Prompt für einen Ad-hoc-Agenten \
vorgeben.\n\
Von einem Sub-Agenten kommt NUR dessen finale Antwort zurück — die Dateiinhalte, \
Suchtreffer und Testausgaben, die er unterwegs gesehen hat, landen NICHT in deinem \
Kontext. Das ist der Hauptgrund zu delegieren: dein Kontext bleibt klein, und du \
behältst über den ganzen Auftrag den Überblick, statt ihn mit Zwischenergebnissen \
zuzuschütten.\n\
Delegiere deshalb:\n\
- Orientierung in unbekanntem Code, sobald dafür mehr als zwei, drei Dateien zu lesen \
wären -> explorer; lass dir die relevanten Stellen mit Pfad und Zeile nennen.\n\
- Tests, Builds oder Shell-Läufe mit langer Ausgabe -> tester; lass dir das Ergebnis \
und nur die entscheidenden Fehlermeldungen berichten.\n\
- Begutachtung von Code oder Diffs -> reviewer.\n\
- Mehrere unabhängige Teilaufgaben -> rufe 'task' MEHRFACH in DERSELBEN Antwort auf \
(sie laufen dann parallel).\n\
Selbst erledigst du: gezieltes Nachlesen einzelner Stellen, die du schon kennst, alle \
Datei-Änderungen und den finalen Zusammenbau samt Antwort an den Nutzer. Delegiere \
nichts Triviales — jeder Sub-Agent ist ein eigener Modell-Lauf, und mehrere \
gleichzeitig belasten dein Ratenlimit. Sub-Agenten teilen sich den Workspace: lass \
nicht mehrere gleichzeitig dieselben Dateien schreiben.";

/// Die eingebauten Rollen (explorer, reviewer, tester) — in dieser Reihenfolge.
/// `general` ist implizit (voller Zugriff) und wird vom `task`-Tool ergänzt.
pub fn builtin_roles() -> Vec<AgentRole> {
    vec![
        AgentRole::new(
            "explorer",
            "Read-only Repo-Erkundung: relevante Dateien/Stellen finden und zusammenfassen.",
            EXPLORER_SYS,
            Some(READ_ONLY_TOOLS),
        ),
        AgentRole::new(
            "reviewer",
            "Read-only Code-/Diff-Begutachtung: Bugs, Risiken, Qualität mit konkreten Findings.",
            REVIEWER_SYS,
            Some(READ_ONLY_TOOLS),
        ),
        AgentRole::new(
            "tester",
            "Führt Tests/Befehle aus und berichtet Pass/Fail samt Fehlermeldungen (kein Code-Edit).",
            TESTER_SYS,
            Some(&["list_files", "glob_files", "grep", "read_file", "run_shell"]),
        ),
    ]
}

// ----------------------------------------------- Custom-Rollen aus Markdown

/// `tools:`-Feld -> Tool-Teilmenge. Fehlt/leer = `None` (alle Tools); `read_only`
/// = die read-only-Teilmenge; sonst eine Komma-/Leerzeichen-Liste von Tool-Namen.
///
/// Öffentlich, weil `agentkit-swarm` dieselbe Schreibweise für die Tool-Auswahl
/// seiner dynamisch erzeugten Schwarm-Mitglieder benutzt — eine Sprache für
/// Rollen-Markdown und Schwarm-Spezifikation.
pub fn parse_tools_field(field: Option<&str>) -> Option<Vec<String>> {
    let field = field.unwrap_or("").trim();
    if field.is_empty() {
        return None;
    }
    if matches!(
        field.to_lowercase().as_str(),
        "read_only" | "readonly" | "read-only"
    ) {
        return Some(READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect());
    }
    let names: Vec<String> = field
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// Lädt Custom-Rollen aus `*.md`-Dateien eines Verzeichnisses. Liefert eine (ggf.
/// leere) Liste in alphabetischer Dateireihenfolge. Gedacht zum Mergen über die
/// eingebauten Rollen via [`merge_roles`].
pub fn load_roles_from_dir(path: &str) -> Vec<AgentRole> {
    let dir = std::path::Path::new(path);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for p in files {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let fm = parse_frontmatter(&text);
        let get = |k: &str| fm.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        let name = get("name")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            });
        out.push(AgentRole {
            name,
            description: get("description").unwrap_or("").to_string(),
            system: body_after_frontmatter(&text).trim().to_string(),
            tools: parse_tools_field(get("tools")),
            strategy: strategy_from_str(get("strategy").unwrap_or("react")),
        });
    }
    out
}

/// Mergt Custom-Rollen über die Basis-Rollen: gleichnamige `extra` überschreiben,
/// neue werden angehängt (entspricht Pythons `{**ROLES, **custom}`).
pub fn merge_roles(base: Vec<AgentRole>, extra: Vec<AgentRole>) -> Vec<AgentRole> {
    let mut out = base;
    for role in extra {
        if let Some(slot) = out.iter_mut().find(|r| r.name == role.name) {
            *slot = role;
        } else {
            out.push(role);
        }
    }
    out
}

// --------------------------------------------------------------- task-Tool

/// Ein fertig konfigurierter Rollen-Slot fürs `task`-Tool.
struct RoleEntry {
    registry: ToolRegistry,
    system: String,
    strategy: Strategy,
    description: String,
}

fn build_registry(coding: &CodingTools, only: Option<&[String]>) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    match only {
        None => coding.register(&mut reg, None),
        Some(names) => {
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            coding.register(&mut reg, Some(&refs));
        }
    }
    reg
}

/// Registriert das `task`-Tool im `registry` des Orchestrators.
///
/// `run` ist der geteilte Lauf-Kontext des Orchestrator-Agenten ([`Agent::run_handle`]),
/// der ZUR LAUFZEIT den aktiven Bus/Stop-Knopf liefert — so landen Sub-Agent-Events live
/// im selben Strom. Jeder Aufruf erzeugt einen FRISCHEN Sub-Agenten mit der Tool-Teilmenge
/// seiner Rolle.
///
/// `coding` sind die Sandbox-Tools des Orchestrators — dieselbe Instanz, damit
/// Sub-Agenten garantiert im selben Workspace und unter denselben Regeln arbeiten.
///
/// `mcp` ist der geteilte [`McpHub`]: beim Spawnen eines Sub-Agenten werden die GERADE
/// aktiven MCP-Server-Tools zusätzlich zu seinen Coding-Tools eingeklinkt — so wirkt ein
/// Toggle im Frontend sofort auch auf neue Sub-Agenten, ohne den Orchestrator neu zu bauen.
///
/// `dry_run` gilt für die FERTIGE Registry jedes Sub-Agenten (Coding- UND MCP-Tools):
/// `--dry-run` muss über die Delegationsgrenze hinweg halten, sonst schreibt der
/// Sub-Agent, was der Orchestrator selbst nicht darf.
/// Was das `task`-Tool von seinem Frontend braucht.
///
/// Als Struct und nicht als Parameterliste — dasselbe Muster wie
/// `agentkit_swarm::SwarmToolConfig` und aus demselben Grund: mit dem
/// Helfer-Kontext wären es acht Positionen, und clippy zieht bei sieben die
/// Grenze.
pub struct TaskToolConfig {
    /// Lauf-Kontext des Orchestrators (Bus + Stop-Knopf zur Laufzeit).
    pub run: RunHandle,
    /// Geteiltes LLM aller Sub-Agenten.
    pub llm: Arc<dyn Llm>,
    /// Die Sandbox-Tools des Orchestrators — dieselbe Instanz für alle.
    pub coding: CodingTools,
    /// Eingebaute + geladene Rollen (`--agents DIR`).
    pub roles: Vec<AgentRole>,
    /// Geteilter MCP-Hub; Sub-Agenten bekommen die beim Bau aktiven Server-Tools.
    pub mcp: Arc<McpHub>,
    /// `--dry-run` des Orchestrators — gilt auch für die Registry jedes Sub-Agenten.
    pub dry_run: bool,
    /// Token-Budget für einen verwalteten Helfer-Kontext (ctxman), oder `None`.
    /// Gesetzt, wenn das Frontend `--ctx` aktiviert hat — dann bekommt JEDER
    /// Sub-Agent seinen eigenen, nicht persistenten Kontext.
    pub helper_ctx_budget: Option<u32>,
}

pub fn add_task_tool(registry: &mut ToolRegistry, cfg: TaskToolConfig) {
    let TaskToolConfig {
        run,
        llm,
        coding,
        roles,
        mcp,
        dry_run,
        helper_ctx_budget,
    } = cfg;
    // Ohne Feature `ctxman` gibt es keinen Helfer-Kontext — das Feld bleibt Teil
    // der Konfiguration (die Frontends setzen es unabhängig vom Feature).
    #[cfg(not(feature = "ctxman"))]
    let _ = helper_ctx_budget;
    // Pro Rolle einen Slot bauen; 'general' (voller Zugriff) ergänzen, falls keine
    // Datei ihn überschreibt.
    let mut entries: Vec<(String, RoleEntry)> = Vec::new();
    let mut has_general = false;
    for role in &roles {
        if role.name == "general" {
            has_general = true;
        }
        entries.push((
            role.name.clone(),
            RoleEntry {
                registry: build_registry(&coding, role.tools.as_deref()),
                system: role.system.clone(),
                strategy: role.strategy,
                description: role.description.clone(),
            },
        ));
    }
    if !has_general {
        entries.push((
            "general".to_string(),
            RoleEntry {
                registry: build_registry(&coding, None),
                system: GENERAL_SUBAGENT_SYSTEM.to_string(),
                strategy: Strategy::React,
                description: "beliebige Teilaufgabe (voller Zugriff)".to_string(),
            },
        ));
    }

    // Typen fürs Schema: alle außer 'general', dann 'general' als letzten.
    let mut types: Vec<String> = roles
        .iter()
        .map(|r| r.name.clone())
        .filter(|n| n != "general")
        .collect();
    types.push("general".to_string());
    let type_doc = types
        .iter()
        .map(|k| {
            let desc = entries
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, e)| e.description.as_str())
                .unwrap_or("");
            format!("{k}: {desc}")
        })
        .collect::<Vec<_>>()
        .join("; ");

    let params = json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string",
                       "description": "Die Mission/Teilaufgabe für den Sub-Agenten, in Worten."},
            "subagent_type": {"type": "string", "enum": types, "default": "general",
                              "description": format!("Welche Rolle. Verfügbar — {type_doc}")},
            "system": {"type": "string",
                       "description": "Optional: eigener System-Prompt für einen Ad-hoc-Agenten (überschreibt die Rolle)."}
        },
        "required": ["prompt"]
    });

    let entries = Arc::new(entries);
    registry.add(
        "task",
        "Delegiert eine Teilaufgabe an einen eigenständigen Sub-Agenten und gibt dessen \
Ergebnis zurück. Für mehrere unabhängige Aufgaben mehrfach in DERSELBEN Antwort \
aufrufen (laufen parallel).",
        params,
        move |args: Value| {
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if prompt.is_empty() {
                return Ok("ERROR: 'prompt' (die Mission) fehlt.".to_string());
            }
            let kind = args
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("general");
            // Rolle suchen; unbekannt -> 'general'.
            let entry = entries
                .iter()
                .find(|(k, _)| k.as_str() == kind)
                .or_else(|| entries.iter().find(|(k, _)| k == "general"))
                .map(|(_, e)| e);
            let Some(entry) = entry else {
                return Ok("ERROR: keine Sub-Agent-Rolle verfügbar.".to_string());
            };

            // System-Prompt: expliziter 'system'-Override > Rolle.
            let system = args
                .get("system")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| entry.system.clone());

            // Coding-Tool-Teilmenge der Rolle + die gerade aktiven MCP-Server-Tools.
            let mut reg = entry.registry.clone();
            mcp.register_enabled(&mut reg);
            if dry_run {
                reg = reg.dry_run_blocking(crate::is_likely_destructive);
            }
            // Eigener, nicht persistenter ctxman-Kontext je Sub-Agent: hält seine
            // Anfragen klein (Externalisierung großer Tool-Ergebnisse), ohne dass
            // parallele Sub-Agenten sich einen Snapshot teilen müssten. Scheitert
            // der Aufbau, läuft der Sub-Agent wie bisher — ein fehlender Kontext
            // ist kein Grund, die Teilaufgabe abzubrechen.
            #[cfg(feature = "ctxman")]
            let kontext = helper_ctx_budget
                .and_then(|b| crate::ManagedContext::ephemeral(b, llm.clone()).ok());
            // Das `expand_context_ref`-Tool muss VOR dem Bau in die Registry —
            // der Agent kopiert sie beim Bauen.
            #[cfg(feature = "ctxman")]
            if let Some(ctx) = &kontext {
                let _ = ctx.set_system(&system);
                ctx.register_tool(&mut reg);
            }
            let mut sub = Agent::builder(llm.clone())
                .tools(reg)
                .system(&system)
                .strategy(entry.strategy)
                .max_steps(SUBAGENT_MAX_STEPS)
                .token_budget(HELPER_TOKEN_BUDGET)
                .build();
            #[cfg(feature = "ctxman")]
            {
                sub.context = kontext;
            }

            // Ein Sub-Agent ist ein normaler Agent — als Tool ausgeführt. Bus/Stop
            // kommen aus dem Lauf-Kontext des Orchestrators (live, nicht zur
            // Registrierzeit fixiert), damit Events sofort im selben Strom landen.
            Ok(sub.run_as_subagent(&prompt, kind, run.bus().as_ref(), run.cancel().as_ref()))
        },
    );
}
