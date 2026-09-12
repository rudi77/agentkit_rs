//! Import-Pfad für Graph-Verzeichnisse aus der Zeit vor der OKF-Umstellung.
//!
//! Bis zu dieser Migration war `graph.jsonl` die einzige Persistenz: eine
//! JSONL-Datei, eine Zeile pro geschriebenem Datensatz, angehängt bei jedem
//! Commit. Seitdem legt [`crate::store::GraphStore`] sein Wissen als
//! OKF-Bundle ab (`crate::okf::bundle`) — dieses Modul wird NIE WIEDER
//! geschrieben, es liest nur noch ein vorhandenes Journal ein einziges Mal
//! ein, damit `GraphStore::open` es zu einem Bundle migrieren kann.
//!
//! ```json
//! {"schema_version":"1","revision":7,"at":1690000000000,"op":{"record":"claim", …}}
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::GraphError;
use crate::model::GraphRevision;
use crate::store::index::GraphOp;

/// Version des Zeilenformats. Wird beim Lesen geprüft: eine unbekannte
/// Version ist ein harter Fehler, kein stilles Ignorieren.
pub const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
struct JournalLine {
    schema_version: String,
    // `revision`/`at` sind Teil des Zeilenformats, werden beim Import aber
    // nicht gebraucht (die Revision kommt aus dem Datensatz selbst nach).
    #[allow(dead_code)]
    revision: GraphRevision,
    #[allow(dead_code)]
    at: u64,
    op: GraphOp,
}

/// Reiner Namensraum für den Lesepfad — kein Zustand, kein offenes Handle mehr:
/// ein Legacy-Journal wird genau einmal (bei der Migration) komplett
/// eingelesen, nie mehr angehängt oder neu geschrieben.
pub(crate) struct Journal;

impl Journal {
    /// Liest ein Journal vollständig ein; eine kaputte Zeile ist ein harter
    /// Fehler. Das ist der Pfad für [`crate::store::GraphStore::open`]: dort
    /// ist niemand sonst am Werk, eine kaputte letzte Zeile wäre ein
    /// tatsächliches Datenproblem.
    pub(crate) fn read_all(path: &Path) -> Result<Vec<GraphOp>, GraphError> {
        Self::read(path, false)
    }

    /// Sperrfreier Lesepfad: toleriert eine abgeschnittene LETZTE Zeile statt
    /// sie zu melden. Ein Leser trifft einen Schreiber mitten im Anhängen —
    /// ohne diese Nachsicht würde eine Anzeige, die im Sekundentakt liest,
    /// sporadisch mit „Zeile 731: EOF while parsing" abbrechen. Dieselbe
    /// Unterscheidung wie in `agentkit_work/src/store/journal.rs`.
    pub(crate) fn read_only(path: &Path) -> Result<Vec<GraphOp>, GraphError> {
        Self::read(path, true)
    }

    /// Gemeinsamer Kern von [`Self::read_all`] und [`Self::read_only`].
    /// `tolerate_tail` verwirft eine unvollständige letzte Zeile, statt sie
    /// zu melden (siehe [`Self::read_only`]).
    fn read(path: &Path, tolerate_tail: bool) -> Result<Vec<GraphOp>, GraphError> {
        let mut ops = Vec::new();
        if !path.exists() {
            return Ok(ops);
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
        let letzte = content.lines().count().saturating_sub(1);
        for (nr, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: JournalLine = match serde_json::from_str(line) {
                Ok(parsed) => parsed,
                // Nur die LETZTE Zeile, nur im Lesepfad: das ist das Muster
                // eines Schreibers, der gerade mittendrin ist.
                Err(_) if tolerate_tail && nr == letzte => break,
                Err(e) => {
                    return Err(GraphError::Journal(format!(
                        "{}, Zeile {}: {e}",
                        path.display(),
                        nr + 1
                    )))
                }
            };
            if parsed.schema_version != SCHEMA_VERSION {
                return Err(GraphError::Journal(format!(
                    "{}, Zeile {}: schema_version '{}' unbekannt (erwartet '{}')",
                    path.display(),
                    nr + 1,
                    parsed.schema_version,
                    SCHEMA_VERSION
                )));
            }
            ops.push(parsed.op);
        }
        Ok(ops)
    }
}
