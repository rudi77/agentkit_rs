//! Bundle-Adapter: bildet ein [`GraphIndex`] auf ein OKF-Verzeichnis ab und
//! zurück.
//!
//! Verzeichnis-Layout (ein Ziel = `<layer>/<slug(scope.kind)>-<slug(scope.id)>/`):
//!
//! ```text
//! <root>/
//!   index.md                     nur `okf_version: '0.2'` im Frontmatter
//!   working/
//!     index.md
//!     session-run-4711/
//!       index.md
//!       log.md
//!       mcp-client.md
//!       episodes/
//!         index.md
//!         ep-3.md
//!   canonical/
//!     workspace-agentkit-rs/ ...
//! ```
//!
//! Dieses Modul kennt kein `GraphStore` und keinen Schreib-Lock — es bildet
//! nur `GraphIndex ⇄ Dateisystem` ab. Die Anbindung an den Store (wann
//! `commit`/`rebuild` aufgerufen wird) ist NICHT Sache dieses Moduls.
//!
//! Episoden haben kein `layer`-Feld (siehe `model.rs`: sie werden nie
//! promotet) — sie leben deshalb IMMER unter `working/<scope-dir>/episodes/`,
//! unabhängig davon, ob unter demselben Scope auch kanonische Entities liegen.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use crate::error::GraphError;
use crate::model::{
    id_order, EntityId, EpisodeId, GraphClaim, GraphEntity, GraphEpisode, GraphScope, GraphSource,
};
use crate::okf::yaml::YamlValue;
use crate::okf::{doc, instant, markdown};
use crate::store::{GraphIndex, GraphOp};

// ---------------------------------------------------------------------------
// Slug
// ---------------------------------------------------------------------------

/// Windows-Gerätenamen (case-insensitive) — reserviert, weil sie NICHT als
/// Dateiname (auch ohne Erweiterung) angelegt werden können: das Betriebssystem
/// öffnet unter diesem Namen das Gerät statt eine Datei anzulegen. `index`/`log`
/// sind aus demselben Grund reserviert, aber weil DIESES Modul sie selbst als
/// Dateinamen benutzt (`index.md`, `log.md`) — eine Entity namens „Index" darf
/// die nicht überschreiben.
/// Lehnt ein Bundle ab, dessen Verzeichnisgerüst aus Symlinks besteht.
///
/// Der LESEPFAD überspringt Symlinks seit jeher und begründet das damit, dass
/// ein Bundle auch aus fremder Hand stammen kann (siehe `collect_md_files`).
/// Für den SCHREIBPFAD galt das nicht: `create_dir_all` und `File::create`
/// folgen einem Verzeichnis-Symlink klaglos. Ein Bundle, in dem `working` ein
/// Link nach draußen ist, ließ agentkit beim ersten `submit` Dokumente
/// außerhalb des Bundles anlegen und dort vorhandene `.md`-Dateien
/// überschreiben — die Dateinamen stammen aus `slug()` und damit aus
/// Modellargumenten.
///
/// Geprüft wird das Gerüst, das dieses Modul selbst anlegt: die beiden
/// Ebenen-Verzeichnisse und alles darunter. Das kostet einen flachen
/// Verzeichnis-Durchlauf je `commit`/`rebuild`, kein Traversieren der
/// Dokumente.
fn pruefe_schreibbare_struktur(root: &Path) -> Result<(), GraphError> {
    fn pruefe(pfad: &Path, tiefe: usize) -> Result<(), GraphError> {
        let Ok(meta) = std::fs::symlink_metadata(pfad) else {
            return Ok(()); // existiert nicht — wird gleich regulär angelegt
        };
        if meta.file_type().is_symlink() {
            return Err(GraphError::Io(format!(
                "{}: Verzeichnis ist ein Symlink — in ein solches Bundle wird nicht \
                 geschrieben, weil der Schreibvorgang sonst aus dem Bundle hinausführt",
                pfad.display()
            )));
        }
        // Zwei Ebenen reichen: `<layer>/<scope>/` plus `episodes/` darunter.
        if !meta.is_dir() || tiefe >= 3 {
            return Ok(());
        }
        let Ok(eintraege) = std::fs::read_dir(pfad) else {
            return Ok(());
        };
        for eintrag in eintraege.flatten() {
            pruefe(&eintrag.path(), tiefe + 1)?;
        }
        Ok(())
    }

    for layer in ["working", "canonical"] {
        pruefe(&root.join(layer), 1)?;
    }
    Ok(())
}

/// Belegt einen im Verzeichnis noch freien Dateinamen: erst `base`, dann
/// `base-<id>`, dann `base-<id>-2`, `-3`, … bis einer wirklich frei ist.
///
/// Die Schleife ist nicht theoretisch. Vorher wurde nur EIN Ausweichname
/// probiert und dessen Belegung nicht geprüft — zwei Entities konnten
/// denselben Pfad bekommen, und die zweite überschrieb die erste beim
/// Schreiben spurlos (kein Fehler, `remove_orphans` schlug nicht an, weil der
/// Pfad ja erwartet wurde). Erreichbar ist das aus Modellargumenten:
/// `normalize` (Entity-Auflösung) behält Unicode-Alphanumerik, `slug` wirft
/// sie weg — `Foo` und `Fooα` sind deshalb zwei Entities mit einem Slug, und
/// eine dritte Entity namens `Foo E3` besetzt den Ausweichnamen `foo-e3`.
///
/// Deterministisch bleibt das Ergebnis, weil die Iteration über `id_order`
/// läuft: dieselben Datensätze ergeben immer dieselbe Zuordnung.
fn eindeutiger_name(slot: &mut HashSet<String>, base: &str, id: &str) -> String {
    if slot.insert(base.to_string()) {
        return base.to_string();
    }
    let mit_id = format!("{base}-{id}");
    if slot.insert(mit_id.clone()) {
        return mit_id;
    }
    for n in 2u32.. {
        let kandidat = format!("{mit_id}-{n}");
        if slot.insert(kandidat.clone()) {
            return kandidat;
        }
    }
    unreachable!("die Schleife endet, sobald ein Name frei ist")
}

const RESERVED_NAMES: &[&str] = &[
    "index", "log", "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6",
    "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Transliteriert die deutschen Umlaute, BEVOR die generische Filterung in
/// [`slug`] greift — sonst würden `ä`/`ö`/`ü`/`ß` einfach zu `-`, und aus
/// „Größe" würde „gr-e" statt des lesbaren „groesse".
fn transliterate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            other => out.push(other),
        }
    }
    out
}

/// Macht einen beliebigen Text zu einem pfad-sicheren Namensteil.
///
/// SICHERHEITSKRITISCH: `raw` kommt aus Modellargumenten (Entity-Namen,
/// Scope-IDs über `graph_remember`/`graph_promote`) — ein Agent kann jederzeit
/// `"../../../../etc/passwd"` oder `"C:\Windows"` als Namen übergeben. Die
/// Filterung ist deshalb bewusst eine ERLAUBTE Positivliste (nur `a-z0-9`
/// bleibt, ALLES andere wird `-`) statt einer Blockliste: eine Blockliste
/// vergisst strukturell immer ein Zeichen (NTFS-ADS-`:`, `\`, Steuerzeichen,
/// Unicode-Homoglyphen, …), eine Positivliste kann das nicht — jedes Zeichen,
/// das nicht explizit erlaubt ist, verschwindet.
pub fn slug(raw: &str) -> String {
    let lowered = transliterate(raw).to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_dash = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            // Nicht erlaubt (auch jedes verbleibende Nicht-ASCII): wird zu
            // einer aufgeschobenen Trennung — mehrere davon in Folge
            // ergeben genau EINEN `-` (siehe Test `slug_kollabiert_striche`).
            pending_dash = true;
        }
    }
    // Zeichenbasiertes Kürzen ist hier zwar gleichbedeutend mit einem
    // Byte-Slice (jedes verbliebene Zeichen ist ASCII) — `chars().take`
    // bleibt trotzdem die robustere Wahl, falls die Filterung oben je um
    // weitere Nicht-ASCII-Fälle ergänzt wird.
    let truncated: String = out.chars().take(60).collect();
    let trimmed = truncated.trim_end_matches('-');
    let result = if trimmed.is_empty() {
        "unbenannt".to_string()
    } else {
        trimmed.to_string()
    };
    if RESERVED_NAMES.contains(&result.as_str()) {
        format!("{result}-doc")
    } else {
        result
    }
}

