//! Trace — der Ereignisstrom eines Laufs als NDJSON-Datei.
//!
//! Der [`crate::EventBus`] ist schon der Kanal, auf dem alles zusammenläuft:
//! Haupt-Agent, Sub-Agenten (`task`) und Schwarm-Mitglieder publizieren in
//! denselben Bus, getaggt über `AgentEvent::source`. Es fehlte nur der *Sink*:
//! ein Schreiber, der diesen Strom haltbar macht, damit ein Lauf nachträglich
//! (und von einem zweiten Prozess aus live) betrachtet werden kann.
//!
//! Eine Zeile je Ereignis, dieselbe Form für ALLE Zeilen — auch für
//! Zusatz-Datensätze wie den Kontext-Schnappschuss, die als
//! [`crate::EventData::Structured`] geschrieben werden:
//!
//! ```json
//! {"schema_version":"1","seq":1,"at_ms":1690000000000,"task_id":-1,"source":"","etype":"tool_call","data":{"tool_call":{"name":"read_file","args":{}}}}
//! {"schema_version":"1","seq":2,"at_ms":1690000000123,"task_id":-1,"source":"","etype":"structured","data":{"structured":{"kind":"context_snapshot","payload":{}}}}
//! ```
//!
//! Zeilenform und Robustheit folgen `agentkit_work/src/store/journal.rs`:
//! `schema_version` in jeder Zeile, `write_all` + `flush` je Zeile, eine
//! abgeschnittene letzte Zeile ist das erwartbare Muster eines mitten im
//! Schreiben gestorbenen Prozesses und Sache des Lesers.
//!
//! **Sicherheit.** Ein Trace enthält alles, was der Agent gelesen und
//! geschrieben hat: Dateiinhalte, Shell-Ausgaben, Modellantworten. In einem
//! Repo mit `.env` also potenziell Secrets im Klartext. Deshalb entsteht er
//! NUR auf ausdrückliche Anforderung (`--trace DIR`), [`TraceWriter::create`]
//! legt neben der Datei eine `.gitignore` mit `*` an, und beim Anlegen warnt
//! [`TraceWriter::create`] auf stderr. Bewusst KEINE Redaktion: ein Filter, dem
//! man vertraut, ist gefährlicher als eine ehrliche Warnung.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::events::{AgentEvent, TEXT_DELTA};
use crate::memory::truncate;

/// Version des Zeilenformats — steht in jeder Zeile, damit ein Leser eine
/// unbekannte Version erkennen kann, statt sie stillschweigend falsch zu deuten.
pub const SCHEMA_VERSION: &str = "1";

/// Ab dieser Länge (Zeichen) wird ein einzelner Text im Trace gekürzt.
///
/// Ein Trace darf nicht größer werden als das Repo, das er beobachtet: ein
/// einziges `read_file` auf eine große Datei landet sonst vollständig als
/// Tool-Ergebnis in der Zeile — und gleich noch einmal im nächsten
/// Kontext-Schnappschuss. Gekürzt wird mit Vermerk der Originalgröße
/// ([`truncate`]), nie stillschweigend.
pub const MAX_TEXT_CHARS: usize = 8_000;

/// Eine Trace-Zeile. Alle Zeilen haben dieselbe Form — ein Zusatz-Datensatz ist
/// nichts anderes als ein Ereignis mit [`crate::EventData::Structured`].
#[derive(Serialize)]
struct TraceLine<'a> {
    schema_version: &'static str,
    seq: u64,
    at_ms: u64,
    task_id: i64,
    source: &'a str,
    /// Weggelassen, wenn leer — nur Tool-Ereignisse tragen sie, und eine Zeile
    /// je Schritt soll nicht um ein leeres Feld wachsen.
    #[serde(skip_serializing_if = "str::is_empty")]
    call_id: &'a str,
    etype: &'a str,
    data: Value,
}

/// Datei und Zähler unter EINEM Lock: so ist `seq` garantiert die Reihenfolge
/// der Zeilen in der Datei und nicht bloß die Reihenfolge der Aufrufe.
struct Inner {
    file: File,
    seq: u64,
}

