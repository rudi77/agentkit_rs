//! Projektbezogene Sitzungsdateien — damit `--session` nicht von Hand
//! verwaltet werden muss.
//!
//! Jede interaktive Sitzung schreibt ihren Verlauf automatisch unter
//! `<config_dir>/sessions/<projekt>/<zeitstempel>.json`. `--continue` setzt die
//! jüngste Sitzung dieses Projekts fort, `--resume`/`/sessions` zeigen die
//! Liste. Der Inhalt ist **exakt** das Format von
//! [`crate::ShortTermMemory::save`] — eine automatisch angelegte Datei lässt
//! sich also genauso mit `--session <datei>` laden wie eine selbst benannte,
//! und `/export --json` schreibt dasselbe.
//!
//! Bewusst **keine** Metadaten-Datei: Datum, Titel und Zug-Zahl werden aus der
//! Datei selbst abgeleitet (mtime + erste User-Nachricht). Ein zweiter,
//! parallel zu pflegender Index wäre eine weitere Sache, die kaputtgehen kann.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::config_dir;
use crate::memory::{content, one_line, ShortTermMemory};

/// Eine gefundene Sitzung mit dem, was zur Auswahl nötig ist.
pub struct SessionInfo {
    pub path: PathBuf,
    /// Zeitpunkt der letzten Änderung (für „vor 5 Min").
    pub modified: SystemTime,
    /// Anzahl der Züge (User-Nachrichten).
    pub turns: usize,
    /// Erste User-Nachricht, gekürzt — als Wiedererkennungshilfe.
    pub title: String,
}

/// Verzeichnis der Sitzungen dieses Projekts. Der Name trägt den letzten
/// Pfadteil (lesbar) plus einen Hash des vollen Pfades (eindeutig) — zwei
/// gleichnamige Projektordner an verschiedenen Orten bleiben so getrennt.
fn project_dir(workspace: &str) -> Option<PathBuf> {
    let voll = Path::new(workspace)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace));
    let name = voll
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "projekt".to_string());
    let sicher: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let hash = fnv1a(&voll.to_string_lossy());
    Some(
        config_dir()?
            .join("sessions")
            .join(format!("{sicher}-{hash:08x}")),
    )
}

/// Pfad für eine neue Sitzung (`<projekt>/<UTC-Zeitstempel>.json`). Legt das
/// Verzeichnis an. Der Zeitstempel ist für Menschen da, die den Ordner
/// durchsehen oder eine Datei an `--session` weiterreichen — die Auflistung
/// selbst sortiert nach mtime, nicht nach Namen.
pub fn new_session_path(workspace: &str) -> Option<PathBuf> {
    let dir = project_dir(workspace)?;
    std::fs::create_dir_all(&dir).ok()?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(dir.join(format!("{}.json", zeitstempel(secs))))
}

/// Alle Sitzungen dieses Projekts, **neueste zuerst**.
pub fn list_sessions(workspace: &str) -> Vec<SessionInfo> {
    let Some(dir) = project_dir(workspace) else {
        return Vec::new();
    };
    let Ok(eintraege) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionInfo> = eintraege
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| session_info(&p))
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Die zuletzt geänderte Sitzung dieses Projekts (`--continue`).
///
/// Geht bewusst NICHT über [`list_sessions`]: dort würde jede Datei des
/// Projekts gelesen und geparst, nur um einen Pfad zurückzugeben — bei jedem
/// `-c`-Start und mit jedem Lauf länger. Hier reicht die mtime.
pub fn latest_session(workspace: &str) -> Option<PathBuf> {
    std::fs::read_dir(project_dir(workspace)?)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// Liest Titel und Zug-Zahl aus einer Sitzungsdatei. `None`, wenn sie nicht
/// lesbar oder kein Verlauf ist — eine kaputte Datei soll die Liste nicht
/// sprengen, sondern schlicht fehlen.
///
/// Bewusst über [`ShortTermMemory::load`] und `turn_starts()` statt eigenem
/// Parsen: so ist das „gleiches Format wie `--session`" aus der Modul-Doku
/// vom Compiler getragen und nicht bloß behauptet.
fn session_info(path: &Path) -> Option<SessionInfo> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let mem = ShortTermMemory::load(path.to_str()?).ok()?;
    let starts = mem.turn_starts();
    Some(SessionInfo {
        path: path.to_path_buf(),
        modified,
        turns: starts.len(),
        title: starts
            .first()
            .map(|i| one_line(content(&mem.messages[*i]), 60))
            .unwrap_or_else(|| "(leer)".to_string()),
    })
}

