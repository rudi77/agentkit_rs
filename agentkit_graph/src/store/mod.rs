//! Der Store: ein `RwLock<Arc<GraphIndex>>` plus ein serialisierender Schreiber.
//!
//! ```text
//! Leser A ─┐                      ┌─ Arc<GraphIndex> (Revision 41)
//! Leser B ─┼─ index.read().clone()┤   unveränderlich, so lange gehalten wie nötig
//! Leser C ─┘                      └─ …
//!
//! Schreiber ── writer.lock() ── neuer Index aus Ops ── Bundle-Commit ── index.write() = Arc(42)
//! ```
//!
//! Ein Leser hält den Read-Lock nur für die Dauer eines `Arc`-Klons; die
//! eigentliche Traversierung läuft danach völlig sperrfrei auf einem
//! unveränderlichen Snapshot. Ein Schreiber blockiert damit **keinen** laufenden
//! Read, und ein langer Read blockiert keinen Schreiber.
//!
//! Der Commit ist synchron: erst das OKF-Bundle auf der Platte (dauerhaft),
//! dann der Tausch (sichtbar). Scheitert der Bundle-Commit, bleibt der alte
//! Snapshot stehen und keine ID ist verbraucht — es gibt keinen Zustand, der
//! im Speicher gilt, aber nicht auf der Platte steht.
//!
//! Mehrere Prozesse dürfen dasselbe Bundle offen haben. Geschrieben wird
//! unter einer Sperre je Commit, und davor gleicht der Schreiber seinen Stand
//! mit der Platte ab (siehe [`lock`]) — so vergeben zwei Prozesse nie dieselbe
//! ID. Fremde Commits werden beim nächsten EIGENEN Schreiben sichtbar; ein
//! reiner Leser behält seinen Stand, bis er neu öffnet.
//!
//! Persistiert wird als OKF-Bundle (`crate::okf::bundle`) — Markdown mit
//! YAML-Frontmatter, ein Dokument je Datensatz. Ein `graph.jsonl`-Journal aus
//! der Zeit davor wird beim ersten `open` einmalig migriert
//! (siehe [`legacy_journal`]).

pub(crate) mod index;
mod legacy_journal;
mod lock;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub use index::{GraphIndex, GraphOp};

use crate::access::GraphAccess;
use crate::error::GraphError;
use crate::model::GraphRevision;
use crate::okf;
use crate::write::{apply, GraphReceipt, GraphWriteCommand, IdCounters};
use legacy_journal::Journal;
use lock::CommitLock;

/// Name der Legacy-Journaldatei innerhalb des Graph-Verzeichnisses. Wird nur
/// noch beim Import eines Verzeichnisses aus der Zeit vor der OKF-Umstellung
/// gebraucht — `tests/store.rs` und die Benchmark-Harness referenzieren den
/// Namen.
pub const JOURNAL_FILE: &str = "graph.jsonl";

/// Datei, an der `GraphStore::open` erkennt, dass ein Verzeichnis bereits ein
/// OKF-Bundle ist.
pub const BUNDLE_INDEX: &str = "index.md";

/// Name, unter dem ein migriertes Journal liegen bleibt — nicht gelöscht,
/// damit ein Blick ins Verzeichnis nachvollziehbar bleibt, woher der Bestand
/// kam.
pub const MIGRATED_JOURNAL: &str = "graph.jsonl.migriert";

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
    /// Serialisiert Schreiber UND schützt die ID-Zähler — kein separater
    /// Journal-Zustand mehr, ein Bundle hat kein offenes Datei-Handle.
    writer: Mutex<WriterState>,
    path: Option<PathBuf>,
    /// Schlüssel, mit dem ein von DIESEM Store angelegtes Bundle gezeichnet
    /// wird (siehe [`crate::trust`]). `None` = nicht zeichnen.
    owner_key: Option<String>,
}

struct WriterState {
    ids: IdCounters,
    /// Der Stempel, zu dem `index` passt. Weicht der auf der Platte ab, hat
    /// ein anderer Store committet (siehe [`lock`]).
    stamp: String,
}

