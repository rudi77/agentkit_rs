//! Skills — Wissen/Vorgehen als Datei, on demand geladen (progressive disclosure).
//!
//! Ein **Skill** ist — nach dem offenen Agent-Skills-Standard — ein Ordner mit
//! einer `SKILL.md`: YAML-Frontmatter (`name`, `description`) + Anleitung.
//! Permanent im Kontext liegt nur der schlanke Index (`list_skills`); die
//! ausführliche Anleitung holt der Agent erst per `read_skill(name)`.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const SKILL_SYSTEM: &str =
    "Du hast Zugriff auf Skills — vorgefertigte Arbeitsanweisungen als Dateien. \
Arbeitsweise: Rufe ZUERST list_skills auf und wähle den passenden Skill. \
Lade ihn dann mit read_skill(name) und folge seiner Anleitung EXAKT. \
Passt kein Skill, arbeite normal weiter.";

/// Erkennt, ob ein getrimmter Frontmatter-Wert ein YAML-Block-Scalar-Indikator ist
/// (`>`, `>-`, `>+`, `|`, `|-`, `|+`, optional mit ignoriertem Indentation-Indikator
/// als Ziffer). Liefert `(gefaltet: bool, Chomping-Zeichen)`.
fn block_scalar_indicator(value: &str) -> Option<(bool, Option<char>)> {
    let mut chars = value.chars();
    let first = chars.next()?;
    if first != '>' && first != '|' {
        return None;
    }
    let folded = first == '>';
    let mut chomp = None;
    for c in chars {
        match c {
            '-' | '+' if chomp.is_none() => chomp = Some(c),
            '0'..='9' => {} // Indentation-Indikator — bewusst ignoriert
            _ => return None,
        }
    }
    Some((folded, chomp))
}

/// Faltet Block-Scalar-Zeilen nach YAML-`>`-Regeln: Zeilen mit Inhalt werden mit
/// einem Leerzeichen verbunden, eine Leerzeile erzeugt einen Zeilenumbruch.
fn fold_block_lines(lines: &[&str]) -> String {
    let mut out = String::new();
    let mut prev_had_content = false;
    for line in lines {
        if line.trim().is_empty() {
            out.push('\n');
            prev_had_content = false;
        } else {
            if prev_had_content {
                out.push(' ');
            }
            out.push_str(line.trim_end());
            prev_had_content = true;
        }
    }
    out
}

/// Rendert die eingesammelten Zeilen eines Block-Scalars (bereits von der
/// Key-Zeile getrennt, noch mit ihrer Original-Einrückung) zu einem String.
fn render_block_scalar(raw_lines: &[&str], folded: bool, chomp: Option<char>) -> String {
    let base_indent = raw_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);
    // Über das Präfix aus Leerzeichen abschneiden, NICHT über `&l[base_indent..]`:
    // Byte-Slicing würde bei einer Zeile mit geringerer Einrückung und
    // Nicht-ASCII-Inhalt mitten in ein UTF-8-Zeichen schneiden und panicken —
    // in einem Repo, dessen Texte durchgehend Umlaute enthalten, kein
    // theoretischer Fall. Wer weniger eingerückt ist, verliert nur seine
    // eigene Einrückung.
    let prefix = " ".repeat(base_indent);
    let stripped: Vec<&str> = raw_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                // Passt das Präfix nicht (Tab-Einrückung, weniger Leerzeichen),
                // bleibt nur der Inhalt ohne führenden Leerraum.
                l.strip_prefix(prefix.as_str())
                    .unwrap_or_else(|| l.trim_start())
            }
        })
        .collect();
    let mut result = if folded {
        fold_block_lines(&stripped)
    } else {
        stripped.join("\n")
    };
    let ohne_leerzeilen = result.trim_end_matches('\n').len();
    match chomp {
        // keep: alle Leerzeilen am Ende bleiben erhalten
        Some('+') => {}
        // strip: kein abschließender Zeilenumbruch (der von okf genutzte Fall)
        Some('-') => result.truncate(ohne_leerzeilen),
        // clip (Default): genau ein abschließender Zeilenumbruch
        _ => {
            result.truncate(ohne_leerzeilen);
            result.push('\n');
        }
    }
    result
}

