//! Fehler des Work-Crates.
//!
//! Bewusst ein kleines Enum statt einer Fehler-Hierarchie (Coding-Guidelines §4):
//! die Tool-Schicht (später `tools.rs`) wandelt jeden Fehler in ein weiches
//! `"ERROR: …"`-Ergebnis um, harte `Err` sieht nur der eingebettete Aufrufer.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkError {
    /// Journal-Datei nicht lesbar/schreibbar (Pfad + I/O-Meldung).
    Io(String),
    /// Eine Journal-Zeile ist kaputt oder hat eine unbekannte `schema_version`.
    Journal(String),
    /// Eingabe verletzt eine Invariante (leeres Feld, Attempt gehört nicht zum
    /// Item, Lease-Attempt passt nicht, unbekannte/zyklische Abhängigkeit, …).
    Invalid(String),
    /// Referenzierter Datensatz (Item, Lauf, Attempt, Lease) existiert nicht.
    NotFound(String),
    /// Ein Statusübergang verletzt `WorkItemStatus::can_transition_to` — ein
    /// Programmierfehler im Aufrufer, kein Modellfehler.
    Transition(String),
    /// Das Projektverzeichnis ist durch eine `work.lock`-Sperrdatei eines
    /// anderen, noch laufenden `WorkStore` belegt (Befund 1 der Handprobe).
    Locked(String),
}

impl fmt::Display for WorkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkError::Io(m) => write!(f, "Work-I/O: {m}"),
            WorkError::Journal(m) => write!(f, "Journal beschädigt: {m}"),
            WorkError::Invalid(m) => write!(f, "ungültig: {m}"),
            WorkError::NotFound(m) => write!(f, "nicht gefunden: {m}"),
            WorkError::Transition(m) => write!(f, "unzulässiger Statusübergang: {m}"),
            WorkError::Locked(m) => write!(f, "gesperrt: {m}"),
        }
    }
}

impl std::error::Error for WorkError {}
