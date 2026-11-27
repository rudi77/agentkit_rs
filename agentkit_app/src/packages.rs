//! Installierbare Skills und Plugins (`agentkit skills …`, `agentkit plugins …`).
//!
//! Ein **Skill** ist ein Ordner mit `SKILL.md` (wie bei `--skills`). Ein
//! **Plugin** bündelt Skills, Befehle und Rollen unter einem Manifest
//! (`plugin.json`) — nur Text: keine Hooks, keine MCP-Server. Installiert wird
//! nach `<config_dir>/skills/<name>/` bzw. `<config_dir>/plugins/<name>/`; ein
//! ausgeschaltetes Plugin liegt in `<config_dir>/plugins-disabled/<name>/`, so
//! braucht es keine Zustandsdatei.
//!
//! Aufbau (hexagonal): die Domäne — Paketformat, Prüfung von Namen, Pfaden,
//! Bestandteilen und Befehlseinstellungen — arbeitet auf [`Items`] im Speicher
//! und kennt weder Dateisystem noch Netz. Adapter sind das Dateisystem unter
//! dem Config-Verzeichnis (jede Funktion bekommt es als `root`, damit Tests ein
//! eigenes nehmen) und der Download über die Naht [`crate::upgrade::Netz`].
//!
//! Das Paketformat ist JSON — je Eintrag relative Pfade auf UTF-8-Text — statt
//! eines Archivs: es braucht keine neue Dependency, und was ein Archiv an
//! Gefahren mitbringt (Symlinks, Gerätedateien, Rechte, Binärdaten), kann das
//! Format gar nicht darstellen.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::upgrade::Netz;

/// Die Dateien eines Eintrags: relativer Pfad (mit `/`) → Text.
pub type Files = BTreeMap<String, String>;
/// Die Einträge eines Pakets: Name → Dateien.
pub type Items = BTreeMap<String, Files>;

/// Was installiert wird.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Skills,
    Plugins,
}

impl Kind {
    /// Verzeichnis unter `<config_dir>` und Wert von `kind` im Paket.
    fn dir(self) -> &'static str {
        match self {
            Kind::Skills => "skills",
            Kind::Plugins => "plugins",
        }
    }

    /// Das Release-Asset. Stabil und unversioniert wie alle Assets
    /// (`releases/latest/download/<asset>`).
    pub fn asset(self) -> &'static str {
        match self {
            Kind::Skills => "agentkit-skills.json",
            Kind::Plugins => "agentkit-plugins.json",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Skills => "Skill",
            Kind::Plugins => "Plugin",
        }
    }

    /// Die Datei, an der ein Ordner als Eintrag zu erkennen ist.
    fn marker(self) -> &'static str {
        match self {
            Kind::Skills => "SKILL.md",
            Kind::Plugins => "plugin.json",
        }
    }
}

const FORMAT: &str = "agentkit-paket";
const FORMAT_VERSION: u64 = 1;
const MAX_NAME: usize = 64;
/// Obergrenzen gegen ein versehentlich gepacktes `target/` oder ein
/// aufgeblähtes Paket — ein Skill ist Anleitung, keine Datensammlung.
const MAX_FILES: usize = 500;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 8;

/// Ausgeschaltete Plugins liegen hier statt unter `plugins/`.
const DISABLED_DIR: &str = "plugins-disabled";
/// Zwischenablage einer Installation, damit ein Fehler keinen halben Eintrag
/// hinterlässt. Liegt NEBEN `skills/` und `plugins/`, weil Skills alles unter
/// `skills/` laden würden, was eine `SKILL.md` hat.
const STAGING_DIR: &str = ".installieren";

/// Schlüssel, die ein Plugin-Manifest haben darf.
const MANIFEST_KEYS: &[&str] = &[
    "name",
    "version",
    "description",
    "author",
    "license",
    "homepage",
];

/// Einstellungen, die ein Plugin-Befehl setzen darf. Alles andere hebt
/// Freigaben auf (`yes`), startet Code (`hooks`, `mcp_config`, `mcp`),
/// erweitert die Sandbox (`allow_read`, `workspace`) oder liest Dateien
/// außerhalb des Plugins (`system_file`, `schema`, `skills`, `agents`,
/// `memory`, …) oder schaltet die `deny:`-Leitplanken des Projekts ab
/// (`no_project_instructions`) — das darf ein installiertes Paket nicht still tun.
///
/// `tools` fehlt bewusst: eine Werkzeug-Auswahl schaltet das Mitladen
/// installierter Skills ab (siehe `with_installed` im Binary), ein
/// Plugin-Befehl verlöre damit die Skills seines eigenen Plugins.
const COMMAND_KEYS: &[&str] = &[
    "description",
    "system",
    "strategy",
    "max_steps",
    "token_limit",
    "model",
    "format",
    "dry_run",
    "verify",
    "check",
    "shell_timeout",
    "timeout",
    "no_subagents",
    "no_swarm",
];

/// Nur abschaltbar, nicht auf einen Pfad setzbar.
const COMMAND_OFF_KEYS: &[&str] = &["graph", "ctx"];

// ------------------------------------------------------------------ Domäne

/// Ein gültiger Name: `[a-z0-9-]`, beginnt mit Buchstabe oder Ziffer,
/// höchstens 64 Zeichen. Er wird Verzeichnisname — mehr braucht es nicht, und
/// so kann er weder Pfad noch Option sein.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Prüft einen relativen Pfad im Paket. Erlaubt sind nur Bestandteile aus
/// `[A-Za-z0-9._-]`, die nicht mit `.` beginnen oder enden — damit gibt es
/// kein `..`, kein `/`-Präfix, kein `\`, kein `C:`, keine versteckte Datei
/// (`.mcp.json`) und keinen Namen, den Windows anders liest.
fn check_path(rel: &str) -> Result<(), String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() > MAX_DEPTH {
        return Err(format!("»{rel}« ist tiefer als {MAX_DEPTH} Ebenen"));
    }
    for part in &parts {
        let ok = !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(format!("unzulässiger Pfad »{rel}«"));
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            return Err(format!("»{rel}« ist unter Windows ein reservierter Name"));
        }
    }
    Ok(())
}