impl WriterState {
    fn new(index: &GraphIndex, stamp: String) -> Self {
        WriterState {
            ids: observed_ids(index),
            stamp,
        }
    }
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

/// Zähler aus dem geladenen Stand nachziehen — unabhängig davon, über welchen
/// der drei Zweige in [`GraphStore::open`] der Index entstanden ist.
fn observed_ids(index: &GraphIndex) -> IdCounters {
    let mut ids = IdCounters::default();
    for op in index.to_ops() {
        ids.observe(&op);
    }
    ids
}

impl GraphStore {
    /// Flüchtiger Graph ohne Bundle — für Tests, Beispiele und Läufe, die
    /// nichts hinterlassen sollen.
    pub fn in_memory() -> Self {
        GraphStore {
            index: RwLock::new(Arc::new(GraphIndex::default())),
            writer: Mutex::new(WriterState::new(&GraphIndex::default(), String::new())),
            path: None,
            owner_key: None,
        }
    }

    /// Ein Store über dem Stand in `dir`, der dieses Verzeichnis nie anfasst:
    /// geladen über [`Self::open_read_only`], ohne Pfad. Für Zugriffe, die nur
    /// lesen sollen (`--graph-readonly`) — sie legen damit weder ein Bundle
    /// an, noch migrieren sie ein Journal oder halten je eine Sperre.
    pub fn open_frozen(dir: impl AsRef<Path>) -> Result<Self, GraphError> {
        let index = Self::open_read_only(dir)?;
        Ok(GraphStore {
            writer: Mutex::new(WriterState::new(&index, String::new())),
            index: RwLock::new(Arc::new(index)),
            path: None,
            owner_key: None,
        })
    }

    /// Öffnet den Graphen in `dir`. Drei Fälle, in dieser Reihenfolge:
    ///
    /// 1. `dir/index.md` existiert bereits ⇒ es ist ein OKF-Bundle, wird
    ///    direkt geladen.
    /// 2. sonst `dir/graph.jsonl` existiert ⇒ Einmal-Migration: das Journal
    ///    wird abgespielt, als Bundle neu geschrieben und danach nach
    ///    [`MIGRATED_JOURNAL`] umbenannt. Die Reihenfolge (erst rebuild, dann
    ///    umbenennen) ist sicher: nach `rebuild` existiert `index.md` bereits
    ///    — ein nächstes `open` nimmt also ohnehin Fall 1. Ein fehlgeschlagenes
    ///    Umbenennen (z. B. weil eine andere Instanz gerade dieselbe Migration
    ///    macht) führt deshalb NICHT zu einer zweiten Migration, höchstens zu
    ///    einer liegen gebliebenen `graph.jsonl` neben dem fertigen Bundle.
    /// 3. sonst ein frisches Bundle: `rebuild` mit leerem Index legt
    ///    `index.md` an, damit Fall 1 ab dem nächsten Öffnen greift. **Vorher
    ///    wird geprüft, dass dort keine fremden Markdown-Dateien liegen** —
    ///    siehe [`Self::pruefe_fremdes_verzeichnis`].
    ///
    /// Fall 2 und 3 schreiben und laufen deshalb unter der Commit-Sperre; der
    /// Bestand wird darin noch einmal geprüft, weil ein anderer Prozess das
    /// Bundle inzwischen angelegt haben kann — ein zweites `rebuild` mit
    /// leerem Index räumte dessen erste Dokumente als verwaist weg.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, GraphError> {
        let root = dir.as_ref().to_path_buf();
        // Stempel VOR dem Laden lesen: endet während des Ladens ein fremder
        // Commit, passt der gelesene Stempel nicht mehr, und der erste eigene
        // Commit lädt neu. Andersherum bliebe ein halb gelesener Stand
        // unbemerkt.
        let mut stamp = lock::read_stamp(&root);
        let index = if root.join(BUNDLE_INDEX).exists() {
            replay(okf::bundle::load(&root)?)
        } else {
            let _lock = CommitLock::acquire(&root)?;
            stamp = lock::read_stamp(&root);
            Self::load_or_create(&root)?
        };

        Ok(GraphStore {
            writer: Mutex::new(WriterState::new(&index, stamp)),
            index: RwLock::new(Arc::new(index)),
            path: Some(root),
            owner_key: None,
        })
    }

