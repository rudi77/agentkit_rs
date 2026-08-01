//! Der Store: ein `RwLock<Arc<GraphIndex>>` plus ein serialisierender Schreiber.
//!
//! ```text
//! Leser A ─┐                      ┌─ Arc<GraphIndex> (Revision 41)
//! Leser B ─┼─ index.read().clone()┤   unveränderlich, so lange gehalten wie nötig
//! Leser C ─┘                      └─ …
//!
//! Schreiber ── writer.lock() ── neuer Index aus Ops ── Journal ── index.write() = Arc(42)
//! ```
//!
//! Ein Leser hält den Read-Lock nur für die Dauer eines `Arc`-Klons; die
//! eigentliche Traversierung läuft danach völlig sperrfrei auf einem
//! unveränderlichen Snapshot. Ein Schreiber blockiert damit **keinen** laufenden
//! Read, und ein langer Read blockiert keinen Schreiber.
//!
//! Der Commit ist synchron: erst das Journal (dauerhaft), dann der Tausch
//! (sichtbar). Scheitert das Journal, bleibt der alte Snapshot stehen — es gibt
//! keinen Zustand, der im Speicher gilt, aber nicht auf der Platte steht.

pub(crate) mod index;
mod journal;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub use index::{GraphIndex, GraphOp};

use crate::access::GraphAccess;
use crate::error::GraphError;
use crate::model::{now_ms, GraphRevision};
use crate::write::{apply, GraphReceipt, GraphWriteCommand, IdCounters};
use journal::Journal;

/// Dateiname des Journals innerhalb des Graph-Verzeichnisses.
pub const JOURNAL_FILE: &str = "graph.jsonl";

/// Ab wie vielen Journal-Zeilen je Datensatz beim Öffnen neu geschrieben wird.
/// Faktor 2 heißt: „im Schnitt wurde jeder Datensatz einmal geändert" — darüber
/// lohnt das Zusammenfalten, darunter ist es Verschwendung.
const REWRITE_FACTOR: u64 = 2;
const REWRITE_MIN_LINES: u64 = 256;

struct WriteState {
    journal: Option<Journal>,
    ids: IdCounters,
}

/// Zahlen für `/graph`-Anzeigen und Tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphStats {
    pub revision: GraphRevision,
    pub entities: usize,
    pub claims: usize,
    pub sources: usize,
    pub episodes: usize,
}

pub struct GraphStore {
    index: RwLock<Arc<GraphIndex>>,
    writer: Mutex<WriteState>,
    path: Option<PathBuf>,
}

/// Ops zu einem Index abspielen und die Revision setzen — der gemeinsame Kern
/// von [`GraphStore::open`] und [`GraphStore::open_read_only`]. Gemeinsam,
/// damit die beiden Pfade nicht auseinanderlaufen und der Betrachter etwas
/// anderes zeigt als die CLI.
fn replay(ops: Vec<GraphOp>) -> GraphIndex {
    let mut index = GraphIndex::default();
    let mut revision: GraphRevision = 0;
    for op in ops {
        revision = revision.max(op_revision(&op));
        index.apply(op);
    }
    index.set_revision(revision);
    index
}

impl GraphStore {
    /// Flüchtiger Graph ohne Journal — für Tests, Beispiele und Läufe, die
    /// nichts hinterlassen sollen.
    pub fn in_memory() -> Self {
        GraphStore {
            index: RwLock::new(Arc::new(GraphIndex::default())),
            writer: Mutex::new(WriteState {
                journal: None,
                ids: IdCounters::default(),
            }),
            path: None,
        }
    }

    /// Öffnet den Graphen in `dir` (legt das Verzeichnis an) und spielt
    /// `graph.jsonl` ab. Ein fehlendes Journal ist ein frischer Graph.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, GraphError> {
        let path = dir.as_ref().join(JOURNAL_FILE);
        let (mut journal, ops) = Journal::open(&path)?;

        let mut ids = IdCounters::default();
        for op in &ops {
            ids.observe(op);
        }
        let index = replay(ops);
        let revision = index.revision();

        // Zusammenfalten, wenn viele Änderungen an wenigen Datensätzen hängen.
        let records = index.record_count() as u64;
        if journal.lines() > REWRITE_MIN_LINES && journal.lines() > REWRITE_FACTOR * records.max(1)
        {
            journal.rewrite(&index.to_ops(), revision, now_ms())?;
        }