/// Prüft die Dateien eines Eintrags allgemein: Pfade, Größe, Text, und dass
/// keine zwei Pfade sich nur in der Groß-/Kleinschreibung unterscheiden (unter
/// Windows wäre das dieselbe Datei).
fn check_files(files: &Files) -> Result<(), String> {
    if files.is_empty() {
        return Err("enthält keine Dateien".to_string());
    }
    if files.len() > MAX_FILES {
        return Err(format!("mehr als {MAX_FILES} Dateien"));
    }
    let mut seen = BTreeSet::new();
    for (rel, text) in files {
        check_path(rel)?;
        if text.len() > MAX_FILE_BYTES {
            return Err(format!("»{rel}« ist größer als {MAX_FILE_BYTES} Bytes"));
        }
        if text.contains('\0') {
            return Err(format!("»{rel}« ist keine Textdatei"));
        }
        if !seen.insert(rel.to_lowercase()) {
            return Err(format!("»{rel}« kommt in anderer Schreibweise doppelt vor"));
        }
    }
    // Ein Pfad darf nicht zugleich Datei und Verzeichnis sein (`a` und `a/b`).
    for rel in files.keys() {
        for (i, _) in rel.match_indices('/') {
            if files.contains_key(&rel[..i]) {
                return Err(format!(
                    "»{}« ist Datei und Verzeichnis zugleich",
                    &rel[..i]
                ));
            }
        }
    }
    Ok(())
}

/// Prüft einen Skill: gültiger Name, `SKILL.md` vorhanden, ein
/// Frontmatter-`name` (falls gesetzt) gleich dem Ordnernamen — so verlangt es
/// der Agent-Skills-Standard, und nur so findet `remove NAME` ihn wieder.
fn check_skill(name: &str, files: &Files) -> Result<(), String> {
    let skill_md = files
        .get("SKILL.md")
        .ok_or_else(|| "SKILL.md fehlt".to_string())?;
    let fm = agentkit::parse_frontmatter(skill_md);
    if let Some((_, fm_name)) = fm.iter().find(|(k, _)| k == "name") {
        if fm_name != name {
            return Err(format!(
                "der Frontmatter-Name »{fm_name}« weicht vom Ordnernamen »{name}« ab"
            ));
        }
    }
    Ok(())
}

/// Prüft einen Plugin-Befehl (`commands/NAME.md`): nur Einstellungen aus
/// [`COMMAND_KEYS`], `graph`/`ctx` nur als `false`. Läuft bei der Installation
/// UND beim Laden — eine von Hand geänderte Datei darf nicht mehr dürfen als
/// eine installierte.
pub fn check_plugin_command(text: &str) -> Result<(), String> {
    for (key, value) in agentkit::parse_frontmatter(text) {
        if COMMAND_KEYS.contains(&key.as_str()) {
            continue;
        }
        if COMMAND_OFF_KEYS.contains(&key.as_str()) {
            if value == "false" {
                continue;
            }
            return Err(format!(
                "»{key}« darf ein Plugin-Befehl nur abschalten (»{key}: false«)"
            ));
        }
        return Err(format!(
            "die Einstellung »{key}« ist in einem Plugin-Befehl nicht erlaubt — sie hebt \
             Freigaben auf, startet Code, erweitert die Sandbox oder liest Dateien außerhalb \
             des Plugins"
        ));
    }
    Ok(())
}

/// Prüft das Manifest: ein JSON-Objekt nur mit [`MANIFEST_KEYS`], `name` gleich
/// dem Eintrag.
fn check_manifest(name: &str, text: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("plugin.json ist kein JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "plugin.json ist kein Objekt".to_string())?;
    for key in obj.keys() {
        if matches!(key.as_str(), "hooks" | "mcpServers" | "mcp_servers") {
            return Err(format!(
                "plugin.json deklariert »{key}« — Plugins dürfen keinen ausführbaren Code \
                 mitbringen (Hooks und MCP-Server laufen ohne Freigabe)"
            ));
        }
        if !MANIFEST_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "plugin.json: unbekannter Schlüssel »{key}« (erlaubt: {})",
                MANIFEST_KEYS.join(", ")
            ));
        }
    }
    match obj.get("name").and_then(|v| v.as_str()) {
        Some(n) if n == name => Ok(()),
        Some(n) => Err(format!("plugin.json nennt »{n}«, erwartet »{name}«")),
        None => Err("plugin.json: »name« fehlt".to_string()),
    }
}

/// Prüft ein Plugin: was auf oberster Ebene liegen darf, was in `commands/`,
/// `agents/` und `skills/`, und das Manifest. Unbekanntes wird ABGELEHNT, nicht
/// übergangen — ein Plugin, das Hooks mitbringt, soll nicht als „installiert"
/// gelten, während die Hälfte fehlt.
fn check_plugin(name: &str, files: &Files) -> Result<(), String> {
    let mut skills: BTreeMap<&str, Files> = BTreeMap::new();
    for (rel, text) in files {
        let parts: Vec<&str> = rel.split('/').collect();
        match (parts[0], parts.len()) {
            ("plugin.json" | "README.md", 1) => {}
            ("commands" | "agents", 2) if parts[1].ends_with(".md") => {
                let stem = parts[1].trim_end_matches(".md");
                if !valid_name(stem) {
                    return Err(format!("»{rel}«: ungültiger Name »{stem}«"));
                }
                if parts[0] == "commands" {
                    check_plugin_command(text).map_err(|e| format!("»{rel}«: {e}"))?;
                }
            }
            ("commands" | "agents", _) => {
                return Err(format!(
                    "»{rel}«: in {}/ liegen nur .md-Dateien, keine Unterordner",
                    parts[0]
                ))
            }
            ("skills", n) if n >= 3 => {
                skills
                    .entry(parts[1])
                    .or_default()
                    .insert(parts[2..].join("/"), text.clone());
            }
            _ => return Err(format!("»{rel}«: unbekannter Bestandteil {ERLAUBT}")),
        }
    }
    let manifest = files
        .get("plugin.json")
        .ok_or_else(|| "plugin.json fehlt".to_string())?;
    check_manifest(name, manifest)?;
    for (skill, skill_files) in &skills {
        if !valid_name(skill) {
            return Err(format!("skills/{skill}: ungültiger Name"));
        }
        check_skill(skill, skill_files).map_err(|e| format!("skills/{skill}: {e}"))?;
    }
    Ok(())
}

