//! Benannte Workspaces: ein Name für „dieser Ordner samt seinen
//! Zusatzverzeichnissen", gespeichert im Benutzerverzeichnis, damit eine neue
//! Sitzung denselben Arbeitsbereich wieder laden kann (`--ws NAME`).
//!
//! Ablage: `<config_dir>/workspaces/<name>.json`, eine Datei je Workspace —
//! benutzerweit statt im Projekt, weil der Name von überall aus gelten soll
//! und ein Workspace mehrere Projekte umfasst. Kein Python-Pendant.
//!
//! Hier liegt auch die Logik der Slash-Befehle `/add-dir` und `/workspace`:
//! REPL und TUI unterscheiden sich nur darin, wie sie die Zeilen zeigen.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::coding::CodingTools;

/// Ein gespeicherter Workspace. Alle Pfade sind absolut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub name: String,
    /// Der Workspace selbst (`-w`): dort lösen sich relative Pfade auf, dort
    /// startet `run_shell`.
    pub root: PathBuf,
    /// Die Zusatzverzeichnisse (`--add-dir`, `/add-dir`).
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
}

/// Pfad der Datei zu `name`. Der Name wird zum Dateinamen, deshalb nur
/// Buchstaben, Ziffern, `-`, `_` und `.` (nicht am Anfang) — ein `../x` darf
/// nicht aus dem Ablageordner herausführen.
fn datei(config_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let gueltig = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !gueltig {
        return Err(format!(
            "Ungültiger Workspace-Name '{name}' — erlaubt sind Buchstaben, Ziffern, '-', '_' und '.'."
        ));
    }
    Ok(config_dir.join("workspaces").join(format!("{name}.json")))
}

/// Speichert `ws` unter seinem Namen; ein gleichnamiger wird ersetzt.
pub fn save_workspace(config_dir: &Path, ws: &SavedWorkspace) -> Result<PathBuf, String> {
    let pfad = datei(config_dir, &ws.name)?;
    let text = serde_json::to_string_pretty(ws).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(pfad.parent().unwrap_or(config_dir)).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&pfad, &text).map_err(|e| e.to_string())?;
    Ok(pfad)
}

/// Lädt den Workspace `name`. `Err` nennt die vorhandenen Namen — wer sich
/// vertippt, soll nicht erst suchen müssen.
pub fn load_workspace(config_dir: &Path, name: &str) -> Result<SavedWorkspace, String> {
    let pfad = datei(config_dir, name)?;
    let Ok(text) = std::fs::read_to_string(&pfad) else {
        let namen: Vec<String> = list_workspaces(config_dir)
            .into_iter()
            .map(|w| w.name)
            .collect();
        return Err(if namen.is_empty() {
            format!("Workspace '{name}' gibt es nicht (noch keiner gespeichert — /workspace save NAME).")
        } else {
            format!(
                "Workspace '{name}' gibt es nicht. Vorhanden: {}",
                namen.join(", ")
            )
        });
    };
    let mut ws: SavedWorkspace =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", pfad.display()))?;
    // Der Dateiname ist der Name, unter dem geladen wurde — eine von Hand
    // kopierte Datei soll nicht unter zwei Namen auftreten.
    ws.name = name.to_string();
    Ok(ws)
}

/// Alle gespeicherten Workspaces, nach Namen sortiert. Unlesbare Dateien
/// werden übergangen.
pub fn list_workspaces(config_dir: &Path) -> Vec<SavedWorkspace> {
    let Ok(eintraege) = std::fs::read_dir(config_dir.join("workspaces")) else {
        return Vec::new();
    };
    let mut namen: Vec<String> = eintraege
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| f.strip_suffix(".json").map(str::to_string))
        .collect();
    namen.sort();
    namen
        .iter()
        .filter_map(|n| load_workspace(config_dir, n).ok())
        .collect()
}

/// Löscht den gespeicherten Workspace `name` — nur den Eintrag, nie die
/// Verzeichnisse, auf die er zeigt.
pub fn delete_workspace(config_dir: &Path, name: &str) -> Result<(), String> {
    let pfad = datei(config_dir, name)?;
    std::fs::remove_file(&pfad).map_err(|_| format!("Workspace '{name}' gibt es nicht."))
}

/// Eine Zeile je Workspace für Listen: Name, Wurzel, Zahl der Zusatzverzeichnisse.
pub fn workspace_zeile(ws: &SavedWorkspace) -> String {
    match ws.dirs.len() {
        0 => format!("{}  {}", ws.name, ws.root.display()),
        n => format!("{}  {}  (+{n} Verzeichnis(se))", ws.name, ws.root.display()),
    }
}

