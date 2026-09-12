//! Ende-zu-Ende: was der ECHTE Schreibpfad auf die Platte legt, ist ein
//! konformes OKF-v0.2-Bundle.
//!
//! Warum neben den Modultests in `src/okf/bundle.rs`? Die bauen einen
//! `GraphIndex` von Hand. Hier läuft stattdessen der Weg, den ein Agent
//! wirklich geht — `GraphStore::submit` mit `RecordClaim`, `RecordEpisode`,
//! `PromoteClaim` —, damit auch die Zusammensetzung der Ops geprüft ist und
//! nicht nur ihre Ablage.
//!
//! Die Regeln unten sind die des Referenz-Validators
//! (`okf-skills/skills/validate/scripts/okf_validate.py`), nachgebildet in
//! Rust. Sie sind **keine** Auswahl der drei harten Fehlerklassen, sondern
//! decken die Warnungen mit ab: Ziel ist ein Bundle, das `--strict` besteht.
//! Die Gegenprobe gegen die Referenz selbst braucht `uv` und Python und ist
//! deshalb bewusst kein `cargo test` — sie steht als manueller Schritt in
//! `agentkit_graph/README.md`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use agentkit_graph::okf::{instant, markdown, yaml};
use agentkit_graph::*;

/// Temporäres Verzeichnis ohne Dev-Dependency, das sich selbst aufräumt.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "agentkit-okf-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Ein Graph, wie ihn ein Lauf hinterlässt: Beobachtung, Hypothese, Episode,
/// eine Promotion — und damit Dokumente in beiden Ebenen.
fn baue_realistischen_graphen(dir: &Path) {
    let store = GraphStore::open(dir).expect("Store muss sich öffnen lassen");
    let access = GraphAccess::session("tester", "agentkit-rs", "run-4711");

    let receipt = store
        .submit(
            GraphWriteCommand::RecordClaim(
                ClaimDraft::new(
                    "Parallele Tool-Aufrufe",
                    "verursacht",
                    "Session-Konkurrenz",
                    SourceDraft::new("test_run")
                        .excerpt("cargo test mcp:: — 2 Fehlschläge")
                        .tool_call("call_7"),
                )
                .status(ClaimStatus::Observation)
                .confidence(0.82),
            ),
            &access,
        )
        .expect("Beobachtung muss schreibbar sein");

    store
        .submit(
            GraphWriteCommand::RecordClaim(
                ClaimDraft::new(
                    "MCP-Client (stdio)",
                    "nutzt",
                    "Session-Mutex",
                    SourceDraft::new("document").artifact("https://example.invalid/mcp"),
                )
                .status(ClaimStatus::Hypothesis)
                .confidence(0.4),
            ),
            &access,
        )
        .expect("Hypothese muss schreibbar sein");

    store
        .submit(
            GraphWriteCommand::RecordEpisode(EpisodeDraft::new(
                "Testlauf gestartet, zwei Fehlschläge in mcp:: beobachtet.",
                SourceDraft::new("agent_turn"),
            )),
            &access,
        )
        .expect("Episode muss schreibbar sein");

    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: receipt.claim_id.expect("Claim-ID"),
            },
            &access,
        )
        .expect("Promotion muss gehen");
}

fn alle_markdown_dateien(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stapel = vec![root.to_path_buf()];
    while let Some(dir) = stapel.pop() {
        let Ok(eintraege) = std::fs::read_dir(&dir) else {
            continue;
        };
        for eintrag in eintraege.flatten() {
            let pfad = eintrag.path();
            if pfad.is_dir() {
                stapel.push(pfad);
            } else if pfad.extension().is_some_and(|e| e == "md") {
                out.push(pfad);
            }
        }
    }
    out.sort();
    out
}

fn relativ(root: &Path, pfad: &Path) -> String {
    pfad.strip_prefix(root)
        .unwrap_or(pfad)
        .to_string_lossy()
        .replace('\\', "/")
}

/// `^(?:[^\s:/]+:\S+|\S+/\S+)$` des Referenz-Validators, plus dessen
/// Beinahe-Treffer-Warnung auf `human`/`process`.
fn ist_gueltiger_actor(text: &str) -> bool {
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return false;
    }
    let lower = text.to_lowercase();
    if (lower.starts_with("human") && !text.starts_with("human:"))
        || (lower.starts_with("process") && !text.starts_with("process:"))
    {
        return false;
    }
    match text.find(':') {
        Some(i) if i > 0 && !text[..i].contains('/') && i + 1 < text.len() => return true,
        _ => {}
    }
    matches!(text.find('/'), Some(i) if i > 0 && i + 1 < text.len())
}