/// Was auf oberster Ebene eines Plugins liegen darf — für Fehlermeldungen.
const ERLAUBT: &str = "(erlaubt: plugin.json, README.md, skills/, commands/, agents/)";

/// Was ein Plugin nicht mitbringen darf, mit Begründung — vor der allgemeinen
/// Pfadprüfung, die `.mcp.json` sonst nur als „unzulässigen Pfad" meldete.
fn forbidden_plugin_part(rel: &str) -> Option<String> {
    let top = rel.split('/').next().unwrap_or("");
    let grund = match top {
        "hooks" | "hooks.json" => "Hooks führen Code ohne Freigabe aus",
        ".mcp.json" | "mcp.json" => "MCP-Server werden ohne Freigabe gestartet",
        ".claude-plugin" => {
            "das ist ein Claude-Code-Plugin; dessen Befehle sind Nutzer-Prompts, agentkits \
             Befehle System-Prompts — es würde falsch geladen"
        }
        _ => return None,
    };
    Some(format!(
        "»{rel}«: {grund}. Plugins enthalten nur Text {ERLAUBT}"
    ))
}

/// Prüft einen Eintrag vollständig. Der einzige Weg, auf dem etwas installiert
/// oder gepackt wird.
pub fn validate(kind: Kind, name: &str, files: &Files) -> Result<(), String> {
    let fehler = |e: String| format!("{} »{name}«: {e}", kind.label());
    if !valid_name(name) {
        return Err(fehler("ungültiger Name (erlaubt: a-z, 0-9, -)".to_string()));
    }
    if kind == Kind::Plugins {
        if let Some(e) = files.keys().find_map(|rel| forbidden_plugin_part(rel)) {
            return Err(fehler(e));
        }
    }
    check_files(files).map_err(fehler)?;
    match kind {
        Kind::Skills => check_skill(name, files),
        Kind::Plugins => check_plugin(name, files),
    }
    .map_err(fehler)
}

/// Liest eine Paketdatei.
/// Liest eine Paketdatei der Art `kind` — ein Plugin-Paket ist kein
/// Skill-Paket und umgekehrt.
pub fn parse_pack(text: &str, kind: Kind) -> Result<Items, String> {
    let (found, items) = parse_any_pack(text)?;
    if found != kind {
        return Err(format!(
            "das Paket enthält {}, keine {}",
            found.dir(),
            kind.dir()
        ));
    }
    Ok(items)
}

fn parse_any_pack(text: &str) -> Result<(Kind, Items), String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("kein gültiges Paket (JSON): {e}"))?;
    if value.get("format").and_then(|v| v.as_str()) != Some(FORMAT) {
        return Err(format!(
            "kein agentkit-Paket (»format« ist nicht »{FORMAT}«)"
        ));
    }
    if value.get("version").and_then(|v| v.as_u64()) != Some(FORMAT_VERSION) {
        return Err(format!(
            "Paketversion nicht unterstützt (erwartet {FORMAT_VERSION}) — agentkit aktualisieren"
        ));
    }
    let kind = match value.get("kind").and_then(|v| v.as_str()) {
        Some("skills") => Kind::Skills,
        Some("plugins") => Kind::Plugins,
        other => return Err(format!("unbekannte Paketart {other:?}")),
    };
    let items: Items = serde_json::from_value(value.get("items").cloned().unwrap_or_default())
        .map_err(|e| format!("»items« nicht lesbar: {e}"))?;
    Ok((kind, items))
}

/// Schreibt eine Paketdatei. Jeder Eintrag wird geprüft — ein Paket, das sich
/// nicht installieren ließe, entsteht gar nicht erst.
pub fn render_pack(kind: Kind, items: &Items) -> Result<String, String> {
    for (name, files) in items {
        validate(kind, name, files)?;
    }
    let value = serde_json::json!({
        "format": FORMAT,
        "version": FORMAT_VERSION,
        "kind": kind.dir(),
        "items": items,
    });
    Ok(serde_json::to_string_pretty(&value).expect("JSON aus Strings"))
}

// --------------------------------------------- Adapter: lokales Dateisystem

/// Liest eine lokale Quelle: eine Paketdatei (`.json`), einen einzelnen
/// Eintrag (Ordner mit `SKILL.md` bzw. `plugin.json`) oder einen Ordner voller
/// Einträge. Symlinks werden abgelehnt — sie könnten auf alles zeigen.
pub fn read_local(kind: Kind, path: &Path) -> Result<Items, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{}: Symlinks werden nicht installiert",
            path.display()
        ));
    }
    if meta.is_file() {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        return parse_pack(&text, kind).map_err(|e| format!("{}: {e}", path.display()));
    }
    if let Some(name) = item_name(kind, path)? {
        return Ok(Items::from([(name, read_tree(path)?)]));
    }
    let mut items = Items::new();
    for entry in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(name) = item_name(kind, &entry.path())? {
                items.insert(name, read_tree(&entry.path())?);
            }
        }
    }
    if items.is_empty() {
        return Err(format!(
            "{}: kein {} gefunden (erwartet: Ordner mit {})",
            path.display(),
            kind.label(),
            kind.marker()
        ));
    }
    Ok(items)
}

/// Ist `dir` selbst ein Eintrag, und wie heißt er? Ein Skill heißt wie sein
/// Ordner, ein Plugin wie in seinem Manifest — die Quelle eines Plugins darf
/// anders heißen (`examples/dokumente/plugin`).
fn item_name(kind: Kind, dir: &Path) -> Result<Option<String>, String> {
    let marker = dir.join(kind.marker());
    if !marker.is_file() {
        return Ok(None);
    }
    match kind {
        // Kanonisch, damit auch `.` und `..` einen Ordnernamen haben.
        Kind::Skills => Ok(dir
            .canonicalize()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))),
        Kind::Plugins => {
            let manifest = marker;
            let text = std::fs::read_to_string(&manifest)
                .map_err(|e| format!("{}: {e}", manifest.display()))?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("{}: kein JSON: {e}", manifest.display()))?;
            value
                .get("name")
                .and_then(|v| v.as_str())
                .map(|n| Some(n.to_string()))
                .ok_or_else(|| format!("{}: »name« fehlt", manifest.display()))
        }
    }
}