    /// Wie [`Self::open`], legt aber nichts an, solange nichts geschrieben
    /// wird: ein fehlendes Bundle ist ein leerer Graph, und erst der erste
    /// Commit erzeugt das Verzeichnis (oder migriert ein Legacy-Journal).
    ///
    /// Für Bundles, die es geben KANN, aber nicht muss — das Benutzer-Bundle
    /// entsteht erst, wenn ein Mensch zum ersten Mal etwas dorthin freigibt.
    pub fn open_lazy(dir: impl AsRef<Path>) -> Result<Self, GraphError> {
        let root = dir.as_ref().to_path_buf();
        let stamp = lock::read_stamp(&root);
        let index = Self::open_read_only(&root)?;
        Ok(GraphStore {
            writer: Mutex::new(WriterState::new(&index, stamp)),
            index: RwLock::new(Arc::new(index)),
            path: Some(root),
            owner_key: None,
        })
    }

    /// Legt dieser Store sein Bundle selbst an, zeichnet er es mit `key` —
    /// damit es beim nächsten Start als das eigene erkannt wird.
    pub fn with_owner_key(mut self, key: &str) -> Self {
        self.owner_key = Some(key.to_string());
        self
    }

    /// Die drei Fälle aus [`Self::open`]. Nur unter gehaltener Sperre.
    fn load_or_create(root: &Path) -> Result<GraphIndex, GraphError> {
        if root.join(BUNDLE_INDEX).exists() {
            Ok(replay(okf::bundle::load(root)?))
        } else if root.join(JOURNAL_FILE).exists() {
            let legacy_path = root.join(JOURNAL_FILE);
            let migrated = replay(Journal::read_all(&legacy_path)?);
            okf::bundle::rebuild(root, &migrated)?;
            let _ = std::fs::rename(&legacy_path, root.join(MIGRATED_JOURNAL));
            Ok(migrated)
        } else {
            Self::pruefe_fremdes_verzeichnis(root)?;
            let fresh = GraphIndex::default();
            okf::bundle::rebuild(root, &fresh)?;
            Ok(fresh)
        }
    }

    /// Verweigert das Anlegen eines frischen Bundles in einem Verzeichnis, in
    /// dem schon Markdown liegt.
    ///
    /// Der Grund ist unangenehm konkret: Fall 3 ruft `rebuild` mit leerem
    /// Index, und `rebuild` räumt verwaiste Dokumente weg — in einem leeren
    /// Bundle heißt das JEDE `.md`-Datei unterhalb der Wurzel. Ein
    /// `agentkit --graph ~/notizen` statt `~/.agentkit/graph`, also ein
    /// Tippfehler, hätte den Markdown-Bestand dieses Verzeichnisses beim
    /// ersten Start gelöscht. Unwiederbringlich und ohne Rückfrage.
    ///
    /// Geprüft wird nur beim ANLEGEN. Ein bestehendes Bundle (Fall 1) und die
    /// Migration (Fall 2) sind davon nicht berührt — dort gehört der Inhalt
    /// dem Graphen.
    fn pruefe_fremdes_verzeichnis(root: &Path) -> Result<(), GraphError> {
        fn erste_md(pfad: &Path, tiefe: usize) -> Option<std::path::PathBuf> {
            if tiefe > 4 {
                return None;
            }
            for eintrag in std::fs::read_dir(pfad).ok()?.flatten() {
                let kind = eintrag.path();
                let Ok(meta) = std::fs::symlink_metadata(&kind) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    if let Some(treffer) = erste_md(&kind, tiefe + 1) {
                        return Some(treffer);
                    }
                } else if kind.extension().is_some_and(|e| e == "md") {
                    return Some(kind);
                }
            }
            None
        }