/// Liest den YAML-Frontmatter-Block zwischen den ersten beiden `---`.
/// Unterstützt einzeilige `key: value`-Paare sowie YAML-Block-Scalars
/// (`>`, `>-`, `>+`, `|`, `|-`, `|+`) — der okf-Skill-Standard nutzt durchgehend
/// gefaltete Block-Scalars (`>-`) für mehrzeilige Beschreibungen.
pub fn parse_frontmatter(text: &str) -> Vec<(String, String)> {
    let mut meta = Vec::new();
    if !text.starts_with("---") {
        return meta;
    }
    let Some(end) = text[3..].find("\n---") else {
        return meta;
    };
    let block = &text[3..3 + end];
    // `\r` selbst abschneiden statt sich auf `lines()` zu verlassen: der Slice
    // endet VOR dem `\n` der schließenden `---`-Zeile, also trägt die letzte Zeile
    // bei CRLF-Dateien ein `\r` ohne folgendes `\n` — und genau dann strippt
    // `lines()` es nicht. Bei gefalteten Block-Scalars fiele das durch `trim_end`
    // auf, bei literalen (`|`) klebte das `\r` unsichtbar am Wert.
    let lines: Vec<&str> = block
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        let key_indent = line.len() - line.trim_start().len();
        let val_raw = v.trim();
        if let Some((folded, chomp)) = block_scalar_indicator(val_raw) {
            // Alle folgenden Zeilen einsammeln, die stärker eingerückt sind als die
            // Key-Zeile (oder leer) — DIESE dürfen niemals als neues key:value gelesen
            // werden, das war der eigentliche Bug.
            let mut block_lines: Vec<&str> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.trim().is_empty() {
                    block_lines.push(l);
                    i += 1;
                    continue;
                }
                let indent = l.len() - l.trim_start().len();
                if indent > key_indent {
                    block_lines.push(l);
                    i += 1;
                } else {
                    break;
                }
            }
            // Ein Indikator OHNE eine einzige Inhaltszeile (`tools: >-` am Ende des
            // Frontmatters, oder mit nicht eingerückter Folgezeile — beides ist kein
            // gültiges YAML) darf NICHT als leerer Wert durchgehen. Für einen
            // Beschreibungstext wäre das harmlos, für `tools:` nicht: leer heißt dort
            // „keine Auswahl getroffen" und damit ALLE Tools. Ein kaputtes Rollen-File
            // würde so mehr Rechte vergeben als eines ohne Tippfehler. Stattdessen
            // bleibt der Rohindikator stehen — ein Wert, den niemand auflösen kann und
            // der überall fail-closed landet.
            let wert = if block_lines.iter().all(|l| l.trim().is_empty()) {
                val_raw.to_string()
            } else {
                render_block_scalar(&block_lines, folded, chomp)
            };
            meta.push((key, wert));
        } else {
            let val = val_raw.trim_matches(|c| c == '\'' || c == '"').to_string();
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

/// Kanonisiert einen Skill-Ordnerpfad zu einem absoluten Pfad mit
/// Forward-Slashes und ohne Windows' Verbatim-Präfix. Forward-Slashes, weil der
/// Pfad in Shell-Kommandos landet und BEIDE Shells von `run_shell` sie
/// akzeptieren (PowerShell auf Windows, sonst bash) — Backslashes wären dort
/// Escape-Zeichen. Schlägt `canonicalize` fehl, wird der Pfad unverändert (nur
/// mit ersetzten Backslashes) übernommen.
///
/// Windows kennt ZWEI Verbatim-Formen, und nur eine davon lässt sich durch
/// bloßes Abschneiden auflösen: aus `\\?\D:\pfad` wird `D:/pfad`, aus dem
/// UNC-Fall `\\?\UNC\server\share` muss dagegen wieder `//server/share`
/// werden. Würde man dort nur die vier Zeichen `\\?\` entfernen, bliebe `UNC`
/// als literales erstes Pfadsegment stehen und der Pfad zeigte ins Leere.
fn canonical_skill_dir(dir: &Path) -> String {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let raw = canon.to_string_lossy().into_owned();
    let entpackt = match raw.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string(),
    };
    entpackt.replace('\\', "/")
}

/// Ein Eintrag im schlanken Index.
#[derive(Clone, Debug, serde::Serialize, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Ein entdeckter Skill: Rohdaten für Index UND vollständiges Lesen, bereits
/// dedupliziert (siehe `Skills::discover`).
struct DiscoveredSkill {
    dir: PathBuf,
    name: String,
    description: String,
    content: String,
}

/// Entdeckt Skills (Ordner mit `SKILL.md`) und bietet sie dem Agenten als Tools an.
#[derive(Clone)]
pub struct Skills {
    dirs: Vec<PathBuf>,
}