#[test]
fn der_echte_schreibpfad_erzeugt_ein_strict_konformes_bundle() {
    let dir = TempDir::new("konform");
    baue_realistischen_graphen(&dir.path);
    let root = &dir.path;

    let dateien = alle_markdown_dateien(root);
    assert!(
        dateien.len() >= 5,
        "zu wenige Dokumente entstanden: {dateien:?}"
    );

    // Sammelt jedes Link-Ziel des ganzen Bundles für die Auflösungsprüfung.
    let mut links: Vec<(String, String)> = Vec::new();
    // Mitgezählt, weil die Schleife `index.md`/`log.md` per `continue`
    // überspringt: ohne diese Zahl könnte der Test grün sein, ohne je eine
    // Konzept-Assertion ausgeführt zu haben.
    let mut geprueft = (0usize, 0usize, 0usize); // Konzepte, Indizes, Logs
    let mut fussnoten = 0usize;

    for pfad in &dateien {
        let rel = relativ(root, pfad);
        let text = std::fs::read_to_string(pfad).expect("lesbar");
        let name = pfad.file_name().unwrap().to_string_lossy().to_string();
        let frontmatter = markdown::split_frontmatter(&text);

        for (_, ziel) in markdown::links(frontmatter.map_or(text.as_str(), |(_, body)| body)) {
            links.push((rel.clone(), ziel));
        }

        // --- §8/§9: reservierte Dateinamen -------------------------------
        if name == "index.md" {
            geprueft.1 += 1;
            let ist_wurzel = rel == "index.md";
            match (ist_wurzel, frontmatter) {
                (false, Some(_)) => panic!("{rel}: index.md außerhalb der Wurzel hat Frontmatter"),
                (true, Some((fm, _))) => {
                    let pairs = yaml::parse_document(fm).expect("Wurzel-Frontmatter parst");
                    let schluessel: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
                    assert_eq!(
                        schluessel,
                        vec!["okf_version"],
                        "{rel}: Wurzel-index.md darf nur okf_version tragen"
                    );
                    assert_eq!(
                        pairs[0].1.as_str(),
                        Some("0.2"),
                        "{rel}: falsche okf_version"
                    );
                }
                _ => {}
            }
            continue;
        }
        if name == "log.md" {
            geprueft.2 += 1;
            assert!(
                frontmatter.is_none(),
                "{rel}: log.md darf kein Frontmatter haben"
            );
            for zeile in text.lines().filter(|z| z.starts_with("## ")) {
                assert!(
                    instant::is_iso_date(zeile[3..].trim()),
                    "{rel}: Datums-Überschrift '{zeile}' ist kein ISO-8601-Datum"
                );
            }
            continue;
        }

        // --- §11: die drei harten Fehlerklassen --------------------------
        geprueft.0 += 1;
        let (fm, body) = frontmatter.unwrap_or_else(|| panic!("{rel}: kein Frontmatter"));
        let pairs = yaml::parse_document(fm)
            .unwrap_or_else(|e| panic!("{rel}: Frontmatter parst nicht: {e}"));
        let hole = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        assert!(
            hole("type")
                .and_then(yaml::YamlValue::as_str)
                .is_some_and(|t| !t.is_empty()),
            "{rel}: 'type' fehlt oder ist leer"
        );

        // --- §4.1: empfohlene Felder (sonst Warnung ⇒ --strict scheitert) -
        for key in ["title", "description", "tags"] {
            assert!(hole(key).is_some(), "{rel}: empfohlenes Feld '{key}' fehlt");
        }

        // --- §5.4: Lifecycle ---------------------------------------------
        if let Some(status) = hole("status").and_then(yaml::YamlValue::as_str) {
            assert!(
                ["draft", "stable", "deprecated"].contains(&status),
                "{rel}: unbekannter status '{status}'"
            );
        }

        // --- §5.2/§7: generated + Actor-Konvention + RFC3339 --------------
        let generated = hole("generated").unwrap_or_else(|| panic!("{rel}: 'generated' fehlt"));
        let by = generated
            .get("by")
            .and_then(yaml::YamlValue::as_str)
            .unwrap_or_else(|| panic!("{rel}: 'generated.by' fehlt"));
        assert!(ist_gueltiger_actor(by), "{rel}: 'generated.by' = '{by}'");
        let at = generated
            .get("at")
            .and_then(yaml::YamlValue::as_str)
            .unwrap_or_else(|| panic!("{rel}: 'generated.at' fehlt"));
        assert!(
            instant::from_rfc3339(at).is_some(),
            "{rel}: 'generated.at' = '{at}' ist kein RFC3339"
        );

        if let Some(verified) = hole("verified").and_then(yaml::YamlValue::as_seq) {
            for eintrag in verified {
                let by = eintrag.get("by").and_then(yaml::YamlValue::as_str);
                assert!(
                    by.is_some_and(ist_gueltiger_actor),
                    "{rel}: 'verified[].by' = {by:?}"
                );
                let at = eintrag.get("at").and_then(yaml::YamlValue::as_str);
                assert!(
                    at.and_then(instant::from_rfc3339).is_some(),
                    "{rel}: 'verified[].at' = {at:?}"
                );
            }
        }

        // --- §5.1: sources[].resource ist Pflicht, Footnoten müssen treffen
        let mut source_ids: HashSet<String> = HashSet::new();
        if let Some(sources) = hole("sources").and_then(yaml::YamlValue::as_seq) {
            for eintrag in sources {
                let resource = eintrag.get("resource").and_then(yaml::YamlValue::as_str);
                assert!(
                    resource.is_some_and(|r| !r.trim().is_empty()),
                    "{rel}: sources[] ohne 'resource'"
                );
                if let Some(id) = eintrag.get("id").and_then(yaml::YamlValue::as_str) {
                    source_ids.insert(id.to_string());
                }
                if let Some(author) = eintrag.get("author").and_then(yaml::YamlValue::as_str) {
                    assert!(
                        ist_gueltiger_actor(author),
                        "{rel}: sources[].author '{author}'"
                    );
                }
            }
        }
        for label in markdown::footnote_refs(body) {
            fussnoten += 1;
            assert!(
                source_ids.contains(&label),
                "{rel}: Fußnote '[^{label}]' trifft kein sources[].id"
            );
        }
    }

    // Beweist, dass oben wirklich geprüft wurde und nicht alles am `continue`
    // vorbeilief: 4 Entities + 1 Episode, Wurzel- und Ebenen-Indizes, ein
    // Protokoll, und Fußnoten hat es auch gegeben.
    assert!(
        geprueft.0 >= 5,
        "zu wenige Konzept-Dokumente geprüft: {geprueft:?}"
    );
    assert!(geprueft.1 >= 4, "zu wenige index.md geprüft: {geprueft:?}");
    assert_eq!(geprueft.2, 1, "genau ein log.md erwartet: {geprueft:?}");
    // Zwei, nicht drei: die beiden Subjekt-Dokumente tragen je eine Referenz
    // im Fließtext. Das Episoden-Dokument hat nur eine Fußnoten-DEFINITION
    // und keine Referenz — `footnote_refs` zählt Definitionszeilen bewusst
    // nicht mit.
    assert!(fussnoten >= 2, "zu wenige Fußnoten geprüft: {fussnoten}");
    assert!(!links.is_empty(), "kein einziger Link im Bundle");

    // --- §6.1: jeder Link löst auf -------------------------------------
    for (quelle, ziel) in links {
        if ziel.starts_with("http://") || ziel.starts_with("https://") || ziel.ends_with('/') {
            continue;
        }
        let ohne_anker = ziel.split('#').next().unwrap_or(&ziel);
        let aufgeloest = match ohne_anker.strip_prefix('/') {
            Some(rest) => root.join(rest),
            None => root
                .join(&quelle)
                .parent()
                .expect("Dokument hat ein Verzeichnis")
                .join(ohne_anker),
        };
        assert!(
            aufgeloest.exists(),
            "{quelle}: Link '{ziel}' zeigt auf nichts ({})",
            aufgeloest.display()
        );
    }
}

