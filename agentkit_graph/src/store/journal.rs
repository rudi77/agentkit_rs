//! Das Journal: eine JSONL-Datei, eine Zeile pro geschriebenem Datensatz.
//!
//! Es ist zugleich Persistenz und Audit-Trail. Beim Öffnen wird es abgespielt
//! (Upsert über die ID), beim Commit angehängt. Eine Zeile trägt immer ihre
//! `schema_version` — das ist die billigste Versicherung gegen ein
//! Migrationsproblem, das sonst erst auffällt, wenn schon Daten da sind.
//!
//! ```json
//! {"schema_version":"1","revision":7,"at":1690000000000,"op":{"record":"claim", …}}
//! ```

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::GraphError;
use crate::model::GraphRevision;
use crate::store::index::GraphOp;

/// Version des Zeilenformats. Wird bei JEDER Zeile geschrieben und beim Lesen
/// geprüft: eine unbekannte Version ist ein harter Fehler, kein stilles Ignorieren.
pub const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
struct JournalLine {
    schema_version: String,
    revision: GraphRevision,
    at: u64,
    op: GraphOp,
}

pub(crate) struct Journal {
    path: PathBuf,
    /// Lazy: wird beim ersten Anhängen geöffnet und fürs Neuschreiben wieder
    /// geschlossen (unter Windows lässt sich sonst nicht darüber umbenennen).
    file: Option<File>,
    lines: u64,
}

impl Journal {
    /// Öffnet (oder legt an) und spielt den vorhandenen Inhalt ab.
    /// Eine fehlende Datei ist KEIN Fehler — das ist ein frischer Graph.
    pub fn open(path: &Path) -> Result<(Self, Vec<GraphOp>), GraphError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| GraphError::Io(format!("{}: {e}", parent.display())))?;
            }
        }
        let (ops, lines) = Self::read(path, false)?;
        Ok((
            Journal {
                path: path.to_path_buf(),
                file: None,
                lines,
            },
            ops,
        ))
    }

    /// Sperrfreier Lesepfad: spielt das Journal ab, ohne irgendeine
    /// Schreibwirkung — es legt insbesondere das Verzeichnis nicht an. Gibt
    /// bewusst nur die Ops zurück, nicht die `Journal`-Handle: über diesen Weg
    /// soll strukturell nichts anhängen können.
    ///
    /// EIN Unterschied zu [`Self::open`], und der ist Absicht: eine
    /// abgeschnittene LETZTE Zeile wird toleriert statt als Fehler gemeldet.
    /// Ein Leser trifft einen Schreiber mitten im `append` — ohne diese
    /// Nachsicht würde eine Anzeige, die im Sekundentakt liest, sporadisch mit
    /// „Zeile 731: EOF while parsing" abbrechen und eine Sekunde später wieder
    /// funktionieren. Für den SCHREIBENDEN Pfad bleibt jede kaputte Zeile ein
    /// harter Fehler: dort ist niemand sonst am Werk. Dieselbe Unterscheidung
    /// wie in `agentkit_work/src/store/journal.rs`.
    pub fn read_only(path: &Path) -> Result<Vec<GraphOp>, GraphError> {
        Ok(Self::read(path, true)?.0)
    }

    /// Gemeinsamer Kern von [`Self::open`] und [`Self::read_only`] — reines
    /// Lesen. `tolerate_tail` verwirft eine unvollständige letzte Zeile,
    /// statt sie zu melden (siehe [`Self::read_only`]).
    fn read(path: &Path, tolerate_tail: bool) -> Result<(Vec<GraphOp>, u64), GraphError> {
        let mut ops = Vec::new();
        let mut lines = 0u64;
        if path.exists() {
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
                lines += 1;
            }
        }
        Ok((ops, lines))
    }

    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Hängt alle Ops einer Mutation an und flusht. Erst wenn das geklappt hat,
    /// tauscht der Store den neuen Snapshot ein — ein fehlgeschlagener Commit
    /// hinterlässt keinen Zustand im Speicher.
    pub fn append(
        &mut self,
        revision: GraphRevision,
        at: u64,
        ops: &[GraphOp],
    ) -> Result<(), GraphError> {
        let mut buffer = String::new();
        for op in ops {
            let line = JournalLine {
                schema_version: SCHEMA_VERSION.to_string(),
                revision,
                at,
                op: op.clone(),
            };
            let json = serde_json::to_string(&line)
                .map_err(|e| GraphError::Journal(format!("nicht serialisierbar: {e}")))?;
            buffer.push_str(&json);
            buffer.push('\n');
        }
        let path = self.path.clone();
        let file = self.writer()?;
        file.write_all(buffer.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
        self.lines += ops.len() as u64;
        Ok(())
    }

    /// Schreibt das Journal aus dem aktuellen Stand neu: aus N Änderungen an einem
    /// Datensatz wird wieder eine Zeile. Läuft über eine Temp-Datei plus Rename,
    /// damit ein Absturz mittendrin nicht das alte Journal zerstört.
    pub fn rewrite(
        &mut self,
        ops: &[GraphOp],
        revision: GraphRevision,
        at: u64,
    ) -> Result<(), GraphError> {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "graph.jsonl".to_string());
        // PID im Namen: zwei Prozesse auf derselben Datei sollen sich nicht die
        // Temp-Datei wegschreiben (derselbe Fehler wie einst in ctxmans Blob-Store).
        let tmp = self
            .path
            .with_file_name(format!("{name}.tmp-{}", std::process::id()));
        {
            let mut file = File::create(&tmp)
                .map_err(|e| GraphError::Io(format!("{}: {e}", tmp.display())))?;
            for op in ops {
                let line = JournalLine {
                    schema_version: SCHEMA_VERSION.to_string(),
                    revision,
                    at,
                    op: op.clone(),
                };
                let json = serde_json::to_string(&line)
                    .map_err(|e| GraphError::Journal(format!("nicht serialisierbar: {e}")))?;
                writeln!(file, "{json}")
                    .map_err(|e| GraphError::Io(format!("{}: {e}", tmp.display())))?;
            }
            file.flush()
                .map_err(|e| GraphError::Io(format!("{}: {e}", tmp.display())))?;
        }
        // Altes Handle schließen, sonst schlägt das Umbenennen unter Windows fehl.
        self.file = None;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| GraphError::Io(format!("{}: {e}", self.path.display())))?;
        self.lines = ops.len() as u64;
        Ok(())
    }

    fn writer(&mut self) -> Result<&mut File, GraphError> {
        if self.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|e| GraphError::Io(format!("{}: {e}", self.path.display())))?;
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("gerade gesetzt"))
    }
}
