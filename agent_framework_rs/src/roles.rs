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
use crate::coding::{ApproveFn, CodingTools, READ_ONLY_TOOLS};
use crate::llm::Llm;
use crate::mcp::McpHub;
use crate::skills::{body_after_frontmatter, parse_frontmatter};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

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
pub const SUBAGENT_SYSTEM: &str =
    "Du kannst Teilaufgaben an eigenständige Sub-Agenten delegieren — mit dem Tool \
'task'. Gib einen klaren 'prompt' (die Mission) und einen 'subagent_type' mit:\n\
- general: beliebige abgegrenzte Teilaufgabe (voller Coding-Zugriff)\n\
- explorer: Repo erkunden / relevante Stellen finden (read-only)\n\
- reviewer: Code oder Diff kritisch begutachten (read-only)\n\
- tester: Tests ausführen und Ergebnis berichten\n\
Optional kannst du mit 'system' einen eigenen System-Prompt für einen Ad-hoc-Agenten \
vorgeben. Nutze Sub-Agenten für gut abgegrenzte, parallelisierbare Arbeit und um \
deinen eigenen Kontext klein zu halten — für mehrere unabhängige Teilaufgaben rufe \
'task' MEHRFACH in DERSELBEN Antwort auf (sie laufen dann parallel). Triviales und \
den finalen Zusammenbau erledigst du selbst. Sub-Agenten teilen sich den Workspace: \
lass nicht mehrere gleichzeitig dieselben Dateien schreiben.";

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
fn parse_tools_field(field: Option<&str>) -> Option<Vec<String>> {
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
/// `mcp` ist der geteilte [`McpHub`]: beim Spawnen eines Sub-Agenten werden die GERADE
/// aktiven MCP-Server-Tools zusätzlich zu seinen Coding-Tools eingeklinkt — so wirkt ein
/// Toggle im Frontend sofort auch auf neue Sub-Agenten, ohne den Orchestrator neu zu bauen.
#[allow(clippy::too_many_arguments)]
pub fn add_task_tool(
    registry: &mut ToolRegistry,
    run: RunHandle,
    llm: Arc<dyn Llm>,
    workspace: &str,
    approval: bool,
    approve: Option<ApproveFn>,
    roles: Vec<AgentRole>,
    mcp: Arc<McpHub>,
) {
    // Coding-Tools EINMAL bauen (legt den Workspace an); Sub-Agenten teilen sie lesend.
    let coding = match approve {
        Some(a) => CodingTools::with_approve(workspace, approval, a, 120),
        None => CodingTools::new(workspace, approval),
    };

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
            let mut sub = Agent::builder(llm.clone())
                .tools(reg)
                .system(&system)
                .strategy(entry.strategy)
                .build();

            match run.bus() {
                None => Ok(sub.run(&prompt)),
                Some(bus) => {
                    let label: String = prompt
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(24)
                        .collect();
                    let source = format!("{kind}:{label}");
                    Ok(sub.run_on_bus(&prompt, &bus, -1, run.cancel().as_ref(), &source))
                }
            }
        },
    );
}