/// Liest alle Dateien unter `dir` als Text. Ein Symlink oder eine Datei, die
/// kein UTF-8 ist, ist ein Fehler — nicht still übergangen, sonst fehlte nach
/// der Installation etwas, ohne dass es jemand merkt.
fn read_tree(dir: &Path) -> Result<Files, String> {
    let mut files = Files::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((current, prefix)) = stack.pop() {
        let entries =
            std::fs::read_dir(&current).map_err(|e| format!("{}: {e}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(format!("»{rel}«: Symlinks werden nicht installiert"));
            }
            if kind.is_dir() {
                stack.push((entry.path(), rel));
                continue;
            }
            if files.len() >= MAX_FILES {
                return Err(format!("{}: mehr als {MAX_FILES} Dateien", dir.display()));
            }
            let bytes = std::fs::read(entry.path()).map_err(|e| format!("»{rel}«: {e}"))?;
            let text =
                String::from_utf8(bytes).map_err(|_| format!("»{rel}« ist keine Textdatei"))?;
            files.insert(rel, text);
        }
    }
    Ok(files)
}

/// Installiert einen geprüften Eintrag nach `<root>/<kind>/<name>/`. Ein
/// vorhandener gleichen Namens wird ersetzt (ein ausgeschaltetes Plugin
/// ebenso — die Installation schaltet es ein).
///
/// Geschrieben wird erst in eine Zwischenablage unter `root`; dann wird der
/// alte Stand beiseite gelegt, der neue eingesetzt und erst danach der alte
/// gelöscht. Scheitert das Einsetzen, kommt der alte Stand zurück. Dateien
/// entstehen mit den Standardrechten, also nie ausführbar.
pub fn install(root: &Path, kind: Kind, name: &str, files: &Files) -> Result<PathBuf, String> {
    validate(kind, name, files)?;
    let io = |e: std::io::Error| e.to_string();
    let staging_root = root.join(STAGING_DIR);
    // Reste früherer, abgebrochener Installationen desselben Eintrags (andere
    // PID) gleich mit wegräumen — sonst blieben sie für immer liegen.
    let prefix = format!("{}-{name}-", kind.dir());
    for entry in dir_entries(&staging_root) {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            remove_entry(&entry.path()).map_err(io)?;
        }
    }
    let staging = staging_root.join(format!("{prefix}{}", std::process::id()));
    let written = files.iter().try_for_each(|(rel, text)| {
        let path = rel.split('/').fold(staging.clone(), |p, part| p.join(part));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)
    });
    if let Err(e) = written {
        remove_entry(&staging).ok();
        return Err(e.to_string());
    }
    let base = root.join(kind.dir());
    std::fs::create_dir_all(&base).map_err(io)?;
    let target = base.join(name);
    let old = staging_root.join(format!("{prefix}{}-alt", std::process::id()));
    let had_old = std::fs::symlink_metadata(&target).is_ok();
    if had_old {
        std::fs::rename(&target, &old).map_err(io)?;
    }
    if let Err(e) = std::fs::rename(&staging, &target) {
        if had_old {
            std::fs::rename(&old, &target).ok();
        }
        remove_entry(&staging).ok();
        return Err(e.to_string());
    }
    remove_entry(&old).map_err(io)?;
    if kind == Kind::Plugins {
        remove_entry(&root.join(DISABLED_DIR).join(name)).map_err(io)?;
    }
    std::fs::remove_dir(&staging_root).ok();
    Ok(target)
}

/// Entfernt einen installierten Eintrag, falls vorhanden. Ein Symlink wird
/// als Link entfernt, sein Ziel bleibt unberührt.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

/// Entfernt einen Skill bzw. ein Plugin (auch ein ausgeschaltetes).
pub fn remove(root: &Path, kind: Kind, name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("ungültiger Name »{name}«"));
    }
    let mut paths = vec![root.join(kind.dir()).join(name)];
    if kind == Kind::Plugins {
        paths.push(root.join(DISABLED_DIR).join(name));
    }
    let present: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
    if present.is_empty() {
        return Err(format!("{} »{name}« ist nicht installiert", kind.label()));
    }
    for p in present {
        remove_entry(p).map_err(|e| e.to_string())?;
    }
    Ok(format!("{} »{name}« entfernt.", kind.label()))
}

/// Schaltet ein Plugin ein oder aus, indem es zwischen `plugins/` und
/// `plugins-disabled/` verschoben wird.
pub fn set_enabled(root: &Path, name: &str, on: bool) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("ungültiger Name »{name}«"));
    }
    let (from, to) = if on {
        (DISABLED_DIR, Kind::Plugins.dir())
    } else {
        (Kind::Plugins.dir(), DISABLED_DIR)
    };
    let state = if on { "eingeschaltet" } else { "ausgeschaltet" };
    let src = root.join(from).join(name);
    let dst = root.join(to).join(name);
    if !src.is_dir() {
        return if dst.is_dir() {
            Ok(format!("Plugin »{name}« ist bereits {state}."))
        } else {
            Err(format!("Plugin »{name}« ist nicht installiert"))
        };
    }
    std::fs::create_dir_all(root.join(to)).map_err(|e| e.to_string())?;
    std::fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    Ok(format!("Plugin »{name}« {state}."))
}

/// Die Einträge eines Verzeichnisses; ein fehlendes ist leer.
fn dir_entries(dir: &Path) -> impl Iterator<Item = std::fs::DirEntry> {
    std::fs::read_dir(dir).into_iter().flatten().flatten()
}

/// Die Namen der Ordner in `dir` mit gültigem Namen, sortiert.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = dir_entries(dir)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| valid_name(n))
        .collect();
    names.sort();
    names
}

/// Stämme der `.md`-Dateien in `dir`, sortiert.
fn entries_md(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = dir_entries(dir)
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".md").map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// Die eingeschalteten Plugins: Ordner unter `plugins/` mit `plugin.json`.
fn enabled_plugins(root: &Path) -> Vec<PathBuf> {
    let base = root.join(Kind::Plugins.dir());
    entries(&base)
        .into_iter()
        .map(|n| base.join(n))
        .filter(|p| p.join(Kind::Plugins.marker()).is_file())
        .collect()
}