impl Skills {
    /// Erschließt Skills aus einem oder mehreren Wurzelverzeichnissen. Mehrere
    /// Verzeichnisse werden mit `;` getrennt — bewusst nicht `:`, das würde einen
    /// Windows-Pfad wie `D:/pfad` zerreißen. Leere Segmente (führendes,
    /// aufeinanderfolgendes oder abschließendes `;`) werden ignoriert; ein
    /// einzelner Pfad ohne `;` verhält sich exakt wie zuvor. Innerhalb eines
    /// Wurzelverzeichnisses werden Skill-Ordner alphabetisch aufgenommen, die
    /// Wurzelverzeichnisse selbst in der angegebenen Reihenfolge. Bei einer
    /// Namenskollision (gleicher Ordnername ODER gleicher Frontmatter-`name` in
    /// zwei Wurzelverzeichnissen) gewinnt das ZUERST genannte Verzeichnis.
    pub fn new(skills_dirs: &str) -> Self {
        let dirs = skills_dirs
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        Skills { dirs }
    }

    /// Sammelt alle Skills über sämtliche Wurzelverzeichnisse, dedupliziert nach
    /// Ordnername und nach aufgelöstem Skill-Namen (jeweils gewinnt das erste
    /// Vorkommen).
    fn discover(&self) -> Vec<DiscoveredSkill> {
        let mut out = Vec::new();
        let mut seen_folders: HashSet<String> = HashSet::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        for root in &self.dirs {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            // `Path::is_dir()` (nicht `DirEntry::file_type()`): es folgt Symlinks,
            // und ein per Symlink eingehängter Skill-Ordner ist bei geteilten
            // Sammlungen der Normalfall. Der eingesparte `stat` je Eintrag wäre
            // den Verlust nicht wert.
            let mut sub_dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            sub_dirs.sort();
            for d in sub_dirs {
                let folder = d
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let skill_md = d.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                if seen_folders.contains(&folder) {
                    continue;
                }
                let content = std::fs::read_to_string(&skill_md).unwrap_or_default();
                let fm = parse_frontmatter(&content);
                let name = frontmatter_get(&fm, "name").unwrap_or(&folder).to_string();
                if seen_names.contains(&name) {
                    continue;
                }
                let description = frontmatter_get(&fm, "description")
                    .unwrap_or("")
                    .to_string();
                seen_folders.insert(folder);
                seen_names.insert(name.clone());
                out.push(DiscoveredSkill {
                    dir: d,
                    name,
                    description,
                    content,
                });
            }
        }
        out
    }

    /// Nur das Frontmatter jedes Skills — der schlanke Index.
    pub fn index(&self) -> Vec<SkillInfo> {
        self.discover()
            .into_iter()
            .map(|s| SkillInfo {
                name: s.name,
                description: s.description,
            })
            .collect()
    }

