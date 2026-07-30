//! Das Journal: eine JSONL-Datei, eine Zeile pro Ereignis (oder ein Snapshot
//! zur Checkpoint-Markierung). Anders als agentkit_graph, dessen Journal je
//! Datensatz einen Op journalt, journalt agentkit-work das ganze [`WorkEvent`]
//! — die Domäne ist Fortschritt, keine Wissensbasis, und die Ereignisse selbst
//! sind der lesbare Audit-Trail (§14 im Originaldokument).
//!
//! ```json
//! {"schema_version":"1","seq":42,"at":1690000000000,"event":{"kind":"work_item_claimed", …}}
//! {"schema_version":"1","seq":43,"at":1690000000123,"snapshot":{ "...WorkState..." }}
//! ```
//!
//! Ein `WorkStore::checkpoint` HÄNGT eine Snapshot-Zeile an ([`Journal::append_snapshot`])
//! — die Historie bleibt vollständig erhalten, im Journal können deshalb
//! MEHRERE Snapshot-Zeilen stehen. Nur die LETZTE zählt beim Öffnen als Basis:
//! [`Journal::open`] überspringt alles davor (es steckt bereits in ihr) und
//! wendet nur die Ereignis-Zeilen NACH ihr über `apply` an — Wiederaufnahme
//! bleibt damit O(1) zur Anzahl der Ereignisse seit dem letzten Checkpoint,
//! nicht O(n) zur gesamten Journallänge. [`Journal::rewrite`] existiert
//! weiterhin, wird aber nur noch für die ECHTE Kompaktierung benutzt (siehe
//! dort) — sie kürzt das Journal wirklich, im Gegensatz zum Checkpoint.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::state::WorkState;

/// Version des Zeilenformats. Wird bei JEDER Zeile geschrieben und beim Lesen
/// geprüft: eine unbekannte Version ist ein harter Fehler, kein stilles Ignorieren.
pub const SCHEMA_VERSION: &str = "1";

/// Dateiname des Journals innerhalb des Projektverzeichnisses.
pub const JOURNAL_FILE: &str = "work.jsonl";