/// Schreibt den Ereignisstrom eines Laufs als NDJSON.
///
/// `Send + Sync`, weil er am [`crate::EventBus`] hängt: `publish` kommt aus dem
/// Agent-Thread, aus Sub-Agenten-Threads und aus Schwarm-Actor-Threads.
pub struct TraceWriter {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl TraceWriter {
    /// Legt `dir` an, schreibt dort eine `.gitignore` mit `*` und öffnet eine
    /// neue Trace-Datei `trace-<pid>-<zeit>.jsonl`. Warnt auf stderr, was in der
    /// Datei landen wird (siehe Moduldoku).
    ///
    /// Bewusst je Lauf eine EIGENE Datei statt einer angehängten: zwei parallel
    /// laufende Agenten im selben Verzeichnis würden sich sonst die Sequenz
    /// überschreiben, und der Betrachter könnte Läufe nicht trennen.
    pub fn create(dir: &Path) -> std::io::Result<TraceWriter> {
        std::fs::create_dir_all(dir)?;
        // Dieselbe Idee wie beim Work-Journal: was hier liegt, gehört nie in ein
        // Repository. Ein bestehendes `.gitignore` wird nicht überschrieben.
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, "*\n")?;
        }
        let path = dir.join(format!("trace-{}-{}.jsonl", std::process::id(), now_ms()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        eprintln!(
            "[WARN] Trace aktiv: {} — die Datei enthält ALLES, was der Agent liest und \
             schreibt (Dateiinhalte, Shell-Ausgaben, Modellantworten), also möglicherweise \
             auch Geheimnisse. Sie wird nicht redigiert.",
            path.display()
        );
        Ok(TraceWriter {
            path,
            inner: Mutex::new(Inner { file, seq: 0 }),
        })
    }

    /// Pfad der geschriebenen Datei (für Meldungen und Tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hängt ein Ereignis als eine Zeile an und flusht.
    ///
    /// Fehler werden auf stderr gemeldet und dann verschluckt: der Trace ist ein
    /// Beobachter. Eine volle Platte darf einen laufenden Auftrag nicht abbrechen.
    ///
    /// Einzige Ausnahme im Strom: `text_delta` wird NICHT geschrieben. Es ist
    /// ein Ereignis PRO TOKEN, dessen Zeilen-Rahmen ein Vielfaches der Nutzlast
    /// wiegt — und derselbe Text steht ohnehin gleich zweimal im Trace (als
    /// `final` und im Kontext-Schnappschuss). Ein Trace darf nicht größer werden
    /// als das Repo, das er beobachtet; das ist dieselbe Überlegung wie bei
    /// [`MAX_TEXT_CHARS`], nur eine Ebene höher.
    pub fn write_event(&self, ev: &AgentEvent) {
        if ev.etype == TEXT_DELTA {
            return;
        }
        let mut data = match serde_json::to_value(&ev.data) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[WARN] Trace-Ereignis nicht serialisierbar: {e}");
                return;
            }
        };
        shorten(&mut data);
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let line = TraceLine {
            schema_version: SCHEMA_VERSION,
            seq: inner.seq,
            at_ms: now_ms(),
            task_id: ev.task_id,
            source: &ev.source,
            call_id: &ev.call_id,
            etype: ev.etype,
            data,
        };
        let json = match serde_json::to_string(&line) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("[WARN] Trace-Zeile nicht serialisierbar: {e}");
                return;
            }
        };
        if let Err(e) = writeln!(inner.file, "{json}").and_then(|()| inner.file.flush()) {
            eprintln!(
                "[WARN] Trace nicht schreibbar ({}): {e}",
                self.path.display()
            );
        }
    }
}

/// Kürzt jeden zu langen String im Ereignis rekursiv — mit Vermerk der
/// gekürzten Zeichenzahl ([`truncate`]).
///
/// Rekursiv über den serialisierten [`Value`] statt je `EventData`-Variante:
/// zu lange Texte stecken nicht nur im Tool-Ergebnis, sondern auch in
/// Tool-Argumenten, in der finalen Antwort und in den Nachrichten eines
/// Kontext-Schnappschusses. Eine Stelle statt fünf, und eine neue
/// `EventData`-Variante ist automatisch mit abgedeckt.
///
/// An Ort und Stelle statt neu gebaut: der Normalfall ist „nichts zu kürzen",
/// und der soll keine Kopie jedes Objekts und jeder Liste kosten. Die
/// Byte-Länge ist die Vorprüfung — sie ist nie kleiner als die Zeichenzahl, ein
/// kurzer String erspart sich damit den O(n)-Lauf über `chars()`.
fn shorten(value: &mut Value) {
    match value {
        Value::String(s) if s.len() > MAX_TEXT_CHARS && s.chars().count() > MAX_TEXT_CHARS => {
            *s = truncate(s, MAX_TEXT_CHARS);
        }
        Value::Array(items) => items.iter_mut().for_each(shorten),
        Value::Object(map) => map.iter_mut().for_each(|(_, v)| shorten(v)),
        _ => {}
    }
}

/// Aktuelle Unix-Zeit in Millisekunden.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