/// Legt ein Bundle unter `target/okf-demo` ab — die Vorlage für die manuelle
/// Gegenprobe gegen die Referenz-Implementierung (siehe README, „Build &
/// Test"). Absichtlich `#[ignore]`: der Test schreibt außerhalb des
/// Temp-Verzeichnisses und gehört damit nicht in einen gewöhnlichen Lauf.
///
/// ```text
/// cargo test --manifest-path agentkit_graph/Cargo.toml --test okf_konformitaet -- --ignored
/// uv run --with pyyaml python <okf-skills>/skills/validate/scripts/okf_validate.py \
///     agentkit_graph/target/okf-demo --strict
/// ```
#[test]
#[ignore = "schreibt nach target/okf-demo; nur für die manuelle Validator-Probe"]
fn demo_bundle_fuer_die_referenz_pruefung() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("okf-demo");
    let _ = std::fs::remove_dir_all(&dir);
    baue_realistischen_graphen(&dir);
    println!("Bundle liegt in {}", dir.display());
}

#[test]
fn ein_neustart_liest_denselben_stand_wieder_ein() {
    let dir = TempDir::new("neustart");
    baue_realistischen_graphen(&dir.path);

    let vorher = GraphStore::open(&dir.path).expect("erstes Öffnen").stats();
    let nachher = GraphStore::open(&dir.path).expect("zweites Öffnen").stats();
    assert_eq!(vorher, nachher, "Öffnen ist nicht idempotent");
    assert!(vorher.claims >= 2 && vorher.episodes == 1 && vorher.entities >= 4);
}