        if let Some(fremd) = erste_md(root, 0) {
            return Err(GraphError::Invalid(format!(
                "{} enthält bereits Markdown ({}), ist aber kein Graph-Bundle. \
                 Ein frisches Bundle würde die vorhandenen .md-Dateien entfernen — \
                 bitte ein leeres oder eigenes Verzeichnis angeben.",
                root.display(),
                fremd.display()
            )));
        }
        Ok(())
    }

    /// Sperrfreier Lesepfad: liest den Bestand in `dir` ein, OHNE irgendeine
    /// Schreibwirkung — weder legt es das Verzeichnis an, noch migriert es ein
    /// Legacy-Journal. Ein Leser darf die Dateien eines lebenden Schreibers
    /// nicht anfassen (siehe `agentkit_viz/tests/viz.rs`,
    /// `graph_endpunkt_ohne_graph_ist_leer_statt_kaputt`).
    ///
    /// Dieselben drei Fälle wie [`Self::open`], aber rein lesend: ein
    /// vorhandenes Bundle wird geladen, ein noch nicht migriertes Legacy-
    /// Journal wird nachsichtig gelesen (`Journal::read_only`,
    /// abgeschnittene letzte Zeile toleriert), und wenn beides fehlt, ist der
    /// Graph für den Leser einfach leer.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<GraphIndex, GraphError> {
        let root = dir.as_ref();
        if root.join(BUNDLE_INDEX).exists() {
            Ok(replay(okf::bundle::load(root)?))
        } else if root.join(JOURNAL_FILE).exists() {
            Ok(replay(Journal::read_only(&root.join(JOURNAL_FILE))?))
        } else {
            Ok(GraphIndex::default())
        }
    }

    /// Pfad des Bundle-Wurzelverzeichnisses (`None` bei [`GraphStore::in_memory`]).
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
    /// im Bundle und für **jeden** Leser sichtbar (Read-your-writes ohne
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

        let mut writer = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        // Ein Bundle, das es noch nicht gibt (`open_lazy`), soll erst ein
        // ANGENOMMENES Kommando anlegen — ein abgelehntes hinterließe sonst
        // ein leeres Verzeichnis samt Bundle. Deshalb hier ein Probelauf ohne
        // Sperre und ohne Wirkung; der eigentliche folgt unten.
        let missing = self
            .path
            .as_deref()
            .is_some_and(|root| !root.join(BUNDLE_INDEX).exists());
        if missing {
            let current = self.snapshot();
            apply(
                &current,
                command.clone(),
                access,
                current.revision() + 1,
                &mut writer.ids.clone(),
            )?;
        }
        // Sperre und Abgleich VOR dem Snapshot: die IDs und die Revision
        // dieser Mutation müssen auf dem Stand der Platte aufsetzen, nicht auf
        // dem dieses Prozesses.
        let _lock = match &self.path {
            Some(root) => Some(self.lock_and_sync(root, &mut writer)?),
            None => None,
        };
        let current = self.snapshot();
        let revision = current.revision() + 1;

        // Auf einer Kopie der Zähler arbeiten: schlägt der Bundle-Commit fehl,
        // sind keine IDs verbraucht.
        let mut next_ids = writer.ids.clone();
        let (ops, receipt) = apply(&current, command, access, revision, &mut next_ids)?;
        if ops.is_empty() {
            return Ok(GraphReceipt {
                revision: current.revision(),
                ..receipt
            });
        }

        let next = current.with_ops(&ops, revision);
        if let Some(root) = &self.path {
            if let Err(e) = okf::bundle::commit(root, &current, &next, &ops) {
                // Ein halber Commit kann Dokumente hinterlassen haben. Der
                // neue Stempel wird bewusst NICHT übernommen: so lädt beim
                // nächsten Schreiben jeder Store neu, dieser eingeschlossen.
                let _ = lock::write_stamp(root, current.revision());
                return Err(e);
            }
            // Der Stempel zuletzt — erst er macht den Commit für andere
            // Stores sichtbar (siehe `open`).
            writer.stamp = lock::write_stamp(root, revision)?;
        }
        writer.ids = next_ids;
        *self.index.write().expect("Graph-Index-Lock nicht poisoned") = Arc::new(next);
        Ok(receipt)
    }

    /// Erwirbt die Commit-Sperre und lädt das Bundle neu, wenn seit dem
    /// letzten eigenen Stand ein anderer Store committet hat.
    fn lock_and_sync(
        &self,
        root: &Path,
        writer: &mut WriterState,
    ) -> Result<CommitLock, GraphError> {
        let lock = CommitLock::acquire(root)?;
        let stamp = lock::read_stamp(root);
        // Ohne `index.md` gibt es das Bundle noch nicht (`open_lazy`) — dann
        // entsteht es hier, mit denselben Prüfungen wie in `open`.
        let created = !root.join(BUNDLE_INDEX).exists();
        if stamp != writer.stamp || created {
            let index = Self::load_or_create(root)?;
            if let (true, Some(key)) = (created, &self.owner_key) {
                crate::trust::sign(root, key)?;
            }
            *writer = WriterState::new(&index, stamp);
            *self.index.write().expect("Graph-Index-Lock nicht poisoned") = Arc::new(index);
        }
        Ok(lock)
    }

    /// Ersetzt den GESAMTEN Bestand durch das, was `edit` aus dem aktuellen
    /// macht, und schreibt das Bundle neu. Der Weg für Eingriffe, die löschen
    /// (siehe [`crate::maintain`]) — bewusst kein [`GraphWriteCommand`]: was
    /// hier geht, soll kein Tool absetzen können.
    ///
    /// Kostet ein vollständiges Neuschreiben des Bundles. Das ist angemessen
    /// für etwas, das ein Mensch von Hand auslöst.
    pub fn rewrite(
        &self,
        edit: impl FnOnce(&GraphIndex, GraphRevision) -> Result<Vec<GraphOp>, GraphError>,
    ) -> Result<GraphRevision, GraphError> {
        let mut writer = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        let _lock = match &self.path {
            Some(root) => Some(self.lock_and_sync(root, &mut writer)?),
            None => None,
        };
        let current = self.snapshot();
        let revision = current.revision() + 1;
        let mut next = replay(edit(&current, revision)?);
        next.set_revision(revision);
        if let Some(root) = &self.path {
            okf::bundle::rebuild(root, &next)?;
            writer.stamp = lock::write_stamp(root, revision)?;
        }
        // Die Zähler fallen nie zurück, solange der Prozess lebt: eine gerade
        // gelöschte ID wird nicht im selben Atemzug neu vergeben.
        let seen = observed_ids(&next);
        writer.ids.entity = writer.ids.entity.max(seen.entity);
        writer.ids.claim = writer.ids.claim.max(seen.claim);
        writer.ids.source = writer.ids.source.max(seen.source);
        writer.ids.episode = writer.ids.episode.max(seen.episode);
        *self.index.write().expect("Graph-Index-Lock nicht poisoned") = Arc::new(next);
        Ok(revision)
    }

    /// Räumt alte Arbeits-Scopes weg (siehe
    /// [`crate::maintain::prune_working_scopes`]). `true`, wenn etwas
    /// entfernt wurde; sonst bleibt das Bundle unberührt.
    pub fn prune_working_scopes(
        &self,
        keep: usize,
        current: Option<&crate::model::GraphScope>,
    ) -> Result<bool, GraphError> {
        // Der billige Blick zuerst: das Neuschreiben kostet das ganze Bundle.
        if crate::maintain::prune_working_scopes(&self.snapshot(), keep, current).is_none() {
            return Ok(false);
        }
        self.rewrite(|index, _| {
            // Unter der Sperre neu gerechnet — der Stand kann sich seit dem
            // Blick oben geändert haben.
            Ok(crate::maintain::prune_working_scopes(index, keep, current)
                .unwrap_or_else(|| index.to_ops()))
        })?;
        Ok(true)
    }

    /// Schreibt das gesamte Bundle aus dem aktuellen Stand neu — z. B. um
    /// verwaiste Dokumente loszuwerden. Ohne Bundle-Pfad ein No-Op. Hält den
    /// Schreiber-Lock: ein gleichzeitiger `submit` darf nicht dazwischenfunken,
    /// sonst könnte `okf::bundle::rebuild` eine gerade committete Datei als
    /// verwaist ansehen und löschen. Aus demselben Grund unter der
    /// Commit-Sperre, die das für andere Prozesse leistet.
    pub fn rebuild_bundle(&self) -> Result<(), GraphError> {
        let mut writer = self
            .writer
            .lock()
            .expect("Graph-Writer-Lock nicht poisoned");
        if let Some(root) = &self.path {
            // Erst abgleichen: ein Rebuild aus einem veralteten Stand räumte
            // die Dokumente fremder Commits als verwaist weg.
            let _lock = self.lock_and_sync(root, &mut writer)?;
            okf::bundle::rebuild(root, &self.snapshot())?;
        }
        Ok(())
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
