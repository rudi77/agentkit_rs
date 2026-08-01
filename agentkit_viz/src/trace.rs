//! Den NDJSON-Trace lesen und nachwachsende Zeilen nachladen (tailen).
//!
//! Das Tailen ist Offset-basiert wie `agentkit work events --tail`: der Leser
//! merkt sich, bis zu welchem BYTE er vollständige Zeilen verarbeitet hat, und
//! liest beim nächsten Mal nur den Rest. Damit ist „live mitschauen" dasselbe
//! wie „nachträglich laden", nur öfter — es gibt keinen zweiten Codepfad und
//! deshalb auch keinen, der auseinanderlaufen könnte.
//!
//! **Ein Schreiber ist möglicherweise mittendrin.** Die letzte Zeile einer
//! gerade wachsenden Datei kann halb geschrieben sein. Deshalb wird der Offset
//! ausschließlich bis zum letzten `\n` vorgerückt: eine unvollständige Zeile
//! bleibt einfach liegen und wird beim nächsten Durchlauf vollständig gelesen.
//! Der Leser fasst die Datei NIE an (kein Reparieren, kein Anlegen) — dieselbe
//! Regel wie bei `WorkStore::open_read_only`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::{TraceEvent, TraceLine, SCHEMA_VERSION};

/// Dateiendung der Trace-Dateien (`agentkit::trace` legt `trace-<pid>-<zeit>.jsonl` an).
pub const TRACE_EXT: &str = "jsonl";

/// Namensanfang derselben Dateien.
///
/// Die Endung allein reicht NICHT, seit rekursiv gesucht wird: in einem
/// Benchmark-Ergebnisbaum liegen fremde `.jsonl` herum (die SWE-bench-
/// Predictions etwa heißen `preds.jsonl`), und die als Sitzung anzubieten
/// hieße, dem Nutzer eine Ansicht mit null Ereignissen unterzuschieben.
pub const TRACE_PREFIX: &str = "trace-";

/// Fehler beim Lesen eines Trace.
#[derive(Debug)]
pub enum TraceError {
    Io(String),
    /// Die Datei ist in einem Format geschrieben, das dieser Betrachter nicht
    /// kennt. Anders als eine kaputte Zeile ist das kein Rauschen, sondern ein
    /// harter Fehler — sonst zeigte der Betrachter stillschweigend Unsinn.
    Schema(String),
}

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceError::Io(m) => write!(f, "{m}"),
            TraceError::Schema(m) => write!(f, "{m}"),
        }
    }
}

/// Eine Trace-Datei im Trace-Verzeichnis (für `/api/runs`).
#[derive(Debug, Clone, Serialize)]
pub struct TraceFileInfo {
    pub name: String,
    pub bytes: u64,
    /// Änderungszeit als Unix-Millisekunden (0, wenn das Dateisystem sie nicht hergibt).
    pub modified_ms: u64,
}

/// Wie tief unter der Wurzel nach Trace-Dateien gesucht wird.
///
/// Der Grund für die Rekursion ist der Benchmark-Betrieb: dort schreibt jeder
/// Task seinen eigenen Trace, und zwar dorthin, wo seine übrigen Artefakte
/// liegen — bei Harbor unter `<benchmark>/<job>/<trial>/agent/trace/`, also
/// fünf Ebenen unter der Ergebniswurzel. Acht Ebenen lassen Luft, ohne dass ein
/// versehentlich auf ein Riesenverzeichnis gerichteter Betrachter die halbe
/// Platte durchwandert.
const MAX_TIEFE: u32 = 8;

/// Obergrenze für besuchte Verzeichnisse. Der Schutz ist die ANZAHL, nicht die
/// Tiefe: ein Ergebnisbaum mit 300 SWE-bench-Instanzen ist flach und trotzdem
/// groß. Wird sie erreicht, bricht die Suche ab und liefert, was sie hat —
/// lieber eine unvollständige Liste als ein Betrachter, der sekündlich hängt.
const MAX_VERZEICHNISSE: usize = 20_000;