/// Ein Verzeichnis aus der Zeit vor der Umstellung. Das alte Format kennt
/// weder `verified` noch `extra`, und die Autoren sind blanke Zeichenketten
/// ohne Actor-Präfix — beides muss beim Import ohne Zutun heilen.
#[test]
fn ein_legacy_journal_wird_beim_oeffnen_zu_einem_konformen_bundle() {
    let dir = TempDir::new("migration");
    std::fs::create_dir_all(&dir.path).expect("Verzeichnis");
    let zeilen = [
        r#"{"schema_version":"1","revision":1,"at":1690000000000,"op":{"record":"source","id":"S-1","source_type":"tool_result","agent_id":"tester","run_id":"run-1","content_hash":"abc123","created_revision":1,"created_at":1690000000000}}"#,
        r#"{"schema_version":"1","revision":1,"at":1690000000000,"op":{"record":"entity","id":"E-1","canonical_name":"Alter Knoten","entity_type":"thing","aliases":["alter knoten"],"layer":"working","scope":{"kind":"session","id":"alt"},"created_revision":1,"updated_revision":1,"created_at":1690000000000}}"#,
        r#"{"schema_version":"1","revision":1,"at":1690000000000,"op":{"record":"entity","id":"E-2","canonical_name":"Ziel","entity_type":"thing","aliases":["ziel"],"layer":"working","scope":{"kind":"session","id":"alt"},"created_revision":1,"updated_revision":1,"created_at":1690000000000}}"#,
        r#"{"schema_version":"1","revision":2,"at":1690000000000,"op":{"record":"claim","id":"C-1","subject":"E-1","predicate":"zeigt auf","object":"E-2","layer":"working","scope":{"kind":"session","id":"alt"},"status":"observation","confidence":0.7,"source_ids":["S-1"],"created_by":"tester","created_revision":2,"updated_revision":2,"created_at":1690000000000}}"#,
    ];
    std::fs::write(dir.path.join(JOURNAL_FILE), zeilen.join("\n") + "\n").expect("Journal");

    let store = GraphStore::open(&dir.path).expect("Migration");
    let index = store.snapshot();
    assert_eq!(index.claim_count(), 1, "Aussage muss überleben");
    assert_eq!(index.entity_count(), 2);

    let claim = index.claim("C-1").expect("C-1");
    assert_eq!(
        claim.created_by.as_str(),
        "agent:tester",
        "blanker Legacy-Principal muss zur Actor-Konvention normalisiert werden"
    );
    assert_eq!(claim.layer, GraphLayer::Working, "Ebene bleibt vorläufig");

    assert!(
        dir.path.join(BUNDLE_INDEX).exists(),
        "Bundle muss angelegt sein"
    );
    assert!(
        !dir.path.join(JOURNAL_FILE).exists(),
        "Journal darf nicht liegen bleiben"
    );
    assert!(
        dir.path.join(MIGRATED_JOURNAL).exists(),
        "Journal muss als .migriert erhalten bleiben — kein Datenverlust"
    );

    // Kein zweiter Durchlauf: das Bundle gewinnt, das umbenannte Journal bleibt.
    let erneut = GraphStore::open(&dir.path).expect("zweites Öffnen");
    assert_eq!(erneut.stats(), store.stats());
    assert!(dir.path.join(MIGRATED_JOURNAL).exists());
}