/// Die Skill-Verzeichnisse, die ohne `--skills` mitgeladen werden:
/// `<root>/skills`, dann `skills/` jedes eingeschalteten Plugins.
///
/// Nur Verzeichnisse, die wirklich einen Skill enthalten: schon ein leeres
/// `skills/` (nach dem letzten `remove`) hängte jedem Lauf den Skills-Absatz
/// an den System-Prompt.
pub fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    std::iter::once(root.join(Kind::Skills.dir()))
        .chain(enabled_plugins(root).into_iter().map(|p| p.join("skills")))
        .filter(|d| dir_entries(d).any(|e| e.path().join(Kind::Skills.marker()).is_file()))
        .collect()
}

/// Die Rollen-Verzeichnisse eingeschalteter Plugins (`agents/`), sofern sie
/// eine Rolle enthalten.
pub fn role_dirs(root: &Path) -> Vec<PathBuf> {
    enabled_plugins(root)
        .into_iter()
        .map(|p| p.join("agents"))
        .filter(|d| !entries_md(d).is_empty())
        .collect()
}

/// Die `--skills`- und `--agents`-Werte samt allem Installierten — die eine
/// Stelle, die beide Binaries fragen.
///
/// - Skills: ausdrückliche Verzeichnisse VORN, denn bei gleichem Namen
///   gewinnt in `agentkit::Skills` das zuerst genannte.
/// - Rollen: ausdrückliche HINTEN, denn beim Mischen überschreibt das später
///   geladene (`agentkit::merge_roles`). Unter `--agents-only` („genau diese
///   Rollen") nur die ausdrücklichen.
pub fn installed_dirs(
    skills: Option<&str>,
    agents: Option<&str>,
    agents_only: bool,
    root: Option<&Path>,
) -> (Option<String>, Option<String>) {
    let explicit = |s: Option<&str>| {
        s.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let paths = |dirs: Vec<PathBuf>| -> Vec<String> {
        dirs.iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    };
    let skill_parts = explicit(skills)
        .into_iter()
        .chain(paths(root.map(skill_dirs).unwrap_or_default()));
    let installed_roles = if agents_only {
        Vec::new()
    } else {
        paths(root.map(role_dirs).unwrap_or_default())
    };
    let agent_parts = installed_roles.into_iter().chain(explicit(agents));
    (join_dirs(skill_parts), join_dirs(agent_parts))
}

fn join_dirs(parts: impl Iterator<Item = String>) -> Option<String> {
    let parts: Vec<String> = parts.collect();
    (!parts.is_empty()).then(|| parts.join(";"))
}

/// Ein Befehl aus einem eingeschalteten Plugin: Pfad und geprüfter Text.
/// `None`, wenn kein Plugin ihn hat; bei mehreren gewinnt das alphabetisch
/// erste. `Err`, wenn die Datei Einstellungen setzt, die ein Plugin nicht darf.
pub fn plugin_command(root: &Path, name: &str) -> Option<Result<(PathBuf, String), String>> {
    if !valid_name(name) {
        return None;
    }
    let path = enabled_plugins(root)
        .into_iter()
        .map(|p| p.join("commands").join(format!("{name}.md")))
        .find(|p| p.is_file())?;
    Some(
        std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|text| {
                check_plugin_command(&text)
                    .map(|_| (path.clone(), text))
                    .map_err(|e| format!("{}: {e}", path.display()))
            }),
    )
}

/// Die Namen aller Befehle eingeschalteter Plugins.
pub fn plugin_commands(root: &Path) -> Vec<String> {
    enabled_plugins(root)
        .iter()
        .flat_map(|p| entries_md(&p.join("commands")))
        .collect()
}

/// Eine Zeile je installiertem Eintrag.
pub fn list(root: &Path, kind: Kind) -> String {
    let lines: Vec<String> = match kind {
        Kind::Skills => {
            let dir = root.join(kind.dir());
            agentkit::Skills::new(&dir.to_string_lossy())
                .index()
                .into_iter()
                .map(|s| {
                    format!("{}  {}", s.name, first_sentence(&s.description))
                        .trim_end()
                        .to_string()
                })
                .collect()
        }
        Kind::Plugins => [(kind.dir(), "an"), (DISABLED_DIR, "aus")]
            .into_iter()
            .flat_map(|(sub, state)| {
                let base = root.join(sub);
                entries(&base)
                    .into_iter()
                    .map(move |name| plugin_line(&base.join(&name), &name, state))
            })
            .collect(),
    };
    if lines.is_empty() {
        format!(
            "Keine {} installiert ({}).",
            kind.dir(),
            root.join(kind.dir()).display()
        )
    } else {
        lines.join("\n")
    }
}

/// Die Zeile eines Plugins für `list`: Name, Version, Zustand, Beschreibung,
/// darunter seine Bestandteile.
fn plugin_line(dir: &Path, name: &str, state: &str) -> String {
    let manifest: serde_json::Value = std::fs::read_to_string(dir.join(Kind::Plugins.marker()))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let field = |k: &str| manifest.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let inhalt: Vec<String> = [
        ("Befehle", entries_md(&dir.join("commands"))),
        ("Skills", entries(&dir.join("skills"))),
        ("Rollen", entries_md(&dir.join("agents"))),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(k, v)| format!("{k}: {}", v.join(", ")))
    .collect();
    format!(
        "{name} {} [{state}]  {}\n    {}",
        field("version"),
        first_sentence(field("description")),
        inhalt.join(" · ")
    )
}

fn first_sentence(text: &str) -> String {
    let s = text.split(". ").next().unwrap_or(text).trim();
    if s.chars().count() > 100 {
        format!("{}…", s.chars().take(100).collect::<String>())
    } else {
        s.to_string()
    }
}

// ------------------------------------------------------------- Kommandozeile

/// Die URL des Release-Pakets — aus [`crate::upgrade::REPO`] und dem festen
/// Asset-Namen gebaut, nie aus einer Eingabe.
pub fn release_url(kind: Kind) -> String {
    format!(
        "https://github.com/{}/releases/latest/download/{}",
        crate::upgrade::REPO,
        kind.asset()
    )
}