/// Speichert den Stand von `coding` (Workspace + Zusatzverzeichnisse) als `name`.
fn save_current(
    config_dir: Option<&Path>,
    coding: &CodingTools,
    name: &str,
) -> Result<PathBuf, String> {
    let config_dir = config_dir.ok_or("Kein Benutzerverzeichnis gefunden (USERPROFILE/HOME).")?;
    save_workspace(
        config_dir,
        &SavedWorkspace {
            name: name.to_string(),
            root: coding.workspace().to_path_buf(),
            dirs: coding.add_dirs(),
        },
    )
}

/// `/add-dir [pfad]` — ein weiteres Arbeitsverzeichnis freigeben (Lesen und
/// Schreiben); ohne Pfad die Liste. Die Rückgabe sind Zeilen für den Menschen.
///
/// Hat die Sitzung einen benannten Workspace (`name`), wird er sofort
/// nachgeführt: ein Name, hinter dem ein veralteter Stand liegt, wäre beim
/// nächsten Laden eine Überraschung.
pub fn add_dir_command(
    rest: &[&str],
    coding: &CodingTools,
    name: Option<&str>,
    config_dir: Option<&Path>,
) -> Result<Vec<String>, String> {
    if rest.is_empty() {
        let dirs = coding.add_dirs();
        if dirs.is_empty() {
            return Ok(vec![
                "Keine zusätzlichen Arbeitsverzeichnisse. Nutzung: /add-dir <pfad>".to_string(),
            ]);
        }
        return Ok(dirs.iter().map(|d| format!("  {}", d.display())).collect());
    }
    // Wieder zusammengesetzt: ein Pfad darf Leerzeichen enthalten.
    let dir = coding.add_dir(&rest.join(" "))?;
    let mut zeilen = vec![format!(
        "✓ {} gehört jetzt zum Arbeitsbereich (Lesen und Schreiben).",
        dir.display()
    )];
    match name {
        Some(name) => match save_current(config_dir, coding, name) {
            Ok(_) => zeilen.push(format!("  Workspace '{name}' aktualisiert.")),
            Err(e) => zeilen.push(format!("  Workspace '{name}' NICHT aktualisiert: {e}")),
        },
        None => zeilen
            .push("  Gilt für diese Sitzung — dauerhaft mit /workspace save NAME.".to_string()),
    }
    Ok(zeilen)
}

/// `/workspace` — den Arbeitsbereich zeigen, `save [NAME]` speichert ihn,
/// `list` zeigt die gespeicherten. `name` ist der Name dieser Sitzung; `save`
/// setzt ihn, damit spätere `/add-dir` denselben Eintrag nachführen.
///
/// Laden gibt es hier bewusst nicht: der Workspace-Ordner steht für die
/// laufende Sitzung fest (Sandbox, Sitzungsdatei, Gedächtnis hängen an ihm) —
/// geladen wird beim Start mit `--ws NAME`.
pub fn workspace_command(
    rest: &[&str],
    coding: &CodingTools,
    name: &mut Option<String>,
    config_dir: Option<&Path>,
) -> Result<Vec<String>, String> {
    match rest {
        [] => {
            let mut zeilen = vec![format!(
                "Workspace: {}",
                name.as_deref().unwrap_or("(unbenannt)")
            )];
            zeilen.push(format!("  {}", coding.workspace().display()));
            for dir in coding.add_dirs() {
                zeilen.push(format!("  + {}", dir.display()));
            }
            if name.is_none() {
                zeilen.push("Speichern: /workspace save NAME".to_string());
            }
            Ok(zeilen)
        }
        ["save"] | ["save", _] => {
            let neu = match (rest.get(1), name.as_deref()) {
                (Some(n), _) => n.to_string(),
                (None, Some(n)) => n.to_string(),
                (None, None) => return Err("Nutzung: /workspace save NAME".to_string()),
            };
            save_current(config_dir, coding, &neu)?;
            let zeile = format!("✓ Workspace '{neu}' gespeichert — laden mit: agentkit --ws {neu}");
            *name = Some(neu);
            Ok(vec![zeile])
        }
        ["list"] => {
            let alle = config_dir.map(list_workspaces).unwrap_or_default();
            if alle.is_empty() {
                return Ok(vec!["Noch kein Workspace gespeichert.".to_string()]);
            }
            Ok(alle
                .iter()
                .map(|w| format!("  {}", workspace_zeile(w)))
                .collect())
        }
        _ => Err("Nutzung: /workspace [save [NAME] | list]".to_string()),
    }
}