#[derive(Debug, Serialize, Deserialize)]
struct EventLine {
    schema_version: String,
    seq: u64,
    at: u64,
    event: WorkEvent,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotLine {
    schema_version: String,
    seq: u64,
    at: u64,
    snapshot: WorkState,
}

/// Eine geparste Zeile — entweder ein Ereignis oder ein Snapshot.
enum Line {
    Event(u64, WorkEvent),
    Snapshot(u64, WorkState),
}

pub(crate) struct Journal {
    path: PathBuf,
    /// Lazy: wird beim ersten Anhängen geöffnet und fürs Neuschreiben wieder
    /// geschlossen (unter Windows lässt sich sonst nicht darüber umbenennen).
    file: Option<File>,
    lines: u64,
}

impl Journal {
    /// Öffnet (oder legt an) das Journal und spielt es zu einem [`WorkState`]
    /// ab: die letzte Snapshot-Zeile als Basis, danach alle folgenden
    /// Ereignis-Zeilen. Eine fehlende Datei ist kein Fehler — frisches Projekt.
    ///
    /// Der dritte Rückgabewert ist die Anzahl der Ereignis-Zeilen, die
    /// tatsächlich über `apply` eingespielt wurden (also NICHT die
    /// Gesamtzeilenzahl) — reine Beobachtungs-Naht für Tests, die belegen
    /// wollen, dass das Öffnen nach einem Checkpoint nicht mehr die gesamte
    /// Journal-Historie erneut anwendet (siehe Moduldoku).
    pub fn open(path: &Path) -> Result<(Self, WorkState, u64), WorkError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| WorkError::Io(format!("{}: {e}", parent.display())))?;
            }
        }

        let mut state = WorkState::default();
        let mut lines = 0u64;
        let mut events_replayed = 0u64;
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| WorkError::Io(format!("{}: {e}", path.display())))?;
            let raw_lines: Vec<&str> = content.lines().collect();
            let mut parsed: Vec<Line> = Vec::with_capacity(raw_lines.len());
            let mut truncated_tail = false;
            for (idx, raw) in raw_lines.iter().enumerate() {
                if raw.trim().is_empty() {
                    continue;
                }
                match parse_line(raw) {
                    Ok(line) => parsed.push(line),
                    // Eine abgeschnittene LETZTE Zeile ist ein Absturz mitten im
                    // `append` (Prozess stirbt zwischen write und flush) — sie
                    // wird verworfen. Nur EIN kaputtes JSON-Dokument zählt als
                    // "abgeschnitten"; eine unbekannte schema_version bleibt
                    // IMMER ein harter Fehler, auch auf der letzten Zeile — sie
                    // ist syntaktisch vollständig, aber bewusst inkompatibel.
                    Err(LineError::Truncated(msg)) if idx == raw_lines.len() - 1 => {
                        eprintln!(
                            "agentkit-work: abgeschnittene letzte Journal-Zeile in {} wird verworfen: {msg}",
                            path.display()
                        );
                        truncated_tail = true;
                    }
                    Err(LineError::Truncated(msg)) | Err(LineError::Broken(msg)) => {
                        return Err(WorkError::Journal(format!(
                            "{}, Zeile {}: {msg}",
                            path.display(),
                            idx + 1
                        )));
                    }
                }
            }

            if truncated_tail {
                // "wird beim nächsten Schreiben überschrieben" heißt konkret: die
                // kaputten Bytes JETZT abschneiden. Ohne das würde ein späteres
                // `append` hinter den Datenmüll schreiben, statt ihn zu ersetzen
                // (Anhängen öffnet die Datei im Append-Modus, nicht am Ende der
                // letzten GÜLTIGEN Zeile).
                let mut fixed: String = raw_lines[..raw_lines.len() - 1].join("\n");
                if !fixed.is_empty() {
                    fixed.push('\n');
                }
                std::fs::write(path, fixed)
                    .map_err(|e| WorkError::Io(format!("{}: {e}", path.display())))?;
            }

            lines = parsed.len() as u64;

            // Nur AB der LETZTEN Snapshot-Zeile wiederherstellen: alles davor
            // steckt bereits in ihr (siehe Moduldoku) — erneutes `apply`
            // darauf wäre nicht nur verschwendete Arbeit, sondern liefe für
            // ein `WorkItemCreated`/`RunStarted`/… mit einer ID, die der
            // Snapshot schon enthält, direkt in die Duplikat-Ablehnung in
            // `state::apply`. Ohne diesen Sprung würde jeder von
            // `WorkStore::checkpoint` angehängte Snapshot (er ERSETZT das
            // Journal nicht mehr, siehe `append_snapshot`) das Öffnen mit der
            // Zeit linear zur GESAMTEN Journallänge verlangsamen statt — wie
            // gefordert — O(1) zu bleiben.
            let last_snapshot_idx = parsed
                .iter()
                .rposition(|line| matches!(line, Line::Snapshot(..)));
            for (idx, line) in parsed.into_iter().enumerate() {
                if last_snapshot_idx.is_some_and(|snap_idx| idx < snap_idx) {
                    continue;
                }
                match line {
                    Line::Snapshot(seq, snapshot) => {
                        state = snapshot;
                        state.seq = seq;
                    }
                    Line::Event(seq, event) => {
                        state.apply(&event)?;
                        state.seq = seq;
                        events_replayed += 1;
                    }
                }
            }
        }

        Ok((
            Journal {
                path: path.to_path_buf(),
                file: None,
                lines,
            },
            state,
            events_replayed,
        ))
    }

    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Hängt eine Ereignis-Zeile an und flusht. Erst wenn das geklappt hat,
    /// tauscht der Store den neuen Snapshot ein — ein fehlgeschlagener Commit
    /// hinterlässt keinen Zustand im Speicher (siehe `store/mod.rs::submit`).
    pub fn append(&mut self, seq: u64, at: u64, event: &WorkEvent) -> Result<(), WorkError> {
        let line = EventLine {
            schema_version: SCHEMA_VERSION.to_string(),
            seq,
            at,
            event: event.clone(),
        };
        let json = serde_json::to_string(&line)
            .map_err(|e| WorkError::Journal(format!("nicht serialisierbar: {e}")))?;
        self.append_raw(json)
    }

    /// Hängt eine Snapshot-Zeile an — der Checkpoint selbst
    /// (`WorkStore::checkpoint`). Gleiche Zeilenform wie [`Journal::rewrite`]
    /// (eine `SnapshotLine`), aber über den Append-Writer statt Temp-Datei
    /// plus Rename: die Historie bleibt vollständig erhalten, nur der
    /// Replay-Startpunkt für das nächste [`Journal::open`] wandert auf diese
    /// Zeile. Ein früherer Entwurf nutzte hier `rewrite` und zerstörte damit
    /// bei JEDEM Checkpoint die Zeitleiste, die `agentkit work events`
    /// anzeigen soll — genau das behebt dieser eigene, anhängende Pfad.
    pub fn append_snapshot(
        &mut self,
        seq: u64,
        at: u64,
        state: &WorkState,
    ) -> Result<(), WorkError> {
        let line = SnapshotLine {
            schema_version: SCHEMA_VERSION.to_string(),
            seq,
            at,
            snapshot: state.clone(),
        };
        let json = serde_json::to_string(&line)
            .map_err(|e| WorkError::Journal(format!("nicht serialisierbar: {e}")))?;
        self.append_raw(json)
    }

    /// Gemeinsamer Kern von [`Journal::append`] und [`Journal::append_snapshot`]
    /// — beide unterscheiden sich nur in der serialisierten Zeile, nicht im
    /// Schreibvorgang selbst.
    fn append_raw(&mut self, json: String) -> Result<(), WorkError> {
        let path = self.path.clone();
        let file = self.writer()?;
        writeln!(file, "{json}")
            .and_then(|()| file.flush())
            .map_err(|e| WorkError::Io(format!("{}: {e}", path.display())))?;
        self.lines += 1;
        Ok(())
    }

    /// Schreibt eine einzelne Snapshot-Zeile als NEUES Journal — ECHTE
    /// Kompaktierung, die die Historie davor bewusst aufgibt. Läuft über eine
    /// Temp-Datei plus Rename, damit ein Absturz mittendrin nicht das alte
    /// Journal zerstört.
    ///
    /// Seit der Korrektur des Checkpoint-Designs ist dies die EINZIGE Stelle
    /// im Crate, an der Journal-Historie verloren geht — und das nur noch an
    /// einer einzigen, bewussten Stelle: `WorkStore::open`, wenn die
    /// Journallänge weit genug vor die Anzahl der Datensätze gelaufen ist
    /// (`REWRITE_MIN_LINES`/`REWRITE_FACTOR`). Der Tausch ist Absicht:
    /// Auditierbarkeit (jede Zeile bleibt lesbar) gegen unbegrenztes
    /// Wachstum (ein jahrelang laufendes Projekt würde sein Journal sonst nie
    /// wieder verkleinern) — und dieser Tausch passiert NUR beim Öffnen und
    /// NUR oberhalb der Schwelle, nie bei einem normalen Checkpoint
    /// (`Journal::append_snapshot`), der die Historie unangetastet lässt.
    pub fn rewrite(&mut self, seq: u64, at: u64, state: &WorkState) -> Result<(), WorkError> {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| JOURNAL_FILE.to_string());
        // PID im Namen: zwei Prozesse auf derselben Datei sollen sich nicht die
        // Temp-Datei wegschreiben.
        let tmp = self
            .path
            .with_file_name(format!("{name}.tmp-{}", std::process::id()));
        {
            let mut file =
                File::create(&tmp).map_err(|e| WorkError::Io(format!("{}: {e}", tmp.display())))?;
            let line = SnapshotLine {
                schema_version: SCHEMA_VERSION.to_string(),
                seq,
                at,
                snapshot: state.clone(),
            };
            let json = serde_json::to_string(&line)
                .map_err(|e| WorkError::Journal(format!("nicht serialisierbar: {e}")))?;
            writeln!(file, "{json}")
                .and_then(|()| file.flush())
                .map_err(|e| WorkError::Io(format!("{}: {e}", tmp.display())))?;
        }
        // Altes Handle schließen, sonst schlägt das Umbenennen unter Windows fehl.
        self.file = None;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| WorkError::Io(format!("{}: {e}", self.path.display())))?;
        self.lines = 1;
        Ok(())
    }

    fn writer(&mut self) -> Result<&mut File, WorkError> {
        if self.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|e| WorkError::Io(format!("{}: {e}", self.path.display())))?;
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("gerade gesetzt"))
    }
}