/// Listet die Trace-Dateien unter einer Wurzel, jüngste zuerst.
///
/// Gesucht wird REKURSIV, und der `name` einer Datei ist ihr Pfad relativ zur
/// Wurzel (immer mit `/`, unabhängig vom Betriebssystem). Damit ist eine
/// „Sitzung" nicht mehr an ein einzelnes Verzeichnis gebunden: `agentkit viz
/// --trace <ergebniswurzel>` zeigt jeden Benchmark-Task als eigene Sitzung, und
/// der relative Pfad ist zugleich sein sprechender Name.
///
/// Ein fehlendes Verzeichnis ist kein Fehler, sondern eine leere Liste: der
/// Betrachter darf vor dem ersten Lauf gestartet werden.
pub fn list_traces(dir: &Path) -> Vec<TraceFileInfo> {
    let mut out: Vec<TraceFileInfo> = Vec::new();
    let mut offen = vec![(dir.to_path_buf(), 0u32)];
    let mut besucht = 0usize;
    while let Some((akt, tiefe)) = offen.pop() {
        besucht += 1;
        if besucht > MAX_VERZEICHNISSE {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&akt) else {
            continue;
        };
        for e in entries.flatten() {
            // `DirEntry::file_type` folgt Symlinks NICHT. Das ist hier genau
            // richtig: ein Link, der auf ein Elternverzeichnis zeigt, wäre
            // sonst eine Endlosschleife.
            let Ok(typ) = e.file_type() else { continue };
            let pfad = e.path();
            if typ.is_dir() {
                if tiefe < MAX_TIEFE {
                    offen.push((pfad, tiefe + 1));
                }
                continue;
            }
            if !ist_trace_datei(&pfad) {
                continue;
            }
            let Some(name) = sitzungsname(dir, &pfad) else {
                continue;
            };
            let meta = e.metadata().ok();
            out.push(TraceFileInfo {
                name,
                bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_ms: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then(a.name.cmp(&b.name)));
    out
}

/// Trägt die Datei den Namen, den [`agentkit::trace`](https://docs.rs/agentkit)
/// vergibt (`trace-<pid>-<zeit>.jsonl`)?
fn ist_trace_datei(pfad: &Path) -> bool {
    pfad.extension().is_some_and(|x| x == TRACE_EXT)
        && pfad
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(TRACE_PREFIX))
}

/// Der Name einer Sitzung: der Pfad RELATIV zur Wurzel, immer `/`-getrennt.
///
/// Die feste Schreibweise ist kein Schönheitsfehler — der Name geht als `run=`
/// zurück an den Server und muss deshalb auf beiden Seiten derselbe sein, egal
/// ob das Betriebssystem `\` benutzt.
fn sitzungsname(wurzel: &Path, datei: &Path) -> Option<String> {
    let rel = datei.strip_prefix(wurzel).ok()?;
    let mut teile: Vec<String> = Vec::new();
    for teil in rel.components() {
        match teil {
            std::path::Component::Normal(s) => teile.push(s.to_string_lossy().into_owned()),
            // Alles andere (`..`, Laufwerk, Wurzel) darf es nach einem
            // `strip_prefix` nicht geben — und wäre als Name unbrauchbar.
            _ => return None,
        }
    }
    (!teile.is_empty()).then(|| teile.join("/"))
}

/// Was ein [`TraceReader::read_new`] geliefert hat.
#[derive(Debug, Default)]
pub struct Nachschub {
    pub events: Vec<TraceEvent>,
    /// true ⇔ der Leser hat WIEDER BEI NULL angefangen, weil die Datei kürzer
    /// wurde als der bisherige Stand (gekürzt oder ersetzt). Dann sind die
    /// Ereignisse der GANZE Inhalt, nicht ein Nachschlag — wer sie an einen
    /// vorhandenen Zustand hängt, hätte sonst jedes davon doppelt.
    pub neu_begonnen: bool,
}

/// Liest eine Trace-Datei fortschreitend.
pub struct TraceReader {
    path: PathBuf,
    /// Bytes, bis zu denen VOLLSTÄNDIGE Zeilen gelesen wurden.
    offset: u64,
    /// Zeilen, die syntaktisch kaputt waren und übersprungen wurden. Wird
    /// gezählt und ausgewiesen, statt still verschluckt zu werden.
    skipped: u64,
}

impl TraceReader {
    pub fn open(path: impl Into<PathBuf>) -> TraceReader {
        TraceReader {
            path: path.into(),
            offset: 0,
            skipped: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Liest alle Zeilen, die seit dem letzten Aufruf VOLLSTÄNDIG dazugekommen
    /// sind. Eine fehlende Datei ist kein Fehler (leeres Ergebnis) — der
    /// Betrachter darf vor dem ersten Lauf laufen.
    pub fn read_new(&mut self) -> Result<Nachschub, TraceError> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Nachschub::default()),
            Err(e) => return Err(TraceError::Io(format!("{}: {e}", self.path.display()))),
        };
        let len = file
            .metadata()
            .map_err(|e| TraceError::Io(format!("{}: {e}", self.path.display())))?
            .len();
        // Kürzer als der Stand: die Datei wurde gekürzt oder ersetzt — von vorn
        // lesen. Der Aufrufer MUSS seinen bisherigen Zustand wegwerfen, sonst
        // steht danach alles doppelt darin (siehe `Nachschub::neu_begonnen`).
        let neu_begonnen = len < self.offset;
        if neu_begonnen {
            self.offset = 0;
            self.skipped = 0;
        }
        if len == self.offset {
            return Ok(Nachschub {
                events: Vec::new(),
                neu_begonnen,
            });
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|e| TraceError::Io(format!("{}: {e}", self.path.display())))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| TraceError::Io(format!("{}: {e}", self.path.display())))?;

        // Nur bis zum letzten Zeilenumbruch: alles danach ist eine Zeile, die
        // der Schreiber gerade erst anfängt (siehe Moduldoku).
        let Some(ende) = buf.iter().rposition(|b| *b == b'\n') else {
            return Ok(Nachschub {
                events: Vec::new(),
                neu_begonnen,
            });
        };
        let text = std::str::from_utf8(&buf[..=ende])
            .map_err(|e| TraceError::Io(format!("{}: kein UTF-8: {e}", self.path.display())))?;
        let mut out = Vec::new();
        // Erst am Ende übernehmen, gemeinsam mit dem Offset: bricht die
        // Schleife mit einem Schema-Fehler ab, bleibt der Stand unverändert —
        // sonst zählte derselbe Lesevorgang bei JEDER weiteren Anfrage erneut
        // dieselben übersprungenen Zeilen mit.
        let mut skipped = 0;
        for zeile in text.lines() {
            if zeile.trim().is_empty() {
                continue;
            }
            match parse_line(zeile) {
                Ok(line) => out.push(TraceEvent::from_line(line)),
                // Eine unbekannte schema_version ist ein harter Fehler: der
                // Betrachter würde sonst stillschweigend etwas anderes zeigen,
                // als in der Datei steht.
                Err(e @ TraceError::Schema(_)) => return Err(e),
                Err(_) => skipped += 1,
            }
        }
        self.skipped += skipped;
        self.offset += ende as u64 + 1;
        Ok(Nachschub {
            events: out,
            neu_begonnen,
        })
    }
}

fn parse_line(raw: &str) -> Result<TraceLine, TraceError> {
    let line: TraceLine = serde_json::from_str(raw)
        .map_err(|e| TraceError::Io(format!("Zeile nicht lesbar: {e}")))?;
    if line.schema_version != SCHEMA_VERSION {
        return Err(TraceError::Schema(format!(
            "Trace-Format '{}' unbekannt (dieser Betrachter liest '{SCHEMA_VERSION}')",
            line.schema_version
        )));
    }
    Ok(line)
}