/// „vor 5 Min" / „vor 2 Std" / „vor 3 Tagen" — grob genug zum Wiedererkennen.
pub fn relatives_alter(t: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "gerade eben".to_string();
    };
    let s = d.as_secs();
    match s {
        0..=59 => "gerade eben".to_string(),
        60..=3599 => format!("vor {} Min", s / 60),
        3600..=86399 => format!("vor {} Std", s / 3600),
        _ => format!("vor {} Tagen", s / 86400),
    }
}

/// `JJJJMMTT-HHMMSS` in UTC aus Unix-Sekunden. Eigene Rechnung statt einer
/// Datums-Abhängigkeit: es geht nur um einen sortierbaren, lesbaren Dateinamen.
fn zeitstempel(secs: u64) -> String {
    let (tage, rest) = (secs / 86_400, secs % 86_400);
    let (h, min, s) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    let (j, m, t) = civil_from_days(tage as i64);
    format!("{j:04}{m:02}{t:02}-{h:02}{min:02}{s:02}")
}

/// Tage seit 1970-01-01 -> (Jahr, Monat, Tag). Howard Hinnants `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// FNV-1a (32 Bit) — deterministisch über Programm- und Rust-Versionen hinweg.
/// `DefaultHasher` wäre das nicht: dessen Ergebnis darf sich zwischen
/// Rust-Versionen ändern, und die Sitzungen eines Projekts wären nach einem
/// Toolchain-Update plötzlich unauffindbar.
///
/// (`agentkit_graph::model::content_hash` rechnet dasselbe in 64 Bit, aus
/// demselben Grund. Zwei Vorkommen sind noch keine Abstraktion wert — beim
/// dritten gehört ein gemeinsamer Helfer her.)
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeitstempel_ist_utc_und_sortierbar() {
        assert_eq!(zeitstempel(0), "19700101-000000");
        // 2024-02-29T12:34:56Z — Schaltjahr, damit die Datumsrechnung stimmt.
        assert_eq!(zeitstempel(1_709_210_096), "20240229-123456");
        // Sortierung als Text == Sortierung nach Zeit.
        assert!(zeitstempel(1_709_210_096) < zeitstempel(1_709_210_097));
    }

    #[test]
    fn fnv1a_ist_stabil_und_unterscheidet() {
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        assert_eq!(fnv1a("a"), 0xe40c_292c);
        assert_ne!(fnv1a("/a/projekt"), fnv1a("/b/projekt"));
    }

    #[test]
    fn relatives_alter_stuft_ab() {
        let vor = |s| SystemTime::now() - std::time::Duration::from_secs(s);
        assert_eq!(relatives_alter(vor(5)), "gerade eben");
        assert_eq!(relatives_alter(vor(300)), "vor 5 Min");
        assert_eq!(relatives_alter(vor(7200)), "vor 2 Std");
        assert_eq!(relatives_alter(vor(3 * 86_400)), "vor 3 Tagen");
    }

    /// Gleichnamige Projektordner an verschiedenen Orten dürfen sich die
    /// Sitzungen nicht teilen.
    #[test]
    fn project_dir_trennt_gleichnamige_projekte() {
        let a = project_dir("/tmp/eins/app").unwrap();
        let b = project_dir("/tmp/zwei/app").unwrap();
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("app-"));
    }
}