        Ok(GraphStore {
            index: RwLock::new(Arc::new(index)),
            writer: Mutex::new(WriteState {
                journal: Some(journal),
                ids,
            }),
            path: Some(path),
        })
    }

    /// Sperrfreier Lesepfad: spielt `graph.jsonl` zu einem [`GraphIndex`] ab,
    /// OHNE irgendeine Schreibwirkung — weder legt es das Verzeichnis an, noch
    /// kompaktiert es das Journal.
    ///
    /// Genau darin liegt der Unterschied zu [`GraphStore::open`]: das
    /// kompaktiert bei Bedarf (`REWRITE_MIN_LINES`/`REWRITE_FACTOR`) und
    /// SCHREIBT damit die Datei neu. Für einen Betrachter wäre das falsch — ein
    /// Leser darf die Datei eines lebenden Schreibers nicht anfassen. Dieselbe
    /// Begründung und dieselbe Machart wie `WorkStore::open_read_only`: es gibt
    /// bewusst keine `GraphStore`-Handle zurück, sondern nur den Stand, damit
    /// über diesen Weg strukturell nichts geschrieben werden kann.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<GraphIndex, GraphError> {
        Ok(replay(Journal::read_only(
            &dir.as_ref().join(JOURNAL_FILE),
        )?))
    }

    /// Pfad des Journals (`None` bei [`GraphStore::in_memory`]).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Ein konsistenter, unveränderlicher Stand. Genau ein kurzer Read-Lock —
    /// alles Weitere läuft sperrfrei auf dem zurückgegebenen `Arc`.
    pub fn snapshot(&self) -> Arc<GraphIndex> {
        self.index
            .read()
            .expect("Graph-Index-Lock nicht poisoned")
            .clone()
    }

    pub fn revision(&self) -> GraphRevision {
        self.snapshot().revision()
    }

    pub fn stats(&self) -> GraphStats {
        let index = self.snapshot();
        GraphStats {
            revision: index.revision(),
            entities: index.entity_count(),
            claims: index.claim_count(),
            sources: index.source_count(),
            episodes: index.episode_count(),
        }
    }

    /// Führt eine Mutation aus. Kehrt sie erfolgreich zurück, ist sie committet,
    /// im Journal und für **jeden** Leser sichtbar (Read-your-writes ohne
    /// Wartemechanik).
    pub fn submit(
        &self,
        command: GraphWriteCommand,
        access: &GraphAccess,
    ) -> Result<GraphReceipt, GraphError> {
        if let Some(target) = &access.write {
            // Wer schreibt, muss das Geschriebene auch sehen — sonst wäre die
            // eigene Beobachtung im nächsten Recall unsichtbar.
            if !access.view.sees(target.layer, &target.scope) {
                return Err(GraphError::Denied(format!(
                    "Schreibziel {target} liegt außerhalb der eigenen Sicht"
                )));
            }
        }

        let mut state = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        let current = self.snapshot();
        let revision = current.revision() + 1;

        // Auf einer Kopie der Zähler arbeiten: schlägt das Journal fehl, sind
        // keine IDs verbraucht.
        let mut ids = state.ids.clone();
        let (ops, receipt) = apply(&current, command, access, revision, &mut ids)?;
        if ops.is_empty() {
            return Ok(GraphReceipt {
                revision: current.revision(),
                ..receipt
            });
        }

        if let Some(journal) = state.journal.as_mut() {
            journal.append(revision, now_ms(), &ops)?;
        }
        state.ids = ids;

        let next = current.with_ops(&ops, revision);
        *self.index.write().expect("Graph-Index-Lock nicht poisoned") = Arc::new(next);
        Ok(receipt)
    }

    /// Schreibt das Journal aus dem aktuellen Stand neu (eine Zeile je Datensatz).
    /// Ohne Journal ein No-Op.
    pub fn compact_journal(&self) -> Result<(), GraphError> {
        let mut state = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        let index = self.snapshot();
        if let Some(journal) = state.journal.as_mut() {
            journal.rewrite(&index.to_ops(), index.revision(), now_ms())?;
        }
        Ok(())
    }

    /// Zeilen im Journal (Tests/Diagnose); `None` ohne Journal.
    pub fn journal_lines(&self) -> Option<u64> {
        let state = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        state.journal.as_ref().map(Journal::lines)
    }
}

impl std::fmt::Debug for GraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("GraphStore")
            .field("revision", &stats.revision)
            .field("entities", &stats.entities)
            .field("claims", &stats.claims)
            .field("path", &self.path)
            .finish()
    }
}

fn op_revision(op: &GraphOp) -> GraphRevision {
    match op {
        GraphOp::Entity(e) => e.updated_revision.max(e.created_revision),
        GraphOp::Claim(c) => c.updated_revision.max(c.created_revision),
        GraphOp::Source(s) => s.created_revision,
        GraphOp::Episode(e) => e.created_revision,
    }
}