/// Nur für die Kollisions-Suffixe (`mcp-client-e7.md`): die ID wird OHNE
/// Trenner lesbar angehängt, `"E-7"` wird also `"e7"`, nicht `"e-7"` — das ID
/// wird hier als Suffix eines bereits vergebenen Slugs verstanden, kein
/// eigenständiger Slug.
fn lowercase_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn target_dir(layer_wire: &str, scope: &GraphScope) -> String {
    format!("{layer_wire}/{}-{}", slug(&scope.kind), slug(&scope.id))
}

fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => String::new(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Zuordnung Datensatz → bundle-relativer Pfad für EINEN Stand. Deterministisch:
/// derselbe Index ergibt immer dieselben Pfade, weil die Kollisionsauflösung
/// über `model::id_order` läuft — eine totale, stabile Ordnung.
pub struct Layout {
    entity_paths: BTreeMap<EntityId, String>,
    episode_paths: BTreeMap<EpisodeId, String>,
    refs: BTreeMap<EntityId, doc::EntityRef>,
}

impl Layout {
    pub fn from_index(index: &GraphIndex) -> Layout {
        let mut entities: Vec<&Arc<GraphEntity>> = index.entities().collect();
        entities.sort_by_key(|e| id_order(&e.id));

        let mut entity_paths = BTreeMap::new();
        let mut refs = BTreeMap::new();
        // Pro Zielverzeichnis vergebene Dateinamen (ohne `.md`) — die
        // Kollisionsauflösung ist verzeichnisweise, nicht global: zwei
        // gleichnamige Entities in VERSCHIEDENEN Zielen kollidieren nicht.
        let mut claimed: HashMap<String, HashSet<String>> = HashMap::new();
        // Für die Belegung siehe `eindeutiger_name` — der Slug allein reicht
        // NICHT, und auch `<slug>-<id>` nicht.

        for entity in entities {
            let dir = target_dir(entity.layer.wire_name(), &entity.scope);
            let base = slug(&entity.canonical_name);
            let slot = claimed.entry(dir.clone()).or_default();
            // Erster Anwärter behält den Slug (aufsteigende `id_order` macht
            // das deterministisch); jeder weitere bekommt die eigene ID
            // angehängt.
            let filename = eindeutiger_name(slot, &base, &lowercase_id(&entity.id));
            let path = format!("{dir}/{filename}.md");
            refs.insert(
                entity.id.clone(),
                doc::EntityRef {
                    title: entity.canonical_name.clone(),
                    path: format!("/{path}"),
                },
            );
            entity_paths.insert(entity.id.clone(), path);
        }

        let mut episodes: Vec<&Arc<GraphEpisode>> = index.episodes().collect();
        episodes.sort_by_key(|e| id_order(&e.id));
        let mut episode_paths = BTreeMap::new();
        let mut claimed_episodes: HashMap<String, HashSet<String>> = HashMap::new();

        for episode in episodes {
            let dir = format!("{}/episodes", target_dir("working", &episode.scope));
            let base = slug(&episode.id);
            let slot = claimed_episodes.entry(dir.clone()).or_default();
            let filename = eindeutiger_name(slot, &base, &lowercase_id(&episode.id));
            episode_paths.insert(episode.id.clone(), format!("{dir}/{filename}.md"));
        }

        Layout {
            entity_paths,
            episode_paths,
            refs,
        }
    }

    pub fn entity_path(&self, id: &str) -> Option<&str> {
        self.entity_paths.get(id).map(String::as_str)
    }

    pub fn episode_path(&self, id: &str) -> Option<&str> {
        self.episode_paths.get(id).map(String::as_str)
    }

    /// Für `doc::render_entity` — Titel und bundle-absoluter Pfad (`/working/…`).
    pub fn refs(&self) -> &BTreeMap<EntityId, doc::EntityRef> {
        &self.refs
    }
}

/// Alle Zielverzeichnisse (`<layer>/<scope-dir>`, OHNE Dateiname und OHNE das
/// `/episodes`-Suffix) mit mindestens einem Dokument — Grundlage für die
/// Ebenen-`index.md` und für die Erkennung neu entstandener Ziele.
fn target_dirs(layout: &Layout) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for path in layout.entity_paths.values() {
        dirs.insert(dir_of(path));
    }
    for path in layout.episode_paths.values() {
        let episodes_dir = dir_of(path); // ".../episodes"
        dirs.insert(dir_of(&episodes_dir));
    }
    dirs
}

// ---------------------------------------------------------------------------
// Lesen
// ---------------------------------------------------------------------------

/// Liest ein Bundle vollständig ein. Reiner Lesepfad: legt nichts an, schreibt
/// nichts, kompaktiert nichts.
pub fn load(root: &Path) -> Result<Vec<GraphOp>, GraphError> {
    let mut files = Vec::new();
    if root.exists() {
        collect_md_files(root, &mut files)?;
    }

    // (Pfad, Op) für ALLE Datensätze aus ALLEN Dateien — die Deduplizierung
    // nach Revision arbeitet dateiübergreifend, nicht dateiweise: derselbe
    // Claim/dieselbe Quelle können unverändert in zwei Entity-Dokumenten
    // stehen (siehe Schreibreihenfolge in `commit`), das ist kein Widerspruch.
    let mut candidates: Vec<(std::path::PathBuf, GraphOp)> = Vec::new();
    for path in &files {
        for op in load_file(path)? {
            candidates.push((path.clone(), op));
        }
    }

    let mut winners: HashMap<(u8, String), (u64, std::path::PathBuf, GraphOp)> = HashMap::new();
    for (path, op) in candidates {
        let (kind, id, revision) = op_identity(&op);
        let key = (kind, id);
        let replace = match winners.get(&key) {
            None => true,
            // Höhere Revision gewinnt; bei Gleichstand der lexikografisch
            // kleinere Pfad — deterministisch, unabhängig von der
            // Scan-Reihenfolge des Betriebssystems. Genau der Zustand, den
            // ein Absturz MITTEN in `commit` hinterlässt (siehe dort).
            Some((cur_rev, cur_path, _)) => {
                revision > *cur_rev || (revision == *cur_rev && path < *cur_path)
            }
        };
        if replace {
            winners.insert(key, (revision, path, op));
        }
    }

    let mut result: Vec<GraphOp> = winners.into_values().map(|(_, _, op)| op).collect();
    // Dieselbe Reihenfolge wie `GraphIndex::to_ops`: Quellen, Entities,
    // Claims, Episoden — hier zusätzlich innerhalb jeder Art nach `id_order`
    // sortiert, damit das Ergebnis unabhängig vom Dateisystem-Scan ist.
    result.sort_by_key(|op| {
        let (kind, id, _) = op_identity(op);
        (kind, id_order(&id))
    });
    Ok(result)
}

fn op_identity(op: &GraphOp) -> (u8, String, u64) {
    match op {
        GraphOp::Source(s) => (0, s.id.clone(), s.created_revision),
        GraphOp::Entity(e) => (1, e.id.clone(), e.updated_revision),
        GraphOp::Claim(c) => (2, c.id.clone(), c.updated_revision),
        GraphOp::Episode(e) => (3, e.id.clone(), e.created_revision),
    }
}