/// Zwei Fehlerklassen, damit das Öffnen sie unterschiedlich behandeln kann:
/// nur ein kaputtes JSON-Dokument gilt als "abgeschnitten" (toleriert auf der
/// letzten Zeile), eine erkannte aber inkompatible Zeile nie.
enum LineError {
    /// Syntaktisch kein vollständiges JSON — das Muster eines Absturzes
    /// mitten im `write`.
    Truncated(String),
    /// Gültiges JSON, aber inhaltlich falsch (unbekannte `schema_version`,
    /// fehlendes Feld, kaputtes Ereignis).
    Broken(String),
}

/// Ein Zeilen-Objekt hat entweder ein `event`- oder ein `snapshot`-Feld — beide
/// tragen `schema_version`/`seq`/`at` gemeinsam, deshalb wird erst generisch in
/// `serde_json::Value` geparst und dann anhand des vorhandenen Feldes entschieden.
fn parse_line(raw: &str) -> Result<Line, LineError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| LineError::Truncated(format!("kein gültiges JSON: {e}")))?;
    let schema_version = value
        .get("schema_version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| LineError::Broken("Feld 'schema_version' fehlt".to_string()))?;
    if schema_version != SCHEMA_VERSION {
        return Err(LineError::Broken(format!(
            "schema_version '{schema_version}' unbekannt (erwartet '{SCHEMA_VERSION}')"
        )));
    }
    if value.get("snapshot").is_some() {
        let line: SnapshotLine =
            serde_json::from_value(value).map_err(|e| LineError::Broken(e.to_string()))?;
        Ok(Line::Snapshot(line.seq, line.snapshot))
    } else if value.get("event").is_some() {
        let line: EventLine =
            serde_json::from_value(value).map_err(|e| LineError::Broken(e.to_string()))?;
        Ok(Line::Event(line.seq, line.event))
    } else {
        Err(LineError::Broken(
            "weder 'event' noch 'snapshot' vorhanden".to_string(),
        ))
    }
}