    /// Listet verfügbare Skills (Name + Beschreibung) als JSON.
    pub fn list_skills(&self) -> String {
        serde_json::to_string_pretty(&self.index()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Lädt die vollständige Anleitung (SKILL.md) eines Skills — gefunden über
    /// Frontmatter-Name oder Ordnernamen. Entspricht `read_skill_with_arguments(name, None)`.
    pub fn read_skill(&self, name: &str) -> String {
        self.read_skill_with_arguments(name, None)
    }

    /// Wie `read_skill`, ersetzt zusätzlich im Text `${CLAUDE_SKILL_DIR}` durch den
    /// absoluten Pfad des Skill-Ordners (nicht des Skills-Wurzelverzeichnisses) und
    /// `$ARGUMENTS` durch `arguments` (leerer String, falls `None`).
    pub fn read_skill_with_arguments(&self, name: &str, arguments: Option<&str>) -> String {
        for s in self.discover() {
            let folder = s.dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if s.name == name || folder == name {
                let dir_str = canonical_skill_dir(&s.dir);
                let text = s.content.replace("${CLAUDE_SKILL_DIR}", &dir_str);
                let text = text.replace("$ARGUMENTS", arguments.unwrap_or(""));
                return text;
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
                   "properties": {
                       "name": {"type": "string", "description": "Name des Skills (aus list_skills)."},
                       "arguments": {"type": "string", "description": "Argumente, die im Skill-Text `$ARGUMENTS` ersetzen — optional."}
                   },
                   "required": ["name"]}),
            move |args: Value| {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = args.get("arguments").and_then(Value::as_str);
                Ok(me.read_skill_with_arguments(name, arguments))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, frontmatter: &str, body: &str) {
        let d = root.join(folder);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    /// Ein eigenes Temp-Verzeichnis je Test — `tag` trennt die parallel laufenden
    /// Tests voneinander, die PID trennt parallele Testläufe.
    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentkit_skillsrs_{tag}_{}", std::process::id()))
    }

    #[test]
    fn parse_frontmatter_folded_block_scalar_no_garbage_key() {
        // Raw-String bewusst statt Escape-Fortsetzungen (`\` am Zeilenende in einem
        // normalen "..."-Literal frisst den Zeilenumbruch UND die folgende
        // Einrückung — das würde genau die Einrückung zerstören, die dieser Test
        // prüfen soll).
        let text = r#"---
name: backfill
description: >-
  Reconstruct an OKF bundle by event-sourcing a repository's history (git log
  and Claude session transcripts). Use when creating an `.okf/` bundle for an
  existing repository. Triggers on: "reconstruct the OKF bundle", "backfill the
  knowledge bundle".
user-invocable: true
---

Body
"#;
        let meta = parse_frontmatter(text);
        assert_eq!(frontmatter_get(&meta, "name"), Some("backfill"));
        let desc = frontmatter_get(&meta, "description").unwrap();
        assert!(
            !desc.contains('\n'),
            "gefaltete Beschreibung soll einzeilig sein: {desc:?}"
        );
        assert!(desc.starts_with("Reconstruct an OKF bundle"));
        assert!(
            desc.ends_with("knowledge bundle\"."),
            "kein sauberes Ende: {desc:?}"
        );
        assert!(
            desc.contains("Triggers on:"),
            "Doppelpunkt im Fließtext muss erhalten bleiben"
        );
        assert_eq!(
            frontmatter_get(&meta, "user-invocable"),
            Some("true"),
            "Folgezeile nach dem Block muss wieder normal geparst werden"
        );
        // Der eigentliche Bug: kein Müll-Key aus einer Zeile innerhalb des Blocks.
        assert!(
            meta.iter()
                .all(|(k, _)| k == "name" || k == "description" || k == "user-invocable"),
            "unerwarteter Müll-Key entstanden: {meta:?}"
        );
    }

    /// CRLF: der Frontmatter-Slice endet vor dem `\n` der schließenden
    /// `---`-Zeile, die letzte Blockzeile trägt also ein `\r`, das `lines()` nicht
    /// strippt. Bei einem literalen Block-Scalar klebte es sonst unsichtbar am
    /// Wert — auf Windows-Checkouts der Normalfall.
    #[test]
    fn parse_frontmatter_crlf_laesst_kein_wagenruecklauf_zeichen_stehen() {
        let text =
            "---\r\nname: x\r\nnotes: |-\r\n  erste Zeile\r\n  letzte Zeile\r\n---\r\n\r\nBody\r\n";
        let meta = parse_frontmatter(text);
        assert_eq!(frontmatter_get(&meta, "name"), Some("x"));
        assert_eq!(
            frontmatter_get(&meta, "notes"),
            Some("erste Zeile\nletzte Zeile")
        );
    }

    /// Rechte-Grenze: ein Block-Scalar-Indikator ohne Inhaltszeile darf nicht zu
    /// einem LEEREN Wert kollabieren. Für `tools:` in einer Rollen-Datei hieße leer
    /// „keine Auswahl" und damit ALLE Tools — ein kaputtes Rollen-File bekäme mehr
    /// Rechte als ein korrektes. Der Rohindikator bleibt stehen und ist überall ein
    /// nicht auflösbarer Wert.
    #[test]
    fn parse_frontmatter_leerer_block_scalar_kollabiert_nicht_zu_leer() {
        // Fortsetzungszeile nicht eingerückt (kein gültiges YAML) …
        let ohne_einrueckung = "---\nname: r\ntools: >-\nread_only\n---\n\nBody\n";
        assert_eq!(
            frontmatter_get(&parse_frontmatter(ohne_einrueckung), "tools"),
            Some(">-")
        );
        // … und gar keine Folgezeile.
        let ohne_inhalt = "---\nname: r\ntools: >-\n---\n\nBody\n";
        assert_eq!(
            frontmatter_get(&parse_frontmatter(ohne_inhalt), "tools"),
            Some(">-")
        );
    }

    /// Ungleichmäßige Einrückung innerhalb eines Block-Scalars darf nicht
    /// panicken. Die zweite Zeile ist schwächer eingerückt als die erste und
    /// beginnt mit Mehrbyte-Zeichen — ein byte-basiertes `&l[base_indent..]`
    /// schneidet dabei mitten in ein UTF-8-Zeichen.
    #[test]
    fn parse_frontmatter_block_scalar_ueberlebt_schiefe_einrueckung_mit_umlauten() {
        let text = "---\nname: x\ndescription: >-\n    Erste Zeile\n äöü weiter\n---\n\nBody\n";
        let meta = parse_frontmatter(text);
        let desc = frontmatter_get(&meta, "description").unwrap();
        assert!(desc.contains("Erste Zeile"), "{desc:?}");
        assert!(desc.contains("äöü weiter"), "{desc:?}");
    }

    #[test]
    fn parse_frontmatter_literal_block_scalar_keeps_newlines() {
        let text = r#"---
name: lit
body: |-
  erste Zeile
  zweite Zeile
    eingerückt
next: ok
---

Body
"#;
        let meta = parse_frontmatter(text);
        let body = frontmatter_get(&meta, "body").unwrap();
        assert!(
            body.contains('\n'),
            "literal block muss Zeilenumbrüche behalten: {body:?}"
        );
        assert_eq!(body, "erste Zeile\nzweite Zeile\n  eingerückt");
        assert_eq!(frontmatter_get(&meta, "next"), Some("ok"));
    }

    #[test]
    fn parse_frontmatter_single_line_values_unchanged() {
        let text = "---\nname: okf\ndescription: 'Ein einfacher Skill'\nargument-hint: \"[dir]\"\n---\n\nBody\n";
        let meta = parse_frontmatter(text);
        assert_eq!(frontmatter_get(&meta, "name"), Some("okf"));
        assert_eq!(
            frontmatter_get(&meta, "description"),
            Some("Ein einfacher Skill")
        );
        assert_eq!(frontmatter_get(&meta, "argument-hint"), Some("[dir]"));
    }

    #[test]
    fn read_skill_replaces_skill_dir_and_arguments() {
        let dir = temp_dir("readargs");
        write_skill(
            &dir,
            "myskill",
            "name: myskill\ndescription: test",
            "uv run \"${CLAUDE_SKILL_DIR}/scripts/run.py\" $ARGUMENTS",
        );
        let sk = Skills::new(dir.to_str().unwrap());
        let skill_dir = std::fs::canonicalize(dir.join("myskill")).unwrap();
        let expected_dir = skill_dir.to_string_lossy().replace('\\', "/");
        let expected_dir = expected_dir.strip_prefix("//?/").unwrap_or(&expected_dir);

        let with_args = sk.read_skill_with_arguments("myskill", Some("--foo bar"));
        assert!(
            with_args.contains(expected_dir),
            "{with_args:?} sollte {expected_dir:?} enthalten"
        );
        assert!(with_args.contains("--foo bar"));
        assert!(!with_args.contains("${CLAUDE_SKILL_DIR}"));
        assert!(!with_args.contains("$ARGUMENTS"));

        let without_args = sk.read_skill("myskill");
        assert!(without_args.contains(expected_dir));
        assert!(
            without_args.ends_with("scripts/run.py\" \n") || without_args.contains("run.py\" \n")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skills_new_merges_multiple_semicolon_separated_dirs() {
        let dir_a = temp_dir("multia");
        let dir_b = temp_dir("multib");
        write_skill(&dir_a, "alpha", "name: alpha\ndescription: A", "Schritt 1.");
        write_skill(&dir_b, "beta", "name: beta\ndescription: B", "Schritt 1.");

        let combined = format!("{};{}", dir_a.to_str().unwrap(), dir_b.to_str().unwrap());
        let sk = Skills::new(&combined);
        let names: HashSet<_> = sk.index().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            ["alpha", "beta"].into_iter().map(String::from).collect()
        );
        assert!(sk.read_skill("alpha").contains("Schritt 1."));
        assert!(sk.read_skill("beta").contains("Schritt 1."));

        // Regression: ein einzelner Pfad ohne ';' verhält sich wie zuvor.
        let single = Skills::new(dir_a.to_str().unwrap());
        let single_names: HashSet<_> = single.index().into_iter().map(|s| s.name).collect();
        assert_eq!(
            single_names,
            ["alpha"].into_iter().map(String::from).collect()
        );

        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }

    #[test]
    fn skills_new_first_root_wins_on_name_collision() {
        let dir_a = temp_dir("cola");
        let dir_b = temp_dir("colb");
        write_skill(
            &dir_a,
            "shared",
            "name: shared\ndescription: aus A",
            "Von A",
        );
        write_skill(
            &dir_b,
            "shared",
            "name: shared\ndescription: aus B",
            "Von B",
        );

        let combined = format!("{};{}", dir_a.to_str().unwrap(), dir_b.to_str().unwrap());
        let sk = Skills::new(&combined);
        let idx = sk.index();
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].description, "aus A");
        assert!(sk.read_skill("shared").contains("Von A"));

        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }
}
