//! Skills — Wissen/Vorgehen als Datei, on demand geladen (progressive disclosure).
//!
//! Ein **Skill** ist — nach dem offenen Agent-Skills-Standard — ein Ordner mit
//! einer `SKILL.md`: YAML-Frontmatter (`name`, `description`) + Anleitung.
//! Permanent im Kontext liegt nur der schlanke Index (`list_skills`); die
//! ausführliche Anleitung holt der Agent erst per `read_skill(name)`.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SKILL_SYSTEM: &str =
    "Du hast Zugriff auf Skills — vorgefertigte Arbeitsanweisungen als Dateien. \
Arbeitsweise: Rufe ZUERST list_skills auf und wähle den passenden Skill. \
Lade ihn dann mit read_skill(name) und folge seiner Anleitung EXAKT. \
Passt kein Skill, arbeite normal weiter.";

/// Liest den YAML-Frontmatter-Block zwischen den ersten beiden `---`.
/// Bewusst minimal (einzeilige `key: value`-Paare) — deckt den Skill-Standard ab.
pub fn parse_frontmatter(text: &str) -> Vec<(String, String)> {
    let mut meta = Vec::new();
    if !text.starts_with("---") {
        return meta;
    }
    let Some(end) = text[3..].find("\n---") else {
        return meta;
    };
    let block = &text[3..3 + end];
    for line in block.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
            meta.push((key, val));
        }
    }
    meta
}

fn frontmatter_get<'a>(meta: &'a [(String, String)], key: &str) -> Option<&'a str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Liefert den Text NACH dem Frontmatter-Block (alles hinter dem zweiten `---`).
/// Bei fehlendem Frontmatter wird der gesamte Text zurückgegeben. Pendant zu Pythons
/// `body_after_frontmatter` — der Body IST z. B. der System-Prompt einer Rolle.
pub fn body_after_frontmatter(text: &str) -> &str {
    if !text.starts_with("---") {
        return text;
    }
    let Some(end) = text[3..].find("\n---") else {
        return text;
    };
    // Hinter die schließende `---`-Zeile springen: erst hinter "\n---", dann hinter
    // den nächsten Zeilenumbruch (Rest der Delimiter-Zeile verwerfen).
    let after = &text[3 + end + 4..];
    match after.find('\n') {
        Some(nl) => &after[nl + 1..],
        None => "",
    }
}

/// Ein Eintrag im schlanken Index.
#[derive(Clone, Debug, serde::Serialize, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Entdeckt Skills (Ordner mit `SKILL.md`) und bietet sie dem Agenten als Tools an.
#[derive(Clone)]
pub struct Skills {
    dir: PathBuf,
}

impl Skills {
    pub fn new(skills_dir: &str) -> Self {
        Skills {
            dir: PathBuf::from(skills_dir),
        }
    }

    /// Alle `*/SKILL.md`-Dateien, alphabetisch nach Ordnernamen.
    fn skill_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return files;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for d in dirs {
            let f = d.join("SKILL.md");
            if f.is_file() {
                files.push(f);
            }
        }
        files
    }

    /// Nur das Frontmatter jedes Skills — der schlanke Index.
    pub fn index(&self) -> Vec<SkillInfo> {
        let mut out = Vec::new();
        for p in self.skill_files() {
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let fm = parse_frontmatter(&content);
            let folder = p
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(SkillInfo {
                name: frontmatter_get(&fm, "name").unwrap_or(&folder).to_string(),
                description: frontmatter_get(&fm, "description")
                    .unwrap_or("")
                    .to_string(),
            });
        }
        out
    }

    /// Listet verfügbare Skills (Name + Beschreibung) als JSON.
    pub fn list_skills(&self) -> String {
        serde_json::to_string_pretty(&self.index()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Lädt die vollständige Anleitung (SKILL.md) eines Skills — gefunden über
    /// Frontmatter-Name oder Ordnernamen.
    pub fn read_skill(&self, name: &str) -> String {
        for p in self.skill_files() {
            let content = std::fs::read_to_string(&p).unwrap_or_default();
            let fm = parse_frontmatter(&content);
            let folder = p
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if frontmatter_get(&fm, "name") == Some(name) || folder == name {
                return content;
            }
        }
        format!("(kein Skill '{name}')")
    }

    /// Bietet dem Agenten `list_skills` / `read_skill` als Tools an.
    pub fn register(&self, registry: &mut ToolRegistry) {
        let me = self.clone();
        registry.add(
            "list_skills",
            "Listet verfügbare Skills (Name + Beschreibung). ZUERST aufrufen, um das \
             passende Vorgehen für die Aufgabe zu finden.",
            json!({"type": "object", "properties": {}, "required": []}),
            move |_args: Value| Ok(me.list_skills()),
        );
        let me = self.clone();
        registry.add(
            "read_skill",
            "Lädt die vollständige Anleitung (SKILL.md) eines Skills und befolgt sie.",
            json!({"type": "object",
                   "properties": {"name": {"type": "string", "description": "Name des Skills (aus list_skills)."}},
                   "required": ["name"]}),
            move |args: Value| {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                Ok(me.read_skill(name))
            },
        );
    }
}

/// Bequemer Helfer: registriert die Skill-Tools in einer (neuen) ToolRegistry.
pub fn skills_tools(registry: Option<ToolRegistry>, skills_dir: &str) -> ToolRegistry {
    let mut registry = registry.unwrap_or_default();
    Skills::new(skills_dir).register(&mut registry);
    registry
}
