//! Die Work-Tool-Schicht als Spezifikation: was das Modell sieht, darf es
//! auch — und nicht mehr. Teststil wie `agentkit_graph/tests/tools.rs`.

use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex};

use agentkit::ToolRegistry;
use agentkit_work::{register_work_tools, ClaimText, GraphGateway, WorkProvenance};
use agentkit_work::{WorkStore, WorkToolCtx};
use serde_json::json;

/// Test-Doppelgänger für [`GraphGateway`] (zweite Implementierung neben dem
/// Adapter in `agentkit_app` — Guidelines §2): liefert einen festen
/// Recall-Text und hält jeden `record_claims`-Aufruf samt Provenance zum
/// Nachprüfen fest.
#[derive(Default)]
struct FakeGraph {
    recall_text: Option<String>,
    recorded: Mutex<Vec<(WorkProvenance, Vec<ClaimText>)>>,
    next_id: std::sync::atomic::AtomicUsize,
}

impl FakeGraph {
    fn new() -> Self {
        FakeGraph {
            next_id: std::sync::atomic::AtomicUsize::new(1),
            ..Default::default()
        }
    }
}

impl GraphGateway for FakeGraph {
    fn recall(&self, _query: &str) -> Option<String> {
        self.recall_text.clone()
    }

    fn record_claims(
        &self,
        prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        let ids: Vec<String> = claims
            .iter()
            .map(|_| {
                let n = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                format!("C-{n}")
            })
            .collect();
        self.recorded
            .lock()
            .unwrap()
            .push((prov.clone(), claims.to_vec()));
        Ok(ids)
    }

    fn promote(&self, claim_ids: &[String]) -> Result<usize, String> {
        Ok(claim_ids.len())
    }
}