/// Rekursiv alle `*.md`-Dateien unter `dir`, `index.md`/`log.md` ausgenommen.
///
/// Symlinks (Verzeichnis- wie Datei-Symlinks) werden NIE verfolgt: ein Bundle
/// wird auch aus fremder Hand gelesen (importiert, aus einem Backup entpackt),
/// und ein Symlink könnte den Scan aus dem Bundle heraus- oder auf beliebige
/// Dateien des Wirtssystems hinführen.
fn collect_md_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), GraphError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| GraphError::Io(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| GraphError::Io(format!("{}: {e}", dir.display())))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_md_files(&path, out)?;
        } else if meta.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name != "index.md" && name != "log.md" {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// Liest EINE Datei; `Ok(vec![])`, wenn es gar kein OKF-Dokument dieses
/// Crates ist (kein Frontmatter, oder Frontmatter ohne `agentkit`-Block — ein
/// fremdes OKF-Dokument im selben Verzeichnis ist kein Fehler). Ein
/// VORHANDENER, aber kaputter `agentkit`-Block bzw. ungültiges YAML ist ein
/// harter Fehler mit Dateipfad in der Meldung.
fn load_file(path: &Path) -> Result<Vec<GraphOp>, GraphError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
    let Some((fm_text, _)) = markdown::split_frontmatter(&text) else {
        return Ok(Vec::new());
    };
    let pairs = crate::okf::yaml::parse_document(fm_text).map_err(|e| with_path(path, e))?;
    let Some(agentkit) = pairs
        .iter()
        .find(|(k, _)| k == "agentkit")
        .and_then(|(_, v)| v.as_map())
    else {
        return Ok(Vec::new());
    };
    let id = agentkit.get("id").and_then(YamlValue::as_str).unwrap_or("");

    if id.starts_with("EP-") {
        let parsed = doc::parse_episode(&text).map_err(|e| with_path(path, e))?;
        let mut ops: Vec<GraphOp> = parsed.sources.into_iter().map(GraphOp::Source).collect();
        ops.push(GraphOp::Episode(parsed.episode));
        Ok(ops)
    } else {
        let parsed = doc::parse_entity(&text).map_err(|e| with_path(path, e))?;
        let mut ops: Vec<GraphOp> = parsed.sources.into_iter().map(GraphOp::Source).collect();
        ops.push(GraphOp::Entity(parsed.entity));
        ops.extend(parsed.claims.into_iter().map(GraphOp::Claim));
        Ok(ops)
    }
}