/// Die Hilfe zu `agentkit skills` bzw. `agentkit plugins`.
pub fn help(kind: Kind) -> String {
    let verb = kind.dir();
    let extra = if kind == Kind::Plugins {
        "\n  enable NAME            Plugin einschalten\n  disable NAME           Plugin ausschalten (bleibt installiert)"
    } else {
        ""
    };
    format!(
        "agentkit {verb} — installierbare {verb} verwalten\n\n\
         Nutzung: agentkit {verb} <unterkommando>\n\n  \
         list                   installierte {verb} anzeigen\n  \
         install NAME…          aus dem agentkit-Release installieren ({asset})\n  \
         install --all          alle {verb} des Release-Pakets installieren\n  \
         install PFAD           aus einem lokalen Ordner oder einer Paketdatei (.json)\n  \
         remove NAME            entfernen{extra}\n  \
         pack DIR…              Paketdatei aus Ordnern bauen (auf stdout)\n\n\
         Installiert wird nach {ort}. Ein vorhandener Pfad gilt als lokale Quelle\n\
         (im Zweifel ./NAME schreiben), sonst wird NAME aus dem Release geladen.\n",
        asset = kind.asset(),
        ort = format_args!("<config_dir>/{verb}/<name>/"),
    )
}

/// Führt `agentkit skills|plugins …` aus. `root` ist das Config-Verzeichnis,
/// `net` der Download — `None`, wenn das Binary ohne HTTP gebaut ist; dann
/// gehen nur lokale Quellen. Liefert, was auf stdout gehört.
pub fn dispatch(
    kind: Kind,
    argv: &[String],
    root: Option<&Path>,
    net: Option<&dyn Netz>,
) -> Result<String, String> {
    let sub = argv.first().map(String::as_str).unwrap_or("help");
    let rest = &argv[argv.len().min(1)..];
    if sub == "pack" {
        if rest.is_empty() {
            return Err(format!(
                "agentkit {} pack DIR… — mindestens ein Ordner",
                kind.dir()
            ));
        }
        let mut items = Items::new();
        for dir in rest {
            for (name, files) in read_local(kind, Path::new(dir))? {
                if items.insert(name.clone(), files).is_some() {
                    return Err(format!("{} »{name}« kommt doppelt vor", kind.label()));
                }
            }
        }
        return render_pack(kind, &items);
    }
    if matches!(sub, "help" | "-h" | "--help") {
        return Ok(help(kind));
    }
    let root =
        root.ok_or("kein Config-Verzeichnis (weder AGENTKIT_HOME noch HOME/USERPROFILE gesetzt)")?;
    let name_arg = || -> Result<&str, String> {
        match rest {
            [name] => Ok(name.as_str()),
            _ => Err(format!("agentkit {} {sub} NAME", kind.dir())),
        }
    };
    match sub {
        "list" => Ok(list(root, kind)),
        "remove" => remove(root, kind, name_arg()?),
        "enable" | "disable" if kind == Kind::Plugins => {
            set_enabled(root, name_arg()?, sub == "enable")
        }
        "install" => install_cmd(kind, rest, root, net),
        other => Err(format!(
            "unbekanntes Unterkommando »{other}« — agentkit {} help",
            kind.dir()
        )),
    }
}