/// Laufende Nummer im Verzeichnisnamen: die Tests laufen parallel, und zwei
/// Namen dürfen nie kollidieren. Der Name allein reicht dafür nicht — die
/// Pfadausbruch-Tests leiten ihn aus dem Dateinamen ab, und `../boese.txt` und
/// `..\boese.txt` ergeben denselben bereinigten Namen.
static TMP_NR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp_dir(name: &str) -> PathBuf {
    let nr = TMP_NR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_tools_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Baut Registry + Store + Kontext über einem frischen Temp-Verzeichnis.
/// `artifacts_dir` liegt unter dem Projektverzeichnis, wie in `WorkToolCtx`
/// dokumentiert.
fn registry(dir: &std::path::Path) -> (Arc<WorkStore>, ToolRegistry, WorkToolCtx) {
    registry_with_gateway(dir, None)
}

/// Wie [`registry`], aber mit einem wählbaren `GraphGateway` — für die
/// `work_claim`-Tests, die nur MIT Gateway existiert.
fn registry_with_gateway(
    dir: &std::path::Path,
    gateway: Option<Arc<dyn agentkit_work::GraphGateway>>,
) -> (Arc<WorkStore>, ToolRegistry, WorkToolCtx) {
    let store = Arc::new(WorkStore::open(dir).unwrap());
    let ctx = WorkToolCtx {
        run_id: "R-1".to_string(),
        work_item_id: "W-1".to_string(),
        attempt_id: "A-1".to_string(),
        agent_id: "agent-1".to_string(),
        max_attempts: 3,
        project_id: "P-1".to_string(),
        repository_revision: None,
        artifacts_dir: dir.join("artifacts"),
        submission: Arc::new(Mutex::new(None)),
        gateway,
        verifies: None,
    };
    let mut tools = ToolRegistry::new();
    register_work_tools(&mut tools, store.clone(), ctx.clone());
    (store, tools, ctx)
}

#[test]
fn alle_drei_tools_sind_nach_dem_registrieren_da() {
    let dir = tmp_dir("registry");
    let (_, tools, _) = registry(&dir);
    assert!(tools.has("work_add_item"));
    assert!(tools.has("work_artifact"));
    assert!(tools.has("work_submit"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn kein_tool_hat_ein_identitaets_argument() {
    let dir = tmp_dir("keine_identitaet");
    // MIT Gateway, damit auch 'work_claim' in der Prüfung steckt — sonst
    // würde ein Provenance-Argument an genau diesem Tool unbemerkt bleiben.
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (_, tools, _) = registry_with_gateway(&dir, Some(gateway));
    assert!(tools.has("work_claim"), "Vorbedingung: Gateway gesetzt");
    // Strukturelle Prüfung statt Textsuche: nur die tatsächlichen Argument-
    // Namen zählen (`function.parameters.properties`), nicht z. B. ein Wort,
    // das zufällig auch in einer Beschreibung vorkommt.
    for schema in tools.schemas().unwrap() {
        let properties = &schema["function"]["parameters"]["properties"];
        let keys: Vec<&String> = properties
            .as_object()
            .expect("parameters.properties ist ein Objekt")
            .keys()
            .collect();
        for verboten in [
            "run_id",
            "work_item_id",
            "attempt_id",
            "agent_id",
            "project_id",
            "repository_revision",
        ] {
            assert!(
                !keys.iter().any(|k| k.as_str() == verboten),
                "'{verboten}' darf kein Argument von '{}' sein: {keys:?}",
                schema["function"]["name"]
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn alle_erlaubten_item_kinds_werden_akzeptiert() {
    // Regressionsschutz gegen Drift zwischen dem Schema-`enum` (ITEM_KINDS)
    // und dem, was `WorkItemKind` per `serde(rename_all)` tatsächlich
    // deserialisiert — sonst bewirbt das Schema einen Wert, den das Tool
    // hinterher als unbekannt ablehnt.
    let dir = tmp_dir("alle_item_kinds");
    let (_, tools, _) = registry(&dir);
    for kind in [
        "discovery",
        "analysis",
        "planning",
        "implementation",
        "test",
        "review",
        "documentation",
    ] {
        let raw = tools
            .call(
                "work_add_item",
                json!({"title": "T", "description": "D", "kind": kind}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "kind '{kind}': {raw}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn alle_erlaubten_artifact_kinds_werden_akzeptiert() {
    let dir = tmp_dir("alle_artifact_kinds");
    let (_, tools, _) = registry(&dir);
    for (i, kind) in ["analysis", "code", "test", "documentation", "other"]
        .into_iter()
        .enumerate()
    {
        let raw = tools
            .call(
                "work_artifact",
                json!({"kind": kind, "filename": format!("f{i}.txt"), "content": "x", "summary": "s"}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "kind '{kind}': {raw}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_legt_ein_pending_item_im_snapshot_an() {
    let dir = tmp_dir("add_item");
    let (store, tools, _) = registry(&dir);

    let raw = tools
        .call(
            "work_add_item",
            json!({
                "title": "Analyse der Race Condition",
                "description": "Finde die Ursache des Deadlocks.",
                "kind": "analysis"
            }),
        )
        .unwrap();
    assert!(!raw.starts_with("ERROR"), "{raw}");

    let snapshot = store.snapshot();
    assert_eq!(snapshot.items.len(), 1);
    let item = snapshot.items.values().next().unwrap();
    assert_eq!(item.status, agentkit_work::WorkItemStatus::Pending);
    assert_eq!(item.title, "Analyse der Race Condition");
    assert_eq!(item.priority, 5, "Standardpriorität ohne explizite Angabe");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_unbekannter_kind_ist_ein_weicher_fehler() {
    let dir = tmp_dir("unbekannte_kind");
    let (store, tools, _) = registry(&dir);

    let raw = tools
        .call(
            "work_add_item",
            json!({"title": "T", "description": "D", "kind": "verzauberung"}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(
        raw.contains("discovery"),
        "erwartet Aufzählung der erlaubten Werte: {raw}"
    );
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_unbekannter_abhaengigkeit_legt_kein_item_an() {
    let dir = tmp_dir("unbekannte_abhaengigkeit");
    let (store, tools, _) = registry(&dir);

    let raw = tools
        .call(
            "work_add_item",
            json!({
                "title": "T",
                "description": "D",
                "kind": "implementation",
                "depends_on": ["W-99"]
            }),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_zyklischer_abhaengigkeit_legt_kein_item_an() {
    let dir = tmp_dir("zyklus");
    let (store, tools, _) = registry(&dir);

    // Frischer Store: das neue Item bekäme die ID "W-1" — sie als eigene
    // Abhängigkeit anzugeben ist der einfachste Zyklus (Selbstreferenz), den
    // `WorkState::validate_dependencies` ablehnt.
    let raw = tools
        .call(
            "work_add_item",
            json!({
                "title": "T",
                "description": "D",
                "kind": "implementation",
                "depends_on": ["W-1"]
            }),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_leerem_titel_ist_ein_weicher_fehler() {
    let dir = tmp_dir("leerer_titel");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_add_item",
            json!({"title": "  ", "description": "D", "kind": "implementation"}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_leerer_beschreibung_ist_ein_weicher_fehler() {
    let dir = tmp_dir("leere_beschreibung");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_add_item",
            json!({"title": "T", "description": "  ", "kind": "implementation"}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_add_item_mit_prioritaet_ausserhalb_des_bereichs_ist_ein_weicher_fehler() {
    let dir = tmp_dir("prioritaet");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_add_item",
            json!({"title": "T", "description": "D", "kind": "implementation", "priority": 42}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().items.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_artifact_schreibt_die_datei_und_journalt_mit_slash_trenner() {
    let dir = tmp_dir("artefakt");
    let (store, tools, ctx) = registry(&dir);

    let raw = tools
        .call(
            "work_artifact",
            json!({
                "kind": "analysis",
                "filename": "befund.md",
                "content": "# Befund\nRace Condition in mcp::",
                "summary": "Ursachenanalyse"
            }),
        )
        .unwrap();
    assert!(!raw.starts_with("ERROR"), "{raw}");

    // Zielpfad je VERSUCH (`ctx.attempt_id` = "A-1" laut `registry()`-Helfer),
    // nicht je Item — siehe Kommentar an `resolve_artifact_path`.
    let written = ctx.artifacts_dir.join("W-1").join("A-1").join("befund.md");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "# Befund\nRace Condition in mcp::"
    );

    let snapshot = store.snapshot();
    assert_eq!(snapshot.artifacts.len(), 1);
    let artifact = snapshot.artifacts.values().next().unwrap();
    assert_eq!(artifact.rel_path, "artifacts/W-1/A-1/befund.md");
    assert!(!artifact.rel_path.contains('\\'));

    // Der im Erfolgstext genannte Pfad muss der sein, unter dem der nächste
    // Agent die Datei tatsächlich über `read_file` findet — siehe die
    // Doku an `WorkToolCtx::artifacts_dir`: der Vertrag hält nur, wenn
    // `artifacts_dir` workspace-relativ übergeben wird.
    assert!(
        raw.contains(&written.to_string_lossy().replace('\\', "/")),
        "Erfolgstext sollte den geschriebenen Pfad nennen: {raw}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_artifact_mit_leerem_filename_ist_ein_weicher_fehler() {
    let dir = tmp_dir("leerer_filename");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_artifact",
            json!({"kind": "other", "filename": "", "content": "x", "summary": "s"}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().artifacts.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_artifact_mit_leerer_summary_ist_ein_weicher_fehler() {
    let dir = tmp_dir("leere_artefakt_summary");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_artifact",
            json!({"kind": "other", "filename": "a.txt", "content": "x", "summary": "  "}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().artifacts.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// Zählt alle Dateien unter `dir` rekursiv (für den Vorher/Nachher-Vergleich
/// unten — robuster als eine geratene Zielposition für den Ausbruch).
fn count_files(dir: &std::path::Path) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else {
            count += 1;
        }
    }
    count
}

/// Ein Pfadausbruch darf weder als Erfolg gemeldet werden noch irgendeine
/// Datei erzeugen — weder innerhalb noch außerhalb von `artifacts_dir`.
///
/// `case` ist ein von Hand vergebener, eindeutiger Bezeichner für das
/// Temp-Verzeichnis: aus `filename` selbst abzuleiten kollidiert leicht
/// (`"../boese.txt"` und `"..\\boese.txt"` sanitisieren beide zum selben
/// String), und Tests laufen standardmäßig parallel im selben Prozess
/// (gleiche `process::id()`) — ein Kollisionsfall würde sich also ein
/// Temp-Verzeichnis mit einem anderen Test teilen.
fn assert_pfadausbruch_wird_abgewiesen(case: &str, filename: &str) {
    let dir = tmp_dir(&format!("ausbruch_{case}"));
    let (store, tools, _ctx) = registry(&dir);
    // work.jsonl existiert schon vom Öffnen des Stores — das ist die Basis,
    // gegen die wir vergleichen, kein Datei-Ausbruch darf etwas hinzufügen.
    let dateien_vorher = count_files(&dir);

    let raw = tools
        .call(
            "work_artifact",
            json!({
                "kind": "other",
                "filename": filename,
                "content": "böse",
                "summary": "s"
            }),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{filename}: {raw}");
    assert!(store.snapshot().artifacts.is_empty(), "{filename}");
    assert_eq!(
        count_files(&dir),
        dateien_vorher,
        "{filename}: es ist eine Datei entstanden"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pfadausbruch_ueber_unix_parent_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("unix_parent", "../boese.txt");
}

#[test]
fn pfadausbruch_ueber_unterverzeichnis_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("unterverzeichnis", "unter/pfad.txt");
}

#[test]
fn pfadausbruch_ueber_windows_parent_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("windows_parent", "..\\boese.txt");
}

#[test]
fn pfadausbruch_ueber_absoluten_unix_pfad_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("absoluter_unix_pfad", "/etc/passwd");
}

#[test]
fn pfadausbruch_ueber_laufwerksbuchstaben_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("laufwerksbuchstabe", "C:\\temp\\x.txt");
}

#[test]
fn dateiname_punkt_wird_ueber_die_komponentenzerlegung_abgewiesen() {
    // "." enthält keinen Trenner, kein ".." und keinen Laufwerksbuchstaben —
    // nur die Komponentenzerlegung (Component::CurDir statt Normal) fängt das
    // ab. Regressionstest: belegt, dass dieser zweite Check kein toter Code
    // ist, sondern genau diesen Fall abdeckt.
    assert_pfadausbruch_wird_abgewiesen("punkt", ".");
}

#[test]
fn pfadausbruch_ueber_alternate_data_stream_wird_abgewiesen() {
    // Windows: "befund.md:versteckt" hat weder Trenner noch ".." noch einen
    // Doppelpunkt an Position 1 (der Laufwerksbuchstaben-Check allein hätte
    // das nicht gefangen) — der Inhalt läge in einem Stream, den `read_file`
    // & Co. nie sehen, während das Journal die Datei als normal vorhanden
    // meldet.
    assert_pfadausbruch_wird_abgewiesen("ads", "befund.md:versteckt");
}

#[test]
fn pfadausbruch_ueber_reservierten_geraetenamen_wird_abgewiesen() {
    // "NUL" (und Groß-/Kleinschreibungs- bzw. Endungsvarianten) ist unter
    // Windows kein normaler Dateiname, sondern das Null-Gerät — ein Schreib-
    // versuch verschwindet lautlos, während das Journal ein Artefakt behauptet.
    assert_pfadausbruch_wird_abgewiesen("geraetename_nul", "NUL");
}

#[test]
fn pfadausbruch_ueber_reservierten_geraetenamen_mit_endung_wird_abgewiesen() {
    assert_pfadausbruch_wird_abgewiesen("geraetename_con_txt", "con.txt");
}

#[test]
fn work_artifact_mit_unbekannter_kind_ist_ein_weicher_fehler() {
    let dir = tmp_dir("artefakt_kind");
    let (store, tools, _) = registry(&dir);
    let raw = tools
        .call(
            "work_artifact",
            json!({"kind": "geheimwissen", "filename": "a.md", "content": "x", "summary": "s"}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(store.snapshot().artifacts.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_submit_fuellt_die_submission_ohne_journal_eintrag() {
    let dir = tmp_dir("submit");
    let (store, tools, ctx) = registry(&dir);
    let seq_vorher = store.snapshot().seq;

    let raw = tools
        .call(
            "work_submit",
            json!({
                "summary": "Fertig.",
                "criteria": [
                    {"criterion": "Kompiliert", "met": true, "evidence": "cargo build ok"},
                    {"criterion": "Getestet", "met": false, "evidence": "kein Test geschrieben"}
                ]
            }),
        )
        .unwrap();
    assert!(!raw.starts_with("ERROR"), "{raw}");
    assert!(
        raw.contains('1'),
        "sollte auf 1 unerfülltes Kriterium hinweisen: {raw}"
    );

    let submission = ctx.submission.lock().unwrap().clone();
    let submission = submission.expect("submission gesetzt");
    assert_eq!(submission.summary, "Fertig.");
    assert_eq!(submission.criteria.len(), 2);
    assert!(!submission.criteria[1].met);

    // work_submit journalt bewusst nichts — der Runner schreibt AttemptFinished.
    assert_eq!(store.snapshot().seq, seq_vorher);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_submit_zweiter_aufruf_gewinnt() {
    let dir = tmp_dir("submit_zweimal");
    let (_, tools, ctx) = registry(&dir);

    tools
        .call("work_submit", json!({"summary": "Erster Versuch"}))
        .unwrap();
    tools
        .call("work_submit", json!({"summary": "Zweiter Versuch"}))
        .unwrap();

    let submission = ctx.submission.lock().unwrap().clone().unwrap();
    assert_eq!(submission.summary, "Zweiter Versuch");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_submit_mit_leerer_summary_ist_ein_weicher_fehler() {
    let dir = tmp_dir("submit_leer");
    let (_, tools, ctx) = registry(&dir);
    let raw = tools
        .call("work_submit", json!({"summary": "   "}))
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert!(ctx.submission.lock().unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

/// Regressionstest für das Race bei der ID-Vergabe: der Agent-Kern führt
/// mehrere Tool-Aufrufe EINER Modellantwort parallel in `std::thread::scope`
/// aus (siehe `agent_framework_rs/CLAUDE.md` „Tools"), und das Zerlegen eines
/// Vorhabens ruft `work_add_item` typischerweise mehrfach in genau so einer
/// Antwort auf. Vor der Korrektur berechneten beide Threads `next_item_id()`
/// auf einem außerhalb des Schreiber-Locks gelesenen Snapshot — beide bekamen
/// dieselbe ID, und der zweite `submit` überschrieb den ersten Datensatz
/// still (`BTreeMap::insert`), ohne dass irgendetwas fehlschlug.
#[test]
fn zwei_parallele_work_add_item_aufrufe_erzeugen_zwei_verschiedene_items() {
    let dir = tmp_dir("race_add_item");
    let (store, tools, _) = registry(&dir);
    let barrier = Barrier::new(2);

    std::thread::scope(|scope| {
        for i in 0..2 {
            let tools = &tools;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let raw = tools
                    .call(
                        "work_add_item",
                        json!({
                            "title": format!("Item {i}"),
                            "description": "D",
                            "kind": "implementation"
                        }),
                    )
                    .unwrap();
                assert!(!raw.starts_with("ERROR"), "{raw}");
            });
        }
    });

    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.items.len(),
        2,
        "zwei parallele Aufrufe müssen zwei Items anlegen, keines darf verschwinden"
    );
    let ids: std::collections::HashSet<&str> = snapshot.items.keys().map(String::as_str).collect();
    assert_eq!(
        ids.len(),
        2,
        "die beiden Items müssen verschiedene IDs haben: {ids:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Befund 4 des Code-Reviews (Verdacht aus der Handprobe): vor der Korrektur
/// schrieb `work_artifact` die Datei AUSSERHALB des Schreiber-Locks — zwei
/// parallele Aufrufe mit demselben Dateinamen konnten sich wortlos
/// überschreiben, und BEIDE meldeten Erfolg, obwohl das früher journalte
/// Artefakt hinterher auf einen Inhalt zeigte, den es nie erzeugt hat.
///
/// Entschiedenes Verhalten (siehe Kommentar an `work_artifact` in `tools.rs`):
/// das Schreiben selbst ist jetzt exklusiv (`create_new` unter dem Lock) —
/// GENAU einer der beiden gleichnamigen Aufrufe gewinnt, der andere scheitert
/// SICHTBAR als weicher Fehler. Die Alternative ("zwei Artefakte mit
/// unterschiedlichem `rel_path`") hätte den stabilen Pfad-Vertrag
/// `artifacts/{item}/{filename}` verwässert (siehe
/// `work_artifact_schreibt_die_datei_und_journalt_mit_slash_trenner`), auf
/// den sich `read_file` beim nächsten Agenten verlässt.
#[test]
fn zwei_parallele_work_artifact_aufrufe_mit_gleichem_dateinamen_lassen_nur_einen_gewinnen() {
    let dir = tmp_dir("race_artifact_gleicher_name");
    let (store, tools, ctx) = registry(&dir);
    let barrier = Barrier::new(2);
    let results: Mutex<Vec<String>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for i in 0..2 {
            let tools = &tools;
            let barrier = &barrier;
            let results = &results;
            scope.spawn(move || {
                barrier.wait();
                let raw = tools
                    .call(
                        "work_artifact",
                        json!({
                            "kind": "other",
                            "filename": "gleich.txt",
                            "content": format!("Inhalt {i}"),
                            "summary": "s"
                        }),
                    )
                    .unwrap();
                results.lock().unwrap().push(raw);
            });
        }
    });

    let results = results.into_inner().unwrap();
    let erfolge = results.iter().filter(|r| !r.starts_with("ERROR")).count();
    let fehler = results.iter().filter(|r| r.starts_with("ERROR")).count();
    assert_eq!(
        (erfolge, fehler),
        (1, 1),
        "genau einer der beiden gleichnamigen Aufrufe darf gewinnen: {results:?}"
    );

    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.artifacts.len(),
        1,
        "nur das gewinnende Artefakt darf journalt sein — kein Datensatz darf auf einen \
         fremden Inhalt zeigen"
    );

    let file = ctx.artifacts_dir.join("W-1").join("A-1").join("gleich.txt");
    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content == "Inhalt 0" || content == "Inhalt 1",
        "Dateiinhalt muss unverändert von genau einem der beiden Aufrufe stammen: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Wie oben, für `work_artifact`: zwei parallele Aufrufe müssen zwei
/// Artefakte mit verschiedenen IDs journalen und zwei verschiedene Dateien
/// schreiben.
#[test]
fn zwei_parallele_work_artifact_aufrufe_erzeugen_zwei_verschiedene_artefakte() {
    let dir = tmp_dir("race_artifact");
    let (store, tools, ctx) = registry(&dir);
    let barrier = Barrier::new(2);

    std::thread::scope(|scope| {
        for i in 0..2 {
            let tools = &tools;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let raw = tools
                    .call(
                        "work_artifact",
                        json!({
                            "kind": "other",
                            "filename": format!("f{i}.txt"),
                            "content": format!("Inhalt {i}"),
                            "summary": "s"
                        }),
                    )
                    .unwrap();
                assert!(!raw.starts_with("ERROR"), "{raw}");
            });
        }
    });

    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.artifacts.len(),
        2,
        "zwei parallele Aufrufe müssen zwei Artefakte journalen, keines darf verschwinden"
    );
    let ids: std::collections::HashSet<&str> =
        snapshot.artifacts.keys().map(String::as_str).collect();
    assert_eq!(
        ids.len(),
        2,
        "die beiden Artefakte müssen verschiedene IDs haben: {ids:?}"
    );

    for i in 0..2 {
        let file = ctx
            .artifacts_dir
            .join("W-1")
            .join("A-1")
            .join(format!("f{i}.txt"));
        assert!(file.exists(), "Datei für f{i}.txt fehlt");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// DIE REGRESSION: der Zielpfad hing vorher nur am Item
/// (`artifacts/<item>/<filename>`), nicht am Versuch. Scheiterte Versuch 1
/// eines Items, NACHDEM er `befund.md` geschrieben hatte, traf Versuch 2
/// desselben Items beim erneuten `work_artifact("befund.md")` auf eine schon
/// existierende Datei und scheiterte an `create_new` — obwohl Retry ein
/// Kernfeature dieser Laufzeit ist (`max_attempts`, `WorkItemReleased`, die
/// vorherige Fehlerursache im nächsten Arbeitspaket). Dieser Test instanziiert
/// zwei Registries mit DEMSELBEN Item, aber verschiedenen `attempt_id`
/// (simuliert zwei aufeinanderfolgende Versuche desselben Items) und verlangt,
/// dass beide erfolgreich denselben Dateinamen ablegen.
#[test]
fn zwei_aufeinanderfolgende_versuche_desselben_items_kollidieren_nicht_bei_gleichem_dateinamen() {
    let dir = tmp_dir("retry_gleicher_dateiname");
    let store = Arc::new(WorkStore::open(&dir).unwrap());

    let make_ctx = |attempt_id: &str| WorkToolCtx {
        run_id: "R-1".to_string(),
        work_item_id: "W-1".to_string(),
        attempt_id: attempt_id.to_string(),
        agent_id: "agent-1".to_string(),
        max_attempts: 3,
        project_id: "P-1".to_string(),
        repository_revision: None,
        artifacts_dir: dir.join("artifacts"),
        submission: Arc::new(Mutex::new(None)),
        gateway: None,
        verifies: None,
    };

    // Versuch 1 (A-1) legt "befund.md" ab und "scheitert" danach (in der
    // echten Laufzeit würde der Runner das journalen — hier reicht es, dass
    // die Datei schon liegt, wenn Versuch 2 antritt).
    let mut tools_a1 = ToolRegistry::new();
    register_work_tools(&mut tools_a1, store.clone(), make_ctx("A-1"));
    let raw1 = tools_a1
        .call(
            "work_artifact",
            json!({
                "kind": "analysis",
                "filename": "befund.md",
                "content": "Versuch 1",
                "summary": "Erster Anlauf"
            }),
        )
        .unwrap();
    assert!(!raw1.starts_with("ERROR"), "{raw1}");

    // Versuch 2 (A-2), Retry desselben Items W-1, legt DENSELBEN Dateinamen
    // ab — das ist der eigentliche Regressionsfall.
    let mut tools_a2 = ToolRegistry::new();
    register_work_tools(&mut tools_a2, store.clone(), make_ctx("A-2"));
    let raw2 = tools_a2
        .call(
            "work_artifact",
            json!({
                "kind": "analysis",
                "filename": "befund.md",
                "content": "Versuch 2",
                "summary": "Zweiter Anlauf"
            }),
        )
        .unwrap();
    assert!(
        !raw2.starts_with("ERROR"),
        "Retry desselben Items darf nicht an einer Datei seines Vorgänger-Versuchs scheitern: {raw2}"
    );

    // Zwei Dateien unter verschiedenen Versuchsverzeichnissen.
    let file_a1 = dir
        .join("artifacts")
        .join("W-1")
        .join("A-1")
        .join("befund.md");
    let file_a2 = dir
        .join("artifacts")
        .join("W-1")
        .join("A-2")
        .join("befund.md");
    assert_eq!(std::fs::read_to_string(&file_a1).unwrap(), "Versuch 1");
    assert_eq!(std::fs::read_to_string(&file_a2).unwrap(), "Versuch 2");

    // Zwei WorkArtifact-Datensätze mit verschiedenen rel_path.
    let snapshot = store.snapshot();
    assert_eq!(snapshot.artifacts.len(), 2);
    let mut rel_paths: Vec<&str> = snapshot
        .artifacts
        .values()
        .map(|a| a.rel_path.as_str())
        .collect();
    rel_paths.sort();
    assert_eq!(
        rel_paths,
        vec!["artifacts/W-1/A-1/befund.md", "artifacts/W-1/A-2/befund.md"]
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------- work_claim

/// `ClaimsRecorded` journalt am Versuch (`state::apply` sucht ihn in
/// `self.attempts`) — anders als bei `work_add_item`/`work_artifact` reicht
/// die reine `WorkToolCtx` mit ihrer manuell gesetzten `attempt_id` also
/// nicht: der Versuch muss WIRKLICH im Store existieren. Legt Item "W-1" an
/// und claimt es als "A-1"/"agent-1" — dieselben IDs, die `registry()`/
/// `registry_with_gateway()` in den Kontext schreiben.
fn seed_attempt(store: &WorkStore) {
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: agentkit_work::WorkItem {
                id: "W-1".to_string(),
                run_id: "R-1".to_string(),
                title: "T".to_string(),
                description: "D".to_string(),
                kind: agentkit_work::WorkItemKind::Implementation,
                status: agentkit_work::WorkItemStatus::Pending,
                priority: 5,
                seq: 1,
                required_role: None,
                dependencies: vec![],
                acceptance_criteria: vec![],
                verification_policy: agentkit_work::VerificationPolicy::None,
                verifies: None,
                claims_promoted: false,
                executor: agentkit_work::ExecutorKind::SingleAgent,
                attempt_count: 0,
                max_attempts: 3,
                updated_at_ms: 0,
            },
        })
        .unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemClaimed {
            item: "W-1".to_string(),
            agent: "agent-1".to_string(),
            attempt: "A-1".to_string(),
            lease_expires_ms: 1_000_000,
            at_ms: 0,
        })
        .unwrap();
}

#[test]
fn work_claim_fehlt_ohne_gateway_und_erscheint_mit_gateway() {
    let dir = tmp_dir("claim_ohne_gateway");
    let (_, tools, _) = registry(&dir);
    assert!(!tools.has("work_claim"));
    std::fs::remove_dir_all(&dir).ok();

    let dir2 = tmp_dir("claim_mit_gateway");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (_, tools2, _) = registry_with_gateway(&dir2, Some(gateway));
    assert!(tools2.has("work_claim"));
    std::fs::remove_dir_all(&dir2).ok();
}

/// Phase 5b: `work_verdict` ist NUR im Prüf-Item registriert (`ctx.verifies`
/// gesetzt), nicht im normalen Item — dasselbe Fähigkeit-bei-Registrierung-
/// Muster wie `work_claim`/`ctx.gateway`.
#[test]
fn work_verdict_ist_nur_im_pruef_item_registriert_nicht_im_normalen_item() {
    let dir = tmp_dir("verdict_normales_item");
    let (_, tools, _) = registry(&dir);
    assert!(
        !tools.has("work_verdict"),
        "ein normales Item (verifies: None) bekommt 'work_verdict' nicht"
    );
    std::fs::remove_dir_all(&dir).ok();

    let dir2 = tmp_dir("verdict_pruef_item");
    let store = Arc::new(WorkStore::open(&dir2).unwrap());
    let ctx = WorkToolCtx {
        run_id: "R-1".to_string(),
        work_item_id: "W-2".to_string(),
        attempt_id: "A-2".to_string(),
        agent_id: "agent-1".to_string(),
        max_attempts: 3,
        project_id: "P-1".to_string(),
        repository_revision: None,
        artifacts_dir: dir2.join("artifacts"),
        submission: Arc::new(Mutex::new(None)),
        gateway: None,
        verifies: Some("W-1".to_string()),
    };
    let mut tools2 = ToolRegistry::new();
    register_work_tools(&mut tools2, store, ctx);
    assert!(
        tools2.has("work_verdict"),
        "ein Prüf-Item (verifies: Some(..)) bekommt 'work_verdict'"
    );
    std::fs::remove_dir_all(&dir2).ok();
}

#[test]
fn work_claim_uebergibt_vollstaendige_provenance_inklusive_artefaktpfade() {
    let dir = tmp_dir("claim_provenance");
    let gateway = Arc::new(FakeGraph::new());
    let (store, tools, ctx) =
        registry_with_gateway(&dir, Some(gateway.clone() as Arc<dyn GraphGateway>));
    seed_attempt(&store);

    // Ein Artefakt DIESES Versuchs muss in `artifact_paths` landen.
    store
        .submit(agentkit_work::WorkEvent::ArtifactCreated {
            artifact: agentkit_work::WorkArtifact {
                id: "AR-1".to_string(),
                work_item_id: ctx.work_item_id.clone(),
                attempt_id: ctx.attempt_id.clone(),
                kind: agentkit_work::ArtifactKind::Analysis,
                rel_path: "artifacts/W-1/A-1/befund.md".to_string(),
                summary: "Befund".to_string(),
                created_at_ms: 0,
            },
        })
        .unwrap();

    let raw = tools
        .call(
            "work_claim",
            json!({
                "claims": [
                    {"subject": "Race Condition", "predicate": "verursacht", "object": "Deadlock"}
                ]
            }),
        )
        .unwrap();
    assert!(!raw.starts_with("ERROR"), "{raw}");

    let recorded = gateway.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let (prov, claims) = &recorded[0];
    assert_eq!(prov.project_id, ctx.project_id);
    assert_eq!(prov.run_id, ctx.run_id);
    assert_eq!(prov.work_item_id, ctx.work_item_id);
    assert_eq!(prov.attempt_id, ctx.attempt_id);
    assert_eq!(prov.agent_id, ctx.agent_id);
    assert_eq!(
        prov.artifact_paths,
        vec!["artifacts/W-1/A-1/befund.md".to_string()]
    );
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].subject, "Race Condition");
    assert_eq!(claims[0].predicate, "verursacht");
    assert_eq!(claims[0].object, "Deadlock");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_claim_mit_leerer_liste_ist_ein_weicher_fehler_und_journalt_nichts() {
    let dir = tmp_dir("claim_leer");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (store, tools, _) = registry_with_gateway(&dir, Some(gateway));
    let seq_vorher = store.snapshot().seq;
    let raw = tools.call("work_claim", json!({"claims": []})).unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert_eq!(
        store.snapshot().seq,
        seq_vorher,
        "nichts darf journalt sein"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_claim_mit_leerem_feld_ist_ein_weicher_fehler_und_journalt_nichts() {
    let dir = tmp_dir("claim_leeres_feld");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (store, tools, _) = registry_with_gateway(&dir, Some(gateway));
    let seq_vorher = store.snapshot().seq;
    let raw = tools
        .call(
            "work_claim",
            json!({"claims": [{"subject": "  ", "predicate": "x", "object": "y"}]}),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert_eq!(
        store.snapshot().seq,
        seq_vorher,
        "nichts darf journalt sein"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_claim_mit_ungueltiger_confidence_ist_ein_weicher_fehler_und_journalt_nichts() {
    let dir = tmp_dir("claim_confidence");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (store, tools, _) = registry_with_gateway(&dir, Some(gateway));
    let seq_vorher = store.snapshot().seq;
    let raw = tools
        .call(
            "work_claim",
            json!({
                "claims": [
                    {"subject": "X", "predicate": "verursacht", "object": "Y", "confidence": 1.5}
                ]
            }),
        )
        .unwrap();
    assert!(raw.starts_with("ERROR:"), "{raw}");
    assert_eq!(
        store.snapshot().seq,
        seq_vorher,
        "nichts darf journalt sein"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zwei_work_claim_aufrufe_haengen_claim_ids_am_versuch_an() {
    let dir = tmp_dir("claim_zweimal");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let (store, tools, ctx) = registry_with_gateway(&dir, Some(gateway));
    seed_attempt(&store);

    let raw1 = tools
        .call(
            "work_claim",
            json!({"claims": [{"subject": "A", "predicate": "p", "object": "B"}]}),
        )
        .unwrap();
    assert!(!raw1.starts_with("ERROR"), "{raw1}");
    let raw2 = tools
        .call(
            "work_claim",
            json!({"claims": [{"subject": "C", "predicate": "p", "object": "D"}]}),
        )
        .unwrap();
    assert!(!raw2.starts_with("ERROR"), "{raw2}");

    let snapshot = store.snapshot();
    let attempt = &snapshot.attempts[&ctx.attempt_id];
    assert_eq!(
        attempt.claim_ids.len(),
        2,
        "beide Aufrufe müssen anhängen, nicht ersetzen: {:?}",
        attempt.claim_ids
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn work_claim_persistiert_ueber_einen_neustart_des_stores() {
    let dir = tmp_dir("claim_neustart");
    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph::new());
    let attempt_id = {
        let (store, tools, ctx) = registry_with_gateway(&dir, Some(gateway));
        seed_attempt(&store);
        let raw = tools
            .call(
                "work_claim",
                json!({"claims": [{"subject": "A", "predicate": "p", "object": "B"}]}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        store.checkpoint().unwrap();
        ctx.attempt_id.clone()
        // `store` fällt hier aus dem Scope — gibt die Sperrdatei frei.
    };

    let reopened = WorkStore::open(&dir).unwrap();
    let attempt = &reopened.snapshot().attempts[&attempt_id];
    assert_eq!(attempt.claim_ids.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}