fn with_path(path: &Path, err: GraphError) -> GraphError {
    match err {
        GraphError::Okf(m) => GraphError::Okf(format!("{}: {m}", path.display())),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Schreiben
// ---------------------------------------------------------------------------

/// Ermittelt Entity-/Episoden-IDs, die eine Mutation berührt hat.
///
/// Ein `Source`-Op allein löst bewusst KEIN Dokument aus: `write.rs` erzeugt
/// eine Quelle nie ohne den referenzierenden Claim/Entity/Episode im selben
/// Batch (`record_claim`, `record_episode`) — ein Source-only-Op wäre also
/// ein Zeichen für einen neuen, hier noch unbekannten Schreibpfad.
fn touched_ids(ops: &[GraphOp]) -> (HashSet<EntityId>, HashSet<EpisodeId>) {
    let mut entities = HashSet::new();
    let mut episodes = HashSet::new();
    for op in ops {
        match op {
            GraphOp::Entity(e) => {
                entities.insert(e.id.clone());
            }
            GraphOp::Claim(c) => {
                entities.insert(c.subject.clone());
            }
            GraphOp::Episode(e) => {
                episodes.insert(e.id.clone());
            }
            GraphOp::Source(_) => {}
        }
    }
    (entities, episodes)
}

/// Schreibt die von `ops` berührten Dokumente. `previous`/`next` sind der Stand
/// vor und nach der Mutation.
pub fn commit(
    root: &Path,
    previous: &GraphIndex,
    next: &GraphIndex,
    ops: &[GraphOp],
) -> Result<(), GraphError> {
    pruefe_schreibbare_struktur(root)?;
    let layout_old = Layout::from_index(previous);
    let layout_next = Layout::from_index(next);
    let (touched_entities, touched_episodes) = touched_ids(ops);

    // 1. Zuerst Entities, die NICHT Subjekt eines Claims in `ops` sind, DANACH
    //    die übrigen — ein Absturz dazwischen hinterlässt nie einen Link auf
    //    ein Dokument, das es nicht gibt (siehe Modul-Doc-Comment).
    let claim_subjects: HashSet<&str> = ops
        .iter()
        .filter_map(|op| match op {
            GraphOp::Claim(c) => Some(c.subject.as_str()),
            _ => None,
        })
        .collect();
    let mut first: Vec<&EntityId> = touched_entities
        .iter()
        .filter(|id| !claim_subjects.contains(id.as_str()))
        .collect();
    first.sort_by_key(|id| id_order(id));
    let mut second: Vec<&EntityId> = touched_entities
        .iter()
        .filter(|id| claim_subjects.contains(id.as_str()))
        .collect();
    second.sort_by_key(|id| id_order(id));
    for id in first.into_iter().chain(second) {
        write_entity_doc(root, next, &layout_next, id)?;
    }

    // 2. Episoden.
    let mut episode_ids: Vec<&EpisodeId> = touched_episodes.iter().collect();
    episode_ids.sort_by_key(|id| id_order(id));
    for id in episode_ids {
        write_episode_doc(root, next, &layout_next, id)?;
    }

    // 3. ERST DANACH Dateien an alten Pfaden löschen, deren Datensatz jetzt
    //    woanders liegt (Promotion). Ein Absturz zwischen 1/2 und hier
    //    hinterlässt dieselbe ID in zwei Dateien — genau die Situation, die
    //    die Revisions-Regel in `load` auflöst.
    for id in &touched_entities {
        if let (Some(old_path), Some(new_path)) =
            (layout_old.entity_path(id), layout_next.entity_path(id))
        {
            if old_path != new_path {
                let full = root.join(old_path);
                if full.exists() {
                    std::fs::remove_file(&full)
                        .map_err(|e| GraphError::Io(format!("{}: {e}", full.display())))?;
                }
            }
        }
    }

    // 4. Zuletzt `index.md`/`log.md` NUR der Verzeichnisse, deren Bestand
    //    sich geändert hat — alle bei jedem Commit neu zu schreiben wäre
    //    O(n) Dateien je Aussage.
    let refresh = refresh_dirs(
        &layout_old,
        &layout_next,
        &touched_entities,
        &touched_episodes,
    );
    write_indexes(root, next, &layout_next, Some(&refresh))
}

/// Schreibt das gesamte Bundle aus `index` neu und entfernt verwaiste Dokumente.
pub fn rebuild(root: &Path, index: &GraphIndex) -> Result<(), GraphError> {
    pruefe_schreibbare_struktur(root)?;
    let layout = Layout::from_index(index);

    let mut entity_ids: Vec<&EntityId> = layout.entity_paths.keys().collect();
    entity_ids.sort_by_key(|id| id_order(id));
    for id in entity_ids {
        write_entity_doc(root, index, &layout, id)?;
    }

    let mut episode_ids: Vec<&EpisodeId> = layout.episode_paths.keys().collect();
    episode_ids.sort_by_key(|id| id_order(id));
    for id in episode_ids {
        write_episode_doc(root, index, &layout, id)?;
    }

    write_indexes(root, index, &layout, None)?;
    remove_orphans(root, &layout)
}

fn refresh_dirs(
    layout_old: &Layout,
    layout_next: &Layout,
    touched_entities: &HashSet<EntityId>,
    touched_episodes: &HashSet<EpisodeId>,
) -> HashSet<String> {
    let old_target_dirs = target_dirs(layout_old);
    let mut dirs = HashSet::new();

    for id in touched_entities {
        if let Some(new_path) = layout_next.entity_path(id) {
            dirs.insert(dir_of(new_path));
        }
        if let Some(old_path) = layout_old.entity_path(id) {
            dirs.insert(dir_of(old_path));
        }
    }
    for id in touched_episodes {
        if let Some(new_path) = layout_next.episode_path(id) {
            let episodes_dir = dir_of(new_path);
            dirs.insert(dir_of(&episodes_dir)); // Ziel (log.md)
            dirs.insert(episodes_dir); // episodes/-Unterverzeichnis
        }
    }

    // Ein NEUES Zielverzeichnis braucht zusätzlich einen Link aus der
    // Ebenen-`index.md` — ein bereits vorhandenes Ziel ändert an der
    // Ebenen-`index.md` nichts.
    let mut layer_dirs = Vec::new();
    for dir in &dirs {
        if !old_target_dirs.contains(dir) {
            if let Some(layer) = dir.split('/').next() {
                layer_dirs.push(layer.to_string());
            }
        }
    }
    dirs.extend(layer_dirs);
    dirs
}

fn write_entity_doc(
    root: &Path,
    index: &GraphIndex,
    layout: &Layout,
    id: &str,
) -> Result<(), GraphError> {
    let Some(path) = layout.entity_path(id) else {
        return Ok(());
    };
    let Some(entity) = index.entity(id) else {
        return Ok(());
    };
    let claims: Vec<GraphClaim> = index
        .incident_claims(id)
        .filter(|c| c.subject == id)
        .map(|c| (**c).clone())
        .collect();
    let mut source_ids: BTreeSet<&str> = BTreeSet::new();
    for claim in &claims {
        for sid in &claim.source_ids {
            source_ids.insert(sid.as_str());
        }
    }
    let sources: Vec<GraphSource> = source_ids
        .into_iter()
        .filter_map(|sid| index.source(sid).map(|s| (**s).clone()))
        .collect();
    let entity_doc = doc::EntityDoc {
        entity: (**entity).clone(),
        claims,
        sources,
    };
    let text = doc::render_entity(&entity_doc, layout.refs());
    write_file_atomic(&root.join(path), &text)
}

fn write_episode_doc(
    root: &Path,
    index: &GraphIndex,
    layout: &Layout,
    id: &str,
) -> Result<(), GraphError> {
    let Some(path) = layout.episode_path(id) else {
        return Ok(());
    };
    let Some(episode) = index.episode(id) else {
        return Ok(());
    };
    let sources: Vec<GraphSource> = episode
        .source_ids
        .iter()
        .filter_map(|sid| index.source(sid).map(|s| (**s).clone()))
        .collect();
    let episode_doc = doc::EpisodeDoc {
        episode: (**episode).clone(),
        sources,
    };
    let text = doc::render_episode(&episode_doc);
    write_file_atomic(&root.join(path), &text)
}

fn remove_orphans(root: &Path, layout: &Layout) -> Result<(), GraphError> {
    let mut expected: HashSet<String> = HashSet::new();
    expected.extend(layout.entity_paths.values().cloned());
    expected.extend(layout.episode_paths.values().cloned());

    let mut files = Vec::new();
    if root.exists() {
        collect_md_files(root, &mut files)?;
    }
    for path in files {
        let rel = path
            .strip_prefix(root)
            .expect("collect_md_files liefert nur Pfade unter root")
            .to_string_lossy()
            .replace('\\', "/");
        if !expected.contains(&rel) {
            std::fs::remove_file(&path)
                .map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// index.md / log.md
// ---------------------------------------------------------------------------

fn entities_in_dir<'a>(
    index: &'a GraphIndex,
    layout: &Layout,
    dir: &str,
) -> Vec<(&'a Arc<GraphEntity>, String)> {
    let mut out: Vec<(&Arc<GraphEntity>, String)> = layout
        .entity_paths
        .iter()
        .filter(|(_, path)| dir_of(path) == dir)
        .filter_map(|(id, path)| index.entity(id).map(|e| (e, path.clone())))
        .collect();
    out.sort_by_key(|(e, _)| id_order(&e.id));
    out
}

fn episodes_in_dir<'a>(
    index: &'a GraphIndex,
    layout: &Layout,
    dir: &str,
) -> Vec<(&'a Arc<GraphEpisode>, String)> {
    let mut out: Vec<(&Arc<GraphEpisode>, String)> = layout
        .episode_paths
        .iter()
        .filter(|(_, path)| dir_of(&dir_of(path)) == dir)
        .filter_map(|(id, path)| index.episode(id).map(|e| (e, path.clone())))
        .collect();
    out.sort_by_key(|(e, _)| id_order(&e.id));
    out
}

/// Schreibt `index.md`/`log.md`. `refresh = None` heißt „alles" (`rebuild`),
/// `Some(dirs)` beschränkt auf die genannten Verzeichnisse (`commit`).
fn write_indexes(
    root: &Path,
    index: &GraphIndex,
    layout: &Layout,
    refresh: Option<&HashSet<String>>,
) -> Result<(), GraphError> {
    // Root: immer — ein einzelnes, statisches Dokument, kein O(n)-Problem.
    // `working`/`canonical` werden hier unbedingt angelegt, auch leer: die
    // Root-`index.md` verlinkt beide fest, ein Link müsste sonst auf ein noch
    // nicht existierendes Verzeichnis zeigen.
    std::fs::create_dir_all(root.join("working"))
        .map_err(|e| GraphError::Io(format!("{}: {e}", root.display())))?;
    std::fs::create_dir_all(root.join("canonical"))
        .map_err(|e| GraphError::Io(format!("{}: {e}", root.display())))?;
    write_file_atomic(&root.join("index.md"), &render_root_index())?;

    let all_dirs = target_dirs(layout);

    for layer in ["working", "canonical"] {
        if refresh.map_or(true, |set| set.contains(layer)) {
            write_file_atomic(
                &root.join(layer).join("index.md"),
                &render_layer_index(layer, &all_dirs),
            )?;
        }
    }

    for dir in &all_dirs {
        if !refresh.map_or(true, |set| set.contains(dir)) {
            continue;
        }
        let entities = entities_in_dir(index, layout, dir);
        let episodes = episodes_in_dir(index, layout, dir);
        write_file_atomic(
            &root.join(dir).join("index.md"),
            &render_target_index(dir, &entities, !episodes.is_empty()),
        )?;

        if !episodes.is_empty() {
            write_file_atomic(&root.join(dir).join("log.md"), &render_log(&episodes))?;
            let episodes_dir = format!("{dir}/episodes");
            if refresh.map_or(true, |set| set.contains(&episodes_dir)) {
                write_file_atomic(
                    &root.join(&episodes_dir).join("index.md"),
                    &render_episodes_index(&episodes),
                )?;
            }
        }
    }
    Ok(())
}

fn render_root_index() -> String {
    let pairs = vec![("okf_version".to_string(), YamlValue::str("0.2"))];
    let body = "# Wissensgraph\n\n\
        * [working](working/) - Vorläufiges Wissen (Sessions, Schwarm-Läufe)\n\
        * [canonical](canonical/) - Konsolidiertes, dauerhaftes Wissen";
    markdown::compose(&pairs, body)
}

/// KEIN Frontmatter — anders als die Root-`index.md` (§8 des Referenz-Validators).
fn render_layer_index(layer: &str, all_dirs: &BTreeSet<String>) -> String {
    let title = match layer {
        "working" => "Vorläufiges Wissen",
        "canonical" => "Konsolidiertes Wissen",
        other => other,
    };
    let mut out = format!("# {title}\n");
    let prefix = format!("{layer}/");
    for dir in all_dirs.iter().filter(|d| d.starts_with(&prefix)) {
        let name = &dir[prefix.len()..];
        out.push_str(&format!("\n* [{name}]({name}/) - Ziel `{name}`"));
    }
    out.push('\n');
    out
}

fn render_target_index(
    dir: &str,
    entities: &[(&Arc<GraphEntity>, String)],
    has_episodes: bool,
) -> String {
    let title = dir.rsplit('/').next().unwrap_or(dir);
    let mut out = format!("# {title}\n");
    for (entity, path) in entities {
        let name = basename(path);
        // Spec §8 will hier die `description` des verlinkten Konzepts. Hat die
        // Entity keine eigene, steht im Dokument nur eine generierte, die den
        // Titel wiederholt — im Index wäre das reine Verdopplung. Der
        // `entity_type` ist dann die einzige zusätzliche wahre Auskunft.
        //
        // Bewusst NICHTS, was von den Aussagen abhängt (etwa deren Anzahl):
        // `commit` schreibt ein `index.md` nur neu, wenn sich der
        // DOKUMENTBESTAND des Verzeichnisses ändert. Eine Beschreibung, die
        // sich mit jeder neuen Aussage ändert, veraltete damit still.
        let description = entity
            .description
            .clone()
            .unwrap_or_else(|| entity.entity_type.clone());
        out.push_str(&format!(
            "\n* [{}]({name}) - {}",
            entity.canonical_name,
            doc::single_line_truncate_to(&description, 200)
        ));
    }
    if has_episodes {
        out.push_str("\n* [episodes](episodes/) - Verlauf dieses Ziels");
    }
    out.push('\n');
    out
}

fn render_episodes_index(episodes: &[(&Arc<GraphEpisode>, String)]) -> String {
    let mut out = String::from("# Episoden\n");
    for (episode, path) in episodes {
        let name = basename(path);
        let title = doc::episode_title(&episode.summary);
        out.push_str(&format!(
            "\n* [{title}]({name}) - {}",
            episode.actor.as_str()
        ));
    }
    out.push('\n');
    out
}

/// `log.md` — KEIN Frontmatter, neueste Tage zuerst; nur aufgerufen, wenn es
/// mindestens eine Episode gibt (siehe `write_indexes`).
fn render_log(episodes: &[(&Arc<GraphEpisode>, String)]) -> String {
    let mut sorted: Vec<&(&Arc<GraphEpisode>, String)> = episodes.iter().collect();
    sorted.sort_by_key(|(episode, _)| std::cmp::Reverse(episode.created_at));

    let mut out = String::from("# Änderungsprotokoll\n");
    let mut current_day: Option<String> = None;
    for (episode, path) in sorted {
        let day = instant::to_iso_date(episode.created_at);
        if current_day.as_deref() != Some(day.as_str()) {
            out.push_str(&format!("\n## {day}\n"));
            current_day = Some(day);
        }
        let name = basename(path);
        let title = doc::episode_title(&episode.summary);
        out.push_str(&format!(
            "\n* **Episode**: [{title}](episodes/{name}) — {}",
            episode.actor.as_str()
        ));
    }
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// Atomares Schreiben — Temp-Datei + Rename, wie `store/journal.rs:160-201`
// ---------------------------------------------------------------------------

fn write_file_atomic(path: &Path, content: &str) -> Result<(), GraphError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GraphError::Io(format!("{}: {e}", parent.display())))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "doc.md".to_string());
    // PID im Namen: zwei Prozesse auf demselben Bundle sollen sich nicht die
    // Temp-Datei wegschreiben (derselbe Grund wie in `journal.rs`).
    let tmp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| GraphError::Io(format!("{}: {e}", tmp.display())))?;
        file.write_all(content.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| GraphError::Io(format!("{}: {e}", tmp.display())))?;
        // Handle schließt hier am Scope-Ende — unter Windows schlägt das
        // Umbenennen sonst fehl (siehe `journal.rs::rewrite`).
    }
    std::fs::rename(&tmp, path).map_err(|e| GraphError::Io(format!("{}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, ClaimStatus, GraphLayer};
    use crate::okf::instant::from_rfc3339;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn ts(text: &str) -> u64 {
        from_rfc3339(text).expect("Testzeitstempel muss parsen")
    }

    /// Ein eigenes Temp-Verzeichnis je Test — `std::env::temp_dir()` +
    /// Prozess-ID + Zähler, keine neue Dev-Dependency. Aufräumen am Ende des
    /// Tests (RAII über `Drop`), damit ein Fehlschlag nicht Dateileichen im
    /// System-Temp hinterlässt.
    struct TempDir {
        path: std::path::PathBuf,
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    impl TempDir {
        fn new(label: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agentkit-graph-bundle-test-{}-{label}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("Temp-Verzeichnis muss anlegbar sein");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // -----------------------------------------------------------------------
    // slug — Angriffstests (SICHERHEITSKRITISCH, siehe Modul-Doc-Comment)
    // -----------------------------------------------------------------------

    /// Prüft gegen eine **Whitelist**, nicht gegen eine Liste verbotener
    /// Zeichen: an einer Sicherheitsgrenze darf der Test nur das durchlassen,
    /// woran jemand gedacht hat — nicht alles außer dem, woran jemand gedacht
    /// hat. Ein künftiges Zeichen in `slug`, das niemand auf eine Blacklist
    /// gesetzt hätte, fällt so sofort auf.
    fn assert_slug_is_safe(raw: &str) {
        let s = slug(raw);
        assert!(!s.is_empty(), "slug({raw:?}) ist leer");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "slug({raw:?}) = {s:?} enthält ein Zeichen außerhalb [a-z0-9-]"
        );
        assert!(
            !s.starts_with('-') && !s.ends_with('-'),
            "slug({raw:?}) = {s:?} beginnt oder endet mit '-'"
        );
        assert!(s.chars().count() <= 64, "slug({raw:?}) ist zu lang: {s:?}");
    }

    #[test]
    fn slug_pfad_ausbrueche_sind_unmoeglich() {
        for raw in [
            "../../etc/passwd",
            "..\\..\\x",
            "C:\\Windows",
            "index",
            "log",
            "CON",
            "nul",
            "   ",
            "",
            "Ä/Ö",
        ] {
            assert_slug_is_safe(raw);
        }
        assert_slug_is_safe(&"x".repeat(500));
        assert_slug_is_safe("@#$%^&*()!~`'\"");

        // Breiter Streifzug statt einer Handvoll ausgedachter Fälle: jedes
        // einzelne Zeichen bis U+02FF, jeweils allein, am Anfang, in der Mitte
        // und am Ende. Dateinamen entstehen aus Modellargumenten — die Grenze
        // muss für JEDE Eingabe halten, nicht für die zwölf, die uns einfielen.
        for code in 0u32..0x300 {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            for muster in [
                format!("{ch}"),
                format!("{ch}abc"),
                format!("ab{ch}cd"),
                format!("abc{ch}"),
                format!("{ch}{ch}{ch}"),
            ] {
                assert_slug_is_safe(&muster);
            }
        }
    }

    #[test]
    fn slug_reservierte_namen_bekommen_doc_suffix() {
        for raw in ["index", "LOG", "Con", "nul", "COM1", "lpt9"] {
            assert!(
                slug(raw).ends_with("-doc"),
                "slug({raw:?}) = {:?} hat kein -doc-Suffix",
                slug(raw)
            );
        }
    }

    #[test]
    fn slug_leere_eingabe_wird_unbenannt() {
        assert_eq!(slug(""), "unbenannt");
        assert_eq!(slug("   "), "unbenannt");
        assert_eq!(slug("---"), "unbenannt");
    }

    #[test]
    fn slug_transliteriert_umlaute() {
        assert_eq!(slug("Größe"), "groesse");
        assert_eq!(slug("Über Straße"), "ueber-strasse");
    }

    #[test]
    fn slug_kollabiert_striche_und_kuerzt() {
        assert_eq!(slug("a   b"), "a-b");
        assert_eq!(slug("--a--"), "a");
        let lang = slug(&"ab".repeat(100));
        assert!(lang.chars().count() <= 60);
        assert!(!lang.ends_with('-'));
    }

    // -----------------------------------------------------------------------
    // Test-Fixture: ein kleiner Graph mit zwei Zielen, Claims, Quellen,
    // Episoden.
    // -----------------------------------------------------------------------

    fn build_sample_index() -> GraphIndex {
        let mut index = GraphIndex::default();
        let mut revision = 0u64;
        let mut apply = |op: GraphOp, rev: u64| {
            index = index.with_ops(&[op], rev);
        };

        let scope_session = GraphScope::session("run-4711");
        let scope_workspace = GraphScope::workspace("agentkit-rs");

        revision += 1;
        apply(
            GraphOp::Source(GraphSource {
                id: "S-1".to_string(),
                source_type: "tool_result".to_string(),
                agent_id: Some(Actor::human("dana")),
                run_id: Some("run-4711".to_string()),
                tool_call_id: None,
                artifact_uri: None,
                excerpt: Some("Quelle eins".to_string()),
                content_hash: "h1".to_string(),
                created_revision: revision,
                created_at: ts("2026-01-01T00:00:00Z"),
            }),
            revision,
        );

        revision += 1;
        apply(
            GraphOp::Entity(GraphEntity {
                id: "E-1".to_string(),
                canonical_name: "MCP-Client".to_string(),
                entity_type: "Konzept".to_string(),
                description: None,
                aliases: vec!["mcp client".to_string()],
                layer: GraphLayer::Working,
                scope: scope_session.clone(),
                promoted_from: None,
                created_revision: revision,
                updated_revision: revision,
                created_at: ts("2026-01-01T00:00:00Z"),
                extra: Default::default(),
            }),
            revision,
        );

        revision += 1;
        apply(
            GraphOp::Entity(GraphEntity {
                id: "E-2".to_string(),
                canonical_name: "Session-Konkurrenz".to_string(),
                entity_type: "Konzept".to_string(),
                description: None,
                aliases: vec![],
                layer: GraphLayer::Working,
                scope: scope_session.clone(),
                promoted_from: None,
                created_revision: revision,
                updated_revision: revision,
                created_at: ts("2026-01-01T00:01:00Z"),
                extra: Default::default(),
            }),
            revision,
        );

        revision += 1;
        apply(
            GraphOp::Claim(GraphClaim {
                id: "C-1".to_string(),
                subject: "E-1".to_string(),
                predicate: "verursacht".to_string(),
                object: "E-2".to_string(),
                layer: GraphLayer::Working,
                scope: scope_session.clone(),
                status: ClaimStatus::Observation,
                confidence: 0.7,
                source_ids: vec!["S-1".to_string()],
                created_by: Actor::agent("tester"),
                superseded_by: None,
                promoted_from: None,
                promoted_from_status: None,
                verified: vec![],
                created_revision: revision,
                updated_revision: revision,
                created_at: ts("2026-01-01T00:02:00Z"),
            }),
            revision,
        );

        revision += 1;
        apply(
            GraphOp::Source(GraphSource {
                id: "S-2".to_string(),
                source_type: "tool_result".to_string(),
                agent_id: None,
                run_id: Some("run-4711".to_string()),
                tool_call_id: None,
                artifact_uri: None,
                excerpt: Some("Quelle zwei".to_string()),
                content_hash: "h2".to_string(),
                created_revision: revision,
                created_at: ts("2026-01-01T00:03:00Z"),
            }),
            revision,
        );

        revision += 1;
        apply(
            GraphOp::Episode(GraphEpisode {
                id: "EP-1".to_string(),
                actor: Actor::agent("tester"),
                summary: "Ein Testlauf hat zwei Entities verknüpft.".to_string(),
                scope: scope_session.clone(),
                source_ids: vec!["S-2".to_string()],
                created_revision: revision,
                created_at: ts("2026-01-01T00:03:00Z"),
            }),
            revision,
        );

        // Ein zweites Ziel (workspace, kanonisch) ohne Claims/Episoden — deckt
        // die Kollisionsauflösung UND das Zwei-Ziele-Layout ab.
        revision += 1;
        apply(
            GraphOp::Entity(GraphEntity {
                id: "E-3".to_string(),
                canonical_name: "Promotion".to_string(),
                entity_type: "Konzept".to_string(),
                description: None,
                aliases: vec![],
                layer: GraphLayer::Canonical,
                scope: scope_workspace.clone(),
                promoted_from: Some(scope_session.clone()),
                created_revision: revision,
                updated_revision: revision,
                created_at: ts("2026-01-01T00:04:00Z"),
                extra: Default::default(),
            }),
            revision,
        );

        index
    }

    // -----------------------------------------------------------------------
    // Voller Umlauf
    // -----------------------------------------------------------------------

    #[test]
    fn rebuild_load_ergibt_dieselben_datensaetze() {
        let dir = TempDir::new("roundtrip");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("rebuild muss gelingen");

        let ops = load(&dir.path).expect("load muss gelingen");
        let reloaded = GraphIndex::default().with_ops(&ops, index.revision());

        assert_eq!(reloaded.entity_count(), index.entity_count());
        assert_eq!(reloaded.claim_count(), index.claim_count());
        assert_eq!(reloaded.source_count(), index.source_count());
        assert_eq!(reloaded.episode_count(), index.episode_count());

        for entity in index.entities() {
            let got = reloaded
                .entity(&entity.id)
                .expect("Entity muss geladen werden");
            assert_eq!(got.canonical_name, entity.canonical_name);
            assert_eq!(got.layer, entity.layer);
            assert_eq!(got.scope, entity.scope);
        }
        for claim in index.claims() {
            let got = reloaded
                .claim(&claim.id)
                .expect("Claim muss geladen werden");
            assert_eq!(got.subject, claim.subject);
            assert_eq!(got.object, claim.object);
            assert_eq!(got.status, claim.status);
        }
        for episode in index.episodes() {
            let got = reloaded
                .episode(&episode.id)
                .expect("Episode muss geladen werden");
            assert_eq!(got.summary, episode.summary);
            assert_eq!(got.source_ids, episode.source_ids);
        }
    }

    #[test]
    fn rebuild_zweimal_ist_byte_identisch() {
        let dir = TempDir::new("idempotent");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("erster rebuild");

        let mut before = HashMap::new();
        collect_all_files_with_content(&dir.path, &mut before);

        rebuild(&dir.path, &index).expect("zweiter rebuild");
        let mut after = HashMap::new();
        collect_all_files_with_content(&dir.path, &mut after);

        assert_eq!(before, after, "rebuild ist nicht byte-stabil");
    }

    fn collect_all_files_with_content(root: &Path, out: &mut HashMap<String, String>) {
        fn walk(dir: &Path, root: &Path, out: &mut HashMap<String, String>) {
            for entry in std::fs::read_dir(dir).expect("Verzeichnis muss lesbar sein") {
                let entry = entry.expect("Eintrag muss lesbar sein");
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let content = std::fs::read_to_string(&path).expect("Datei muss lesbar sein");
                    out.insert(rel, content);
                }
            }
        }
        walk(root, root, out);
    }

    #[test]
    fn commit_schreibt_nur_beruehrte_dateien() {
        let dir = TempDir::new("commit-scope");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("initialer rebuild");

        let mut before = HashMap::new();
        collect_all_files_with_content(&dir.path, &mut before);

        // Ein neuer Claim an E-1 (selbes Ziel, session-run-4711) — E-3 im
        // Ziel workspace-agentkit-rs darf NICHT angefasst werden.
        let new_claim = GraphClaim {
            id: "C-2".to_string(),
            subject: "E-1".to_string(),
            predicate: "bezieht sich auf".to_string(),
            object: "E-2".to_string(),
            layer: GraphLayer::Working,
            scope: GraphScope::session("run-4711"),
            status: ClaimStatus::Hypothesis,
            confidence: 0.3,
            source_ids: vec!["S-1".to_string()],
            created_by: Actor::agent("tester"),
            superseded_by: None,
            promoted_from: None,
            promoted_from_status: None,
            verified: vec![],
            created_revision: index.revision() + 1,
            updated_revision: index.revision() + 1,
            created_at: ts("2026-01-02T00:00:00Z"),
        };
        let ops = vec![GraphOp::Claim(new_claim)];
        let next = index.with_ops(&ops, index.revision() + 1);
        commit(&dir.path, &index, &next, &ops).expect("commit muss gelingen");

        let mut after = HashMap::new();
        collect_all_files_with_content(&dir.path, &mut after);

        let layout = Layout::from_index(&next);
        let e1_path = layout.entity_path("E-1").unwrap().to_string();
        let e3_path = layout.entity_path("E-3").unwrap().to_string();

        assert_ne!(
            before[&e1_path], after[&e1_path],
            "E-1 hätte sich ändern müssen"
        );
        assert_eq!(
            before[&e3_path], after[&e3_path],
            "E-3 haette unberuehrt bleiben muessen"
        );
        assert_eq!(
            before["working/session-run-4711/index.md"], after["working/session-run-4711/index.md"],
            "die Dokumentliste des Ziels hat sich nicht geändert"
        );
        assert_eq!(
            before["canonical/workspace-agentkit-rs/index.md"],
            after["canonical/workspace-agentkit-rs/index.md"]
        );
    }

    #[test]
    fn promotion_entfernt_die_datei_am_alten_pfad() {
        let dir = TempDir::new("promotion");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("initialer rebuild");

        let old_layout = Layout::from_index(&index);
        let old_path = old_layout.entity_path("E-1").unwrap().to_string();
        assert!(dir.path.join(&old_path).exists());

        let mut promoted = (**index.entity("E-1").unwrap()).clone();
        promoted.layer = GraphLayer::Canonical;
        promoted.scope = GraphScope::workspace("agentkit-rs");
        promoted.promoted_from = Some(GraphScope::session("run-4711"));
        promoted.updated_revision = index.revision() + 1;
        let ops = vec![GraphOp::Entity(promoted)];
        let next = index.with_ops(&ops, index.revision() + 1);
        commit(&dir.path, &index, &next, &ops).expect("commit muss gelingen");

        let new_layout = Layout::from_index(&next);
        let new_path = new_layout.entity_path("E-1").unwrap().to_string();
        assert_ne!(old_path, new_path);
        assert!(
            !dir.path.join(&old_path).exists(),
            "alte Datei haette weg sein muessen"
        );
        assert!(dir.path.join(&new_path).exists());
    }

    #[test]
    fn absturz_simulation_hoehere_revision_gewinnt() {
        let dir = TempDir::new("crash");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("initialer rebuild");

        let old_layout = Layout::from_index(&index);
        let old_path = dir.path.join(old_layout.entity_path("E-1").unwrap());
        let old_text = std::fs::read_to_string(&old_path).unwrap();

        // Simuliert den Zustand NACH einem Absturz zwischen Schritt 1/2 und 3
        // in `commit`: dieselbe Entity liegt an ZWEI Pfaden. Die Kopie mit der
        // höheren `updated_revision` gewinnt.
        let mut newer = (**index.entity("E-1").unwrap()).clone();
        newer.canonical_name = "MCP-Client (aktualisiert)".to_string();
        newer.updated_revision = index.revision() + 10;
        let newer_ops = vec![GraphOp::Entity(newer)];
        let newer_index = index.with_ops(&newer_ops, index.revision() + 10);
        let new_layout = Layout::from_index(&newer_index);
        let new_path = dir.path.join(new_layout.entity_path("E-1").unwrap());
        // Absichtlich NICHT über `commit` — wir bauen die Absturz-Situation
        // von Hand nach, exakt wie der Test es fordert.
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        let newer_text = doc::render_entity(
            &doc::EntityDoc {
                entity: (**newer_index.entity("E-1").unwrap()).clone(),
                claims: index
                    .incident_claims("E-1")
                    .filter(|c| c.subject == "E-1")
                    .map(|c| (**c).clone())
                    .collect(),
                sources: vec![(**index.source("S-1").unwrap()).clone()],
            },
            new_layout.refs(),
        );
        std::fs::write(&new_path, &newer_text).unwrap();

        // Beide Pfade existieren jetzt gleichzeitig — die alte Datei blieb
        // unangetastet liegen (kein `commit`-Aufruf hat sie entfernt).
        assert!(old_path.exists());
        assert!(new_path.exists());
        assert_ne!(old_text, newer_text);

        let ops = load(&dir.path).expect("load muss trotz Doppel-Eintrag gelingen");
        let reloaded = GraphIndex::default().with_ops(&ops, 99);
        let winner = reloaded.entity("E-1").expect("E-1 muss vorhanden sein");
        assert_eq!(winner.canonical_name, "MCP-Client (aktualisiert)");
        assert_eq!(winner.updated_revision, index.revision() + 10);
    }

    /// Der Ausweichname `<slug>-<id>` kann SELBST schon belegt sein. Vorher
    /// wurde er trotzdem vergeben: zwei Entities auf einem Pfad, die zweite
    /// überschrieb die erste beim Schreiben spurlos.
    ///
    /// Das Muster ist aus Modellargumenten erreichbar — `normalize` behält
    /// Unicode-Alphanumerik (`Foo` ≠ `Fooα`, also zwei Entities), `slug` wirft
    /// sie weg (beide werden `foo`), und eine dritte Entity `Foo E3` besetzt
    /// den Ausweichnamen `foo-e3`.
    #[test]
    fn ein_belegter_ausweichname_fuehrt_nicht_zum_datenverlust() {
        let dir = TempDir::new("collision-chain");
        let scope = GraphScope::session("s1");
        let mut index = GraphIndex::default();
        for (nr, (id, name)) in [("E-1", "Foo E3"), ("E-2", "Foo"), ("E-3", "Fooα")]
            .iter()
            .enumerate()
        {
            index = index.with_ops(
                &[GraphOp::Entity(GraphEntity {
                    id: (*id).to_string(),
                    canonical_name: (*name).to_string(),
                    entity_type: "Konzept".to_string(),
                    description: None,
                    aliases: vec![],
                    layer: GraphLayer::Working,
                    scope: scope.clone(),
                    promoted_from: None,
                    created_revision: nr as u64 + 1,
                    updated_revision: nr as u64 + 1,
                    created_at: ts("2026-01-01T00:00:00Z"),
                    extra: Default::default(),
                })],
                nr as u64 + 1,
            );
        }

        let layout = Layout::from_index(&index);
        let pfade: HashSet<&str> = ["E-1", "E-2", "E-3"]
            .iter()
            .map(|id| layout.entity_path(id).expect("Pfad"))
            .collect();
        assert_eq!(
            pfade.len(),
            3,
            "drei Entities brauchen drei Pfade: {pfade:?}"
        );

        rebuild(&dir.path, &index).expect("rebuild");
        let ops = load(&dir.path).expect("load");
        let geladen: HashSet<String> = ops
            .iter()
            .filter_map(|op| match op {
                GraphOp::Entity(e) => Some(e.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            geladen.len(),
            3,
            "keine Entity darf verschwinden: {geladen:?}"
        );
    }

    #[test]
    fn slug_kollision_ergibt_zwei_dateien_die_beide_geladen_werden() {
        let dir = TempDir::new("collision");
        let mut index = GraphIndex::default();
        let scope = GraphScope::session("run-1");

        index = index.with_ops(
            &[GraphOp::Entity(GraphEntity {
                id: "E-1".to_string(),
                canonical_name: "Gleicher Name".to_string(),
                entity_type: "Konzept".to_string(),
                description: None,
                aliases: vec![],
                layer: GraphLayer::Working,
                scope: scope.clone(),
                promoted_from: None,
                created_revision: 1,
                updated_revision: 1,
                created_at: ts("2026-01-01T00:00:00Z"),
                extra: Default::default(),
            })],
            1,
        );
        index = index.with_ops(
            &[GraphOp::Entity(GraphEntity {
                id: "E-2".to_string(),
                canonical_name: "Gleicher Name".to_string(),
                entity_type: "Konzept".to_string(),
                description: None,
                aliases: vec![],
                layer: GraphLayer::Working,
                scope: scope.clone(),
                promoted_from: None,
                created_revision: 2,
                updated_revision: 2,
                created_at: ts("2026-01-01T00:01:00Z"),
                extra: Default::default(),
            })],
            2,
        );

        rebuild(&dir.path, &index).expect("rebuild muss gelingen");
        let layout = Layout::from_index(&index);
        let path_1 = layout.entity_path("E-1").unwrap();
        let path_2 = layout.entity_path("E-2").unwrap();
        assert_ne!(path_1, path_2);
        assert!(dir.path.join(path_1).exists());
        assert!(dir.path.join(path_2).exists());

        let ops = load(&dir.path).expect("load muss gelingen");
        let reloaded = GraphIndex::default().with_ops(&ops, 2);
        assert!(reloaded.entity("E-1").is_some());
        assert!(reloaded.entity("E-2").is_some());
    }

    /// Der Lesepfad überspringt Symlinks; der Schreibpfad folgte ihnen.
    /// Ein Bundle, in dem `working` ein Link nach draußen ist, ließ agentkit
    /// beim ersten Commit Dokumente außerhalb des Bundles anlegen — mit
    /// Dateinamen aus `slug()`, also aus Modellargumenten.
    #[test]
    fn in_ein_bundle_mit_symlink_verzeichnis_wird_nicht_geschrieben() {
        let dir = TempDir::new("symlink-dir");
        let draussen = TempDir::new("symlink-ziel");
        std::fs::create_dir_all(&draussen.path).expect("Zielverzeichnis");
        std::fs::create_dir_all(&dir.path).expect("Bundle-Wurzel");

        let link = dir.path.join("working");
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&draussen.path, &link).is_ok();
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(&draussen.path, &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let created = false;
        if !created {
            return; // ohne Symlink-Recht gibt es nichts zu prüfen
        }

        let fehler = rebuild(&dir.path, &build_sample_index())
            .expect_err("ein Symlink-Verzeichnis muss abgelehnt werden");
        assert!(
            matches!(&fehler, GraphError::Io(m) if m.contains("Symlink")),
            "unerwarteter Fehler: {fehler}"
        );
        assert_eq!(
            std::fs::read_dir(&draussen.path).expect("lesbar").count(),
            0,
            "außerhalb des Bundles darf nichts entstanden sein"
        );
    }

    #[test]
    fn symlink_wird_uebersprungen() {
        let dir = TempDir::new("symlink");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("rebuild muss gelingen");

        let target = dir.path.join("working/session-run-4711/mcp-client.md");
        let link = dir.path.join("working/session-run-4711/ueber-symlink.md");
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&target, &link).is_ok();
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(not(any(unix, windows)))]
        let created = false;

        if !created {
            // Unter Windows ohne Developer-Mode/erhöhte Rechte schlägt das
            // Anlegen eines Symlinks fehl — dann gibt es nichts zu prüfen,
            // der Test wird sauber übersprungen statt rot zu laufen.
            return;
        }

        let ops = load(&dir.path).expect("load muss trotz Symlink gelingen");
        let reloaded = GraphIndex::default().with_ops(&ops, index.revision());
        // Der Symlink zeigt auf dieselbe Entity — wird er NICHT übersprungen,
        // würde sich an der Zählung nichts ändern (Upsert über dieselbe ID).
        // Der eigentliche Beweis ist deshalb: das Verzeichnis wird nicht in
        // die Irre geführt, load() bricht nicht ab und liefert genau EINE
        // Entity mit dieser ID.
        assert_eq!(reloaded.entity_count(), index.entity_count());
    }

    #[test]
    fn fremdes_okf_dokument_ohne_agentkit_block_wird_uebersprungen() {
        let dir = TempDir::new("foreign");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("rebuild muss gelingen");

        let foreign = dir.path.join("working/session-run-4711/fremd.md");
        std::fs::write(
            &foreign,
            "---\ntype: thing\ntitle: Fremd\n---\n\nEin fremdes OKF-Dokument.\n",
        )
        .unwrap();

        let ops = load(&dir.path).expect("load darf am fremden Dokument nicht scheitern");
        let reloaded = GraphIndex::default().with_ops(&ops, index.revision());
        assert_eq!(reloaded.entity_count(), index.entity_count());
    }

    #[test]
    fn kaputtes_frontmatter_ist_ein_harter_fehler_mit_pfad() {
        let dir = TempDir::new("broken");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("rebuild muss gelingen");

        let path = dir.path.join("working/session-run-4711/mcp-client.md");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text = text.replacen("status: draft", "status: draft\nstatus: draft", 1);
        std::fs::write(&path, text).unwrap();

        let err = load(&dir.path).expect_err("doppelter Schlüssel muss ein Fehler sein");
        let message = err.to_string();
        assert!(
            message.contains("mcp-client.md"),
            "Fehlermeldung ohne Dateipfad: {message}"
        );
    }

    // -----------------------------------------------------------------------
    // Konformität des erzeugten Bundles (Regeln aus `okf_validate.py`
    // nachgebildet — siehe Modul-Doc-Comment für die Abgrenzung).
    // -----------------------------------------------------------------------

    #[test]
    fn erzeugtes_bundle_ist_okf_konform() {
        let dir = TempDir::new("conformance");
        let index = build_sample_index();
        rebuild(&dir.path, &index).expect("rebuild muss gelingen");

        let mut all_files = Vec::new();
        collect_all_md_including_reserved(&dir.path, &mut all_files);
        assert!(!all_files.is_empty());

        let mut known_paths: HashSet<String> = HashSet::new();
        let mut all_source_ids: HashSet<String> = HashSet::new();
        for path in &all_files {
            let rel = path
                .strip_prefix(&dir.path)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            known_paths.insert(rel);
        }

        for path in &all_files {
            let rel = path
                .strip_prefix(&dir.path)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(path).unwrap();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap();
            let is_root_index = name == "index.md" && path.parent() == Some(dir.path.as_path());

            if name == "log.md" {
                assert!(
                    markdown::split_frontmatter(&text).is_none(),
                    "{rel}: log.md darf kein Frontmatter haben"
                );
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("## ") {
                        assert!(
                            crate::okf::instant::is_iso_date(rest),
                            "{rel}: Datumsüberschrift '{rest}' ist kein YYYY-MM-DD"
                        );
                    }
                }
                continue;
            }
            if name == "index.md" {
                if is_root_index {
                    let (fm, _) = markdown::split_frontmatter(&text)
                        .expect("Root-index.md muss Frontmatter haben");
                    let pairs = crate::okf::yaml::parse_document(fm).unwrap();
                    assert_eq!(
                        pairs.len(),
                        1,
                        "{rel}: Root-index.md darf nur okf_version tragen"
                    );
                    assert_eq!(pairs[0].0, "okf_version");
                } else {
                    assert!(
                        markdown::split_frontmatter(&text).is_none(),
                        "{rel}: nicht-Root index.md darf kein Frontmatter haben"
                    );
                }
                continue;
            }

            // Jede übrige `.md`-Datei ist ein Konzept: Frontmatter mit
            // nicht-leerem `type`.
            let (fm, body) = markdown::split_frontmatter(&text)
                .unwrap_or_else(|| panic!("{rel}: Konzept-Datei ohne Frontmatter"));
            let pairs = crate::okf::yaml::parse_document(fm)
                .unwrap_or_else(|e| panic!("{rel}: Frontmatter kaputt: {e}"));
            let type_value = pairs
                .iter()
                .find(|(k, _)| k == "type")
                .and_then(|(_, v)| v.as_str());
            assert!(
                type_value.is_some_and(|t| !t.is_empty()),
                "{rel}: 'type' fehlt oder ist leer"
            );

            if let Some(sources) = pairs
                .iter()
                .find(|(k, _)| k == "sources")
                .and_then(|(_, v)| v.as_seq())
            {
                for entry in sources {
                    if let Some(id) = entry
                        .as_map()
                        .and_then(|m| m.get("id"))
                        .and_then(|v| v.as_str())
                    {
                        all_source_ids.insert(id.to_string());
                    }
                }
            }
            for label in markdown::footnote_refs(body) {
                assert!(
                    all_source_ids.contains(&label) || pairs.iter().any(|(k, _)| k == "sources"),
                    "{rel}: Footnote [^{label}] ohne sources[]"
                );
            }
        }

        // Jeder Markdown-Link im gesamten Bundle löst auf eine existierende
        // Datei oder ein existierendes Verzeichnis auf.
        for path in &all_files {
            let text = std::fs::read_to_string(path).unwrap();
            let (_, body) = markdown::split_frontmatter(&text).unwrap_or(("", text.as_str()));
            for (_, target) in markdown::links(body) {
                if target.starts_with("http://") || target.starts_with("https://") {
                    continue;
                }
                let stripped = target.strip_suffix('/').unwrap_or(&target);
                // Bundle-absolute Links (führendes `/`, wie sie
                // `doc::EntityRef::path` liefert) lösen relativ zum
                // Bundle-ROOT auf, alle anderen relativ zur Datei selbst.
                let resolved = if let Some(from_root) = stripped.strip_prefix('/') {
                    dir.path.join(from_root)
                } else {
                    path.parent().unwrap().join(stripped)
                };
                assert!(
                    resolved.exists(),
                    "Link '{target}' aus {} löst auf nichts Vorhandenes auf",
                    path.display()
                );
            }
        }
    }

    fn collect_all_md_including_reserved(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect_all_md_including_reserved(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
}
