//! Fehler des Graph-Crates.
//!
//! Bewusst ein kleines Enum statt einer Fehler-Hierarchie (Coding-Guidelines §4):
//! die Tool-Schicht wandelt jeden Fehler in ein weiches `"ERROR: …"`-Ergebnis um,
//! harte `Err` sieht nur der eingebettete Aufrufer.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// Journal-Datei nicht lesbar/schreibbar (Pfad + I/O-Meldung).
    Io(String),
    /// Eine Journal-Zeile ist kaputt oder hat eine unbekannte `schema_version`.
    Journal(String),
    /// Referenzierter Datensatz existiert nicht.
    NotFound(String),
    /// Der [`crate::GraphAccess`] erlaubt die Operation nicht (kein Schreib- bzw.
    /// kein Promotion-Ziel, oder Ziel-Scope außerhalb der erlaubten Sicht).
    Denied(String),
    /// Eingabe verletzt eine Invariante (leeres Prädikat, fehlende Quelle, …).
    Invalid(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Io(m) => write!(f, "Graph-I/O: {m}"),
            GraphError::Journal(m) => write!(f, "Journal beschädigt: {m}"),
            GraphError::NotFound(m) => write!(f, "nicht gefunden: {m}"),
            GraphError::Denied(m) => write!(f, "nicht erlaubt: {m}"),
            GraphError::Invalid(m) => write!(f, "ungültig: {m}"),
        }
    }
}

impl std::error::Error for GraphError {}