/// `install`: lokale Quelle, wenn es den Pfad gibt, sonst Namen aus dem Release.
fn install_cmd(
    kind: Kind,
    rest: &[String],
    root: &Path,
    net: Option<&dyn Netz>,
) -> Result<String, String> {
    let all = rest.iter().any(|a| a == "--all");
    let args: Vec<&String> = rest.iter().filter(|a| *a != "--all").collect();
    let (items, wanted): (Items, Vec<String>) = match args.as_slice() {
        [path] if Path::new(path.as_str()).exists() && !all => {
            let items = read_local(kind, Path::new(path.as_str()))?;
            let names = items.keys().cloned().collect();
            (items, names)
        }
        [] if !all => return Err(format!("agentkit {} install NAME|PFAD|--all", kind.dir())),
        names => {
            if let Some(bad) = names.iter().find(|n| !valid_name(n)) {
                return Err(format!(
                    "»{bad}« ist weder ein vorhandener Pfad noch ein gültiger Name"
                ));
            }
            let net = net.ok_or(
                "dieses Binary kann nichts herunterladen (gebaut ohne Feature »upgrade«) — \
                 installiere aus einem lokalen Pfad",
            )?;
            let url = release_url(kind);
            let bytes = net.bytes(&url)?;
            let text = String::from_utf8(bytes).map_err(|_| format!("{url}: kein Text"))?;
            let items = parse_pack(&text, kind).map_err(|e| format!("{url}: {e}"))?;
            let wanted: Vec<String> = if all {
                items.keys().cloned().collect()
            } else {
                names.iter().map(|n| n.to_string()).collect()
            };
            if let Some(missing) = wanted.iter().find(|n| !items.contains_key(*n)) {
                return Err(format!(
                    "{} »{missing}« gibt es im Release nicht (vorhanden: {})",
                    kind.label(),
                    items.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            (items, wanted)
        }
    };
    // Erst alles prüfen, dann schreiben: ein ungültiger Eintrag soll nicht
    // die Hälfte eines Pakets installiert zurücklassen.
    for name in &wanted {
        validate(kind, name, &items[name])?;
    }
    let mut out = Vec::new();
    for name in &wanted {
        let target = install(root, kind, name, &items[name])?;
        out.push(format!(
            "{} »{name}« installiert: {}",
            kind.label(),
            target.display()
        ));
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> Files {
        pairs
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect()
    }

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentkit_pakete_{name}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SKILL: &str =
        "---\nname: review\ndescription: Prüft einen Diff. Gründlich.\n---\n\nAnleitung.\n";

    fn plugin() -> Files {
        files(&[
            (
                "plugin.json",
                r#"{"name": "doku", "version": "1.0.0", "description": "Dokumente."}"#,
            ),
            (
                "commands/doku.md",
                "---\nstrategy: react\ngraph: false\n---\n\nDu bist …\n",
            ),
            (
                "agents/leser.md",
                "---\nname: leser\ntools: Read\n---\n\nLies.\n",
            ),
            (
                "skills/auswerten/SKILL.md",
                "---\nname: auswerten\n---\n\nSchritte.\n",
            ),
        ])
    }

    #[test]
    fn namen_sind_schlicht() {
        assert!(valid_name("dokument-auswerten"));
        for bad in ["", "-x", "A", "a/b", "a..b", "a.b", "../x", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn pfade_koennen_nicht_ausbrechen() {
        for bad in [
            "../x",
            "a/../b",
            "/etc/passwd",
            "C:/x",
            "a\\b",
            ".mcp.json",
            "a/.git/config",
            "a/",
            "",
            "con.md",
            "skills/LPT1",
            "a./b",
            "a b",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
        assert!(check_path("skills/x/scripts/run_1.py").is_ok());
    }

    #[test]
    fn ein_skill_braucht_skill_md_und_passenden_namen() {
        assert!(validate(Kind::Skills, "review", &files(&[("SKILL.md", SKILL)])).is_ok());
        let e = validate(Kind::Skills, "review", &files(&[("README.md", "x")])).unwrap_err();
        assert!(e.contains("SKILL.md fehlt"), "{e}");
        let e = validate(Kind::Skills, "anders", &files(&[("SKILL.md", SKILL)])).unwrap_err();
        assert!(e.contains("weicht"), "{e}");
        let e = validate(
            Kind::Skills,
            "review",
            &files(&[("SKILL.md", SKILL), ("bild.png", "a\0b")]),
        )
        .unwrap_err();
        assert!(e.contains("keine Textdatei"), "{e}");
        let e = validate(
            Kind::Skills,
            "review",
            &files(&[("SKILL.md", SKILL), ("skill.md", "x")]),
        )
        .unwrap_err();
        assert!(e.contains("doppelt"), "{e}");
    }

    #[test]
    fn ein_plugin_mit_hooks_mcp_oder_unbekanntem_wird_abgelehnt() {
        assert!(validate(Kind::Plugins, "doku", &plugin()).is_ok());
        for (extra, erwartet) in [
            ("hooks/pre.sh", "Hooks"),
            ("hooks.json", "Hooks"),
            (".mcp.json", "MCP"),
            (".claude-plugin/plugin.json", "Claude-Code-Plugin"),
            ("bin/tool", "unbekannter Bestandteil"),
            ("commands/x.sh", "nur .md"),
            ("commands/sub/x.md", "nur .md"),
            ("skills/leer/README.md", "SKILL.md fehlt"),
        ] {
            let mut f = plugin();
            f.insert(extra.to_string(), "x".to_string());
            let e = validate(Kind::Plugins, "doku", &f).unwrap_err();
            assert!(e.contains(erwartet), "{extra}: {e}");
        }
    }

    #[test]
    fn das_manifest_darf_keinen_code_deklarieren() {
        for (manifest, erwartet) in [
            (r#"{"name": "doku", "hooks": {}}"#, "ausführbaren Code"),
            (r#"{"name": "doku", "mcpServers": {}}"#, "ausführbaren Code"),
            (
                r#"{"name": "doku", "commands": "./x"}"#,
                "unbekannter Schlüssel",
            ),
            (r#"{"name": "andere"}"#, "erwartet »doku«"),
        ] {
            let mut f = plugin();
            f.insert("plugin.json".to_string(), manifest.to_string());
            let e = validate(Kind::Plugins, "doku", &f).unwrap_err();
            assert!(e.contains(erwartet), "{manifest}: {e}");
        }
    }

    #[test]
    fn ein_plugin_befehl_darf_keine_freigaben_aufheben() {
        assert!(check_plugin_command("---\nmax_steps: 50\ngraph: false\n---\nText").is_ok());
        for key in [
            "yes: true",
            "hooks: h.json",
            "mcp_config: m.json",
            "mcp: a",
            "allow_read: /",
            "workspace: /",
            "no_project_instructions: true",
            "system_file: /etc/passwd",
            "skills: /x",
            "graph: /tmp/g",
            "tools: read_file",
        ] {
            let text = format!("---\n{key}\n---\nText");
            assert!(check_plugin_command(&text).is_err(), "{key}");
        }
    }

    #[test]
    fn paket_rundreise_und_installation() {
        let items = Items::from([("review".to_string(), files(&[("SKILL.md", SKILL)]))]);
        let text = render_pack(Kind::Skills, &items).unwrap();
        let back = parse_pack(&text, Kind::Skills).unwrap();
        assert_eq!(back, items);
        let e = parse_pack(&text, Kind::Plugins).unwrap_err();
        assert!(e.contains("enthält skills"), "{e}");

        let root = tmp("rundreise");
        let target = install(&root, Kind::Skills, "review", &back["review"]).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            SKILL
        );
        assert!(
            !root.join(STAGING_DIR).exists(),
            "Zwischenablage bleibt nicht liegen"
        );
        assert!(list(&root, Kind::Skills).starts_with("review  Prüft einen Diff"));
        assert_eq!(skill_dirs(&root), vec![root.join("skills")]);

        // Neu installieren ersetzt — keine alten Dateien bleiben liegen, und
        // Reste einer abgebrochenen Installation (andere PID) werden weggeräumt.
        std::fs::write(target.join("alt.md"), "alt").unwrap();
        let rest = root.join(STAGING_DIR).join("skills-review-1");
        std::fs::create_dir_all(&rest).unwrap();
        install(&root, Kind::Skills, "review", &back["review"]).unwrap();
        assert!(!target.join("alt.md").exists());
        assert!(!root.join(STAGING_DIR).exists(), "Reste weggeräumt");

        remove(&root, Kind::Skills, "review").unwrap();
        assert!(!target.exists());
        assert!(remove(&root, Kind::Skills, "review").is_err());
        // Ein leeres skills/ zählt nicht als Skill-Verzeichnis — sonst bekäme
        // jeder Lauf den Skills-Absatz im System-Prompt.
        assert!(root.join("skills").is_dir());
        assert!(skill_dirs(&root).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ein_ungueltiger_eintrag_schreibt_nichts() {
        let root = tmp("ungueltig");
        let bad = files(&[("SKILL.md", SKILL), ("../../draussen.md", "x")]);
        assert!(install(&root, Kind::Skills, "review", &bad).is_err());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "nichts geschrieben"
        );
        assert!(!root.parent().unwrap().join("draussen.md").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn plugins_liefern_skills_rollen_und_befehle_nur_eingeschaltet() {
        let root = tmp("plugin");
        install(&root, Kind::Plugins, "doku", &plugin()).unwrap();
        let p = root.join("plugins/doku");
        assert_eq!(skill_dirs(&root), vec![p.join("skills")]);
        assert_eq!(role_dirs(&root), vec![p.join("agents")]);
        assert_eq!(plugin_commands(&root), ["doku"]);
        assert!(plugin_command(&root, "doku").unwrap().is_ok());
        assert!(plugin_command(&root, "fehlt").is_none());

        // Eine von Hand verschärfte Befehlsdatei wird beim Laden abgelehnt.
        std::fs::write(p.join("commands/doku.md"), "---\nyes: true\n---\nx").unwrap();
        assert!(plugin_command(&root, "doku").unwrap().is_err());

        set_enabled(&root, "doku", false).unwrap();
        assert!(skill_dirs(&root).is_empty() && role_dirs(&root).is_empty());
        assert!(plugin_command(&root, "doku").is_none());
        assert!(list(&root, Kind::Plugins).contains("doku 1.0.0 [aus]"));
        set_enabled(&root, "doku", true).unwrap();
        assert_eq!(role_dirs(&root).len(), 1);

        // Installieren schaltet ein ausgeschaltetes Plugin wieder ein.
        set_enabled(&root, "doku", false).unwrap();
        install(&root, Kind::Plugins, "doku", &plugin()).unwrap();
        assert!(!root.join(DISABLED_DIR).join("doku").exists());
        remove(&root, Kind::Plugins, "doku").unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ausdrueckliche_verzeichnisse_gewinnen() {
        let root = tmp("reihenfolge");
        install(&root, Kind::Plugins, "doku", &plugin()).unwrap();
        let (skills, agents) = installed_dirs(Some("/eigen"), Some("/eigen"), false, Some(&root));
        let (skills, agents) = (skills.unwrap(), agents.unwrap());
        assert!(skills.starts_with("/eigen;"), "{skills}");
        assert!(agents.ends_with(";/eigen"), "{agents}");
        // `--agents-only`: genau die ausdrücklichen Rollen.
        let (_, agents) = installed_dirs(None, Some("/eigen"), true, Some(&root));
        assert_eq!(agents.as_deref(), Some("/eigen"));
        assert_eq!(installed_dirs(None, None, false, None), (None, None));
        let leer = tmp("leer");
        assert_eq!(
            installed_dirs(None, Some(" "), false, Some(&leer)),
            (None, None)
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&leer).ok();
    }

    #[test]
    fn lokale_quelle_lehnt_symlinks_und_binaerdateien_ab() {
        let src = tmp("lokal");
        let skill = src.join("review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), SKILL).unwrap();
        let items = read_local(Kind::Skills, &src).unwrap();
        assert_eq!(items.keys().collect::<Vec<_>>(), ["review"]);
        assert_eq!(read_local(Kind::Skills, &skill).unwrap(), items);
        // `.` hat keinen Dateinamen — gemeint ist trotzdem der Ordner.
        assert_eq!(read_local(Kind::Skills, &skill.join(".")).unwrap(), items);

        std::fs::write(skill.join("bild.png"), [0xff, 0xfe, 0x00]).unwrap();
        let e = read_local(Kind::Skills, &skill).unwrap_err();
        assert!(e.contains("keine Textdatei"), "{e}");
        std::fs::remove_file(skill.join("bild.png")).unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", skill.join("geheim")).unwrap();
            let e = read_local(Kind::Skills, &skill).unwrap_err();
            assert!(e.contains("Symlinks"), "{e}");
        }
        std::fs::remove_dir_all(&src).ok();
    }

    /// Was das Release ausliefert, muss sich packen lassen — sonst fiele ein
    /// ungültiger Skill erst im Release-Workflow auf.
    #[test]
    fn die_mitgelieferten_pakete_sind_gueltig() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for (kind, dir) in [
            (Kind::Skills, "agentkit_app/skills"),
            (
                Kind::Plugins,
                "agent_framework_rs/examples/dokumente/plugin",
            ),
        ] {
            let items = read_local(kind, &repo.join(dir)).unwrap();
            render_pack(kind, &items).unwrap_or_else(|e| panic!("{dir}: {e}"));
        }
    }

    /// Download nur über die `Netz`-Naht, nur von der festen Release-URL.
    #[test]
    fn install_aus_dem_release_ueber_die_netz_naht() {
        struct FakeNetz(String, std::cell::RefCell<Vec<String>>);
        impl Netz for FakeNetz {
            fn text(&self, _: &str) -> Result<String, String> {
                unreachable!()
            }
            fn bytes(&self, url: &str) -> Result<Vec<u8>, String> {
                self.1.borrow_mut().push(url.to_string());
                Ok(self.0.clone().into_bytes())
            }
        }
        let items = Items::from([("review".to_string(), files(&[("SKILL.md", SKILL)]))]);
        let net = FakeNetz(
            render_pack(Kind::Skills, &items).unwrap(),
            Default::default(),
        );
        let root = tmp("release");
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let out = dispatch(
            Kind::Skills,
            &argv(&["install", "review"]),
            Some(&root),
            Some(&net),
        )
        .unwrap();
        assert!(out.contains("»review« installiert"), "{out}");
        assert_eq!(
            net.1.borrow().as_slice(),
            ["https://github.com/rudi77/agentkit_rs/releases/latest/download/agentkit-skills.json"]
        );
        let e = dispatch(
            Kind::Skills,
            &argv(&["install", "fehlt"]),
            Some(&root),
            Some(&net),
        )
        .unwrap_err();
        assert!(e.contains("gibt es im Release nicht"), "{e}");
        let e = dispatch(
            Kind::Skills,
            &argv(&["install", "Böse/x"]),
            Some(&root),
            Some(&net),
        )
        .unwrap_err();
        assert!(e.contains("weder ein vorhandener Pfad"), "{e}");
        let e = dispatch(
            Kind::Skills,
            &argv(&["install", "review"]),
            Some(&root),
            None,
        )
        .unwrap_err();
        assert!(e.contains("lokalen Pfad"), "{e}");
        // Ein Plugin-Paket ist kein Skill-Paket.
        let e = dispatch(
            Kind::Plugins,
            &argv(&["install", "review"]),
            Some(&root),
            Some(&net),
        )
        .unwrap_err();
        assert!(e.contains("enthält skills"), "{e}");
        std::fs::remove_dir_all(&root).ok();
    }
}
