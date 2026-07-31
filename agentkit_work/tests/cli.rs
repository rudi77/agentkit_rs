//! Die Work-CLI als Spezifikation: Parsing, Verzeichnis-/Journal-Anlage,
//! `--items`-Verarbeitung, JSON-Kontrakt (genau ein Dokument auf stdout) und
//! Exit-Codes. Teststil wie die übrigen Tests (deutsche Satznamen, ein
//! Verhalten pro Test).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::testing::FakeLlm;
use agentkit::{new_cancel, Chunk, ExitCode, Llm};
use agentkit_work::{dispatch_with_io, ClaimText, GraphGateway, WorkCliDeps, WorkProvenance};
use serde_json::{json, Value};

/// Test-Doppelgänger für [`GraphGateway`] (Duplikat-Begründung siehe
/// `tests/graph.rs`/`tests/tools.rs`/`tests/runner.rs`): zeichnet `promote`-
/// Aufrufe auf.
#[derive(Default)]
struct FakeGraph {
    promoted: Mutex<Vec<String>>,
}

impl GraphGateway for FakeGraph {
    fn recall(&self, _query: &str) -> Option<String> {
        None
    }

    fn record_claims(
        &self,
        _prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        Ok((1..=claims.len()).map(|n| format!("C-{n}")).collect())
    }

    fn promote(&self, claim_ids: &[String]) -> Result<usize, String> {
        self.promoted
            .lock()
            .unwrap()
            .extend(claim_ids.iter().cloned());
        Ok(claim_ids.len())
    }
}

// ------------------------------------------------------------------ Helfer

fn tmp_dir(name: &str) -> std::path::PathBuf {
    static NR: AtomicUsize = AtomicUsize::new(0);
    let nr = NR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_cli_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `WorkCliDeps` mit einem gegebenen LLM — `'static` über `Box::leak`, weil
/// Tests keine Aufräum-Logik für die Closure brauchen (der Prozess endet
/// nach dem Testlauf ohnehin).
fn deps_with(llm: Arc<dyn Llm>) -> WorkCliDeps<'static> {
    let f: &'static dyn Fn(&str, bool) -> Arc<dyn Llm> =
        Box::leak(Box::new(move |_p: &str, _demo: bool| llm.clone()));
    WorkCliDeps {
        llm: f,
        approve: Arc::new(|_: &str| false),
        extra_tools: None,
        cancel: new_cancel(),
        graph: None,
        // Kein Dispatcher in diesen Tests — der Einzelagenten-Executor läuft
        // unverändert, exakt wie vor Phase 6.
        build_executor: None,
        trace: None,
    }
}

fn deps_stub() -> WorkCliDeps<'static> {
    deps_with(Arc::new(FakeLlm::new(vec![])))
}

/// Wie [`deps_with`], aber mit einem angebundenen Wissensgraphen (Phase 5b) —
/// eigene Funktion statt `deps_with`s Signatur zu ändern, das würde jeden
/// bestehenden Aufruf berühren.
fn deps_with_graph(llm: Arc<dyn Llm>, graph: Arc<dyn GraphGateway>) -> WorkCliDeps<'static> {
    let mut deps = deps_with(llm);
    deps.graph = Some(graph);
    deps
}

fn run_cli(argv: &[String], deps: WorkCliDeps<'static>) -> (ExitCode, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = dispatch_with_io(argv, deps, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

// ------------------------------------------------------------------ create

#[test]
fn create_legt_verzeichnis_journal_projekt_und_lauf_an_und_gibt_slug_aus() {
    let ws = tmp_dir("create");
    let (code, out, _err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Graceful Swarm Shutdown",
            "--objective",
            "Analysiere und behebe das Problem.",
            "-w",
        ])
        .into_iter()
        .chain(std::iter::once(ws.to_string_lossy().to_string()))
        .collect::<Vec<_>>(),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(out.trim(), "graceful-swarm-shutdown");

    let project_dir = ws
        .join(".agentkit")
        .join("work")
        .join("graceful-swarm-shutdown");
    assert!(project_dir.join("work.jsonl").exists());

    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    let project = snapshot.project.as_ref().expect("Projekt angelegt");
    assert_eq!(project.title, "Graceful Swarm Shutdown");
    // Angepasst für Befund 5 (Handprobe): `project.workspace` speichert seit
    // der Korrektur den KANONISIERTEN, absoluten Pfad, nicht mehr den rohen
    // Aufrufwert wörtlich — `ws` selbst ist zwar schon absolut, aber unter
    // Windows kann `canonicalize` eine andere Schreibweise liefern (z. B.
    // ein `\\?\`-Präfix), deshalb wird hier gegen dieselbe Kanonisierung
    // verglichen statt gegen den rohen String.
    assert_eq!(
        project.workspace,
        std::fs::canonicalize(&ws).unwrap().display().to_string()
    );
    assert!(snapshot.runs.contains_key("R-1"));
    assert_eq!(
        snapshot.runs["R-1"].status,
        agentkit_work::RunStatus::Running
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn zweimaliges_create_mit_gleichem_titel_ergibt_slug_und_slug_2() {
    let ws = tmp_dir("create_kollision");
    let ws_str = ws.to_string_lossy().to_string();

    let (code1, out1, _) = run_cli(
        &args(&[
            "create",
            "--title",
            "Demo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code1, ExitCode::Success);
    assert_eq!(out1.trim(), "demo");

    let (code2, out2, _) = run_cli(
        &args(&[
            "create",
            "--title",
            "Demo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code2, ExitCode::Success);
    assert_eq!(out2.trim(), "demo-2");

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------- Phase 7: Git-Isolation

/// `tmp_dir` legt nur ein normales Verzeichnis an (kein `git init`) — genau
/// der Fall, den `--git-isolation` ablehnen soll: ein Vorhaben außerhalb
/// eines Git-Repositoriums darf nicht mit einem kryptischen Git-Fehler erst
/// beim ersten Lauf scheitern, sondern schon bei `create` mit einer klaren
/// deutschen Meldung.
#[test]
fn create_mit_git_isolation_ausserhalb_eines_git_repos_wird_klar_abgelehnt() {
    let ws = tmp_dir("git_isolation_kein_repo");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, _out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Demo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--git-isolation",
        ]),
        deps_stub(),
    );

    assert_eq!(code, ExitCode::GeneralError);
    assert!(
        err.contains("--git-isolation") && err.contains("Git-Repository"),
        "erwarte eine klare deutsche Meldung, war: {err}"
    );
    // Kein Projektverzeichnis darf zurückbleiben — die Ablehnung greift, BEVOR
    // irgendetwas angelegt wird.
    assert!(!ws.join(".agentkit").join("work").join("demo").exists());

    std::fs::remove_dir_all(&ws).ok();
}

/// Innerhalb eines echten Git-Repos wird `--git-isolation` akzeptiert und im
/// Journal festgehalten (`WorkProject::git_isolation`).
#[test]
fn create_mit_git_isolation_innerhalb_eines_git_repos_wird_gespeichert() {
    let ws = tmp_dir("git_isolation_repo");
    let ws_str = ws.to_string_lossy().to_string();
    // Die „lege ein Wegwerf-Repo mit einem Commit an"-Sequenz lebt gemeinsam
    // mit `src/git.rs`s Unit-Tests und `tests/runner.rs` in EINER Stelle
    // (Befund 3 des Reviews, Feature `test-support`).
    agentkit_work::git::init_repo_with_commit(&ws_str);

    let (code, out, _err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Demo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--git-isolation",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success);
    assert_eq!(out.trim(), "demo");

    let project_dir = ws.join(".agentkit").join("work").join("demo");
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    assert!(snapshot.project.as_ref().unwrap().git_isolation);

    std::fs::remove_dir_all(&ws).ok();
}

/// Legt ein Vorhaben mit ZWEI unabhängigen, schreibenden Items an
/// (`--items`, kein `depends_on` zwischen ihnen) und liefert dessen ID —
/// gemeinsamer Aufbau für die Regressionstests unten.
fn create_projekt_mit_zwei_unabhaengigen_items(ws: &std::path::Path) -> String {
    let ws_str = ws.to_string_lossy().to_string();
    // Die Items-Datei liegt bewusst NEBEN, nicht IN `ws`: sie ist selbst kein
    // Teil des Arbeitsbaums, sondern nur `create`s Eingabe — läge sie in `ws`,
    // wäre sie eine echte, uncommittete fremde Datei und würde den
    // Arbeitsbaum zu Recht als "nicht sauber" markieren, unabhängig vom
    // `.agentkit`-Ausschluss, den dieser Test eigentlich prüft.
    let items_path = ws.with_extension("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Feature A",
                "description": "Lege a.txt an.",
                "kind": "implementation"
            },
            {
                "title": "Feature B",
                "description": "Lege b.txt an.",
                "kind": "implementation"
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ZweiUnabhaengigeItems",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--git-isolation",
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    out.trim().to_string()
}

/// FakeLlm-Skript für EIN schreibendes Item: legt `filename` mit `content`
/// an, ruft dann `work_submit` auf und schließt mit Text ab — dasselbe
/// Drei-Zug-Muster wie die übrigen `write_file`-Tests dieser Datei.
fn write_file_and_submit_script(
    filename: &'static str,
    content: &'static str,
    tool_id: &'static str,
    summary: &'static str,
) -> Vec<Vec<Chunk>> {
    vec![
        vec![Chunk::tool(
            0,
            tool_id,
            "write_file",
            &json!({"path": filename, "content": content}).to_string(),
        )],
        vec![Chunk::tool(
            0,
            &format!("{tool_id}-submit"),
            "work_submit",
            &json!({"summary": summary, "criteria": []}).to_string(),
        )],
        vec![Chunk::text(summary)],
    ]
}

/// Regressionstest zum Widerspruch aus Git-Isolation und Journal-Ablage im
/// selben Arbeitsbaum: `agentkit work create` legt `.agentkit/work/…/
/// work.jsonl` schon VOR dem ersten Versuch an — ohne den Ausschluss dieses
/// Pfads aus `git::is_clean` wäre der Arbeitsbaum dadurch bereits vor dem
/// ersten Item "nicht sauber", und jeder `--git-isolation`-Lauf schlüge fehl,
/// solange kein `.gitignore` für `.agentkit/` existiert. Bewusst KEIN
/// `.gitignore` hier (anders als der frühere Workaround in
/// `events_zeigt_nach_git_commit_eine_sprechende_zeile_mit_commit_und_branch`
/// unten) — genau das ist der Fall, den die Korrektur tragen muss: zwei
/// schreibende Items bekommen je einen eigenen Commit, die automatische
/// Integration merged beide in den Ausgangsbranch, ohne dass der Bediener
/// irgendetwas an `.gitignore` pflegen musste.
#[test]
fn git_isolation_lauf_mit_zwei_items_laeuft_ohne_gitignore_vollstaendig_durch() {
    let ws = tmp_dir("git_isolation_zwei_items");
    let ws_str = ws.to_string_lossy().to_string();
    agentkit_work::git::init_repo_with_commit(&ws_str);
    let project_id = create_projekt_mit_zwei_unabhaengigen_items(&ws);

    let mut script = write_file_and_submit_script("a.txt", "a", "c1", "Feature A fertig.");
    script.extend(write_file_and_submit_script(
        "b.txt",
        "b",
        "c2",
        "Feature B fertig.",
    ));
    let llm = Arc::new(FakeLlm::new(script)) as Arc<dyn Llm>;

    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stdout: {out}, stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("all_items_done"));

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.items["W-1"].status,
        agentkit_work::WorkItemStatus::Completed
    );
    assert_eq!(
        snapshot.items["W-2"].status,
        agentkit_work::WorkItemStatus::Completed
    );
    let integration = snapshot
        .items
        .values()
        .find(|it| it.kind == agentkit_work::WorkItemKind::Integration)
        .expect("Integrations-Item wurde angelegt");
    assert_eq!(
        integration.status,
        agentkit_work::WorkItemStatus::Completed,
        "die Integration muss beide Item-Branches ohne Konflikt gemerged haben"
    );

    assert_eq!(agentkit_work::git::current_branch(&ws_str).unwrap(), "main");
    assert!(ws.join("a.txt").exists());
    assert!(ws.join("b.txt").exists());
    assert!(
        agentkit_work::git::is_clean(&ws_str).unwrap(),
        "nach dem Lauf ist der Arbeitsbaum sauber (bis auf die eigene, ausgenommene Buchführung)"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Ergänzt den Test oben: der Commit EINES Items enthält die fachliche
/// Änderung, aber keine Datei unterhalb von `.agentkit/` — auch wenn genau
/// dieses Verzeichnis während desselben Laufs aktiv beschrieben wird (das
/// offene Journal). Regressionsschutz für die zweite Hälfte der Korrektur:
/// `git::commit_all` staged über eine Pathspec-Ausschlussregel, nicht per
/// nachträglichem `reset`.
#[test]
fn item_commit_enthaelt_die_fachdatei_aber_keine_datei_unterhalb_von_agentkit() {
    let ws = tmp_dir("git_isolation_commit_inhalt");
    let ws_str = ws.to_string_lossy().to_string();
    agentkit_work::git::init_repo_with_commit(&ws_str);
    let project_id = create_projekt_mit_zwei_unabhaengigen_items(&ws);

    let mut script = write_file_and_submit_script("a.txt", "a", "c1", "Feature A fertig.");
    script.extend(write_file_and_submit_script(
        "b.txt",
        "b",
        "c2",
        "Feature B fertig.",
    ));
    let llm = Arc::new(FakeLlm::new(script)) as Arc<dyn Llm>;

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    let commit_w1 = snapshot
        .artifacts
        .values()
        .find(|a| {
            a.kind == agentkit_work::ArtifactKind::GitCommit && a.work_item_id.as_str() == "W-1"
        })
        .and_then(|a| a.commit_id.clone())
        .expect("W-1 hat einen GitCommit-Artefakt");

    let out = std::process::Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &commit_w1,
        ])
        .current_dir(&ws_str)
        .output()
        .expect("git diff-tree ausführbar");
    let files = String::from_utf8_lossy(&out.stdout);
    assert!(files.contains("a.txt"), "{files}");
    assert!(!files.contains(".agentkit"), "{files}");

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------- Befund 1: Ausgangsbranch

/// Baut ein git-isoliertes Vorhaben mit genau einem, schon `Completed` Item
/// (Kind `Review` — zählt laut `WorkItemKind::is_git_isolated` bewusst NICHT
/// als git-isoliert, siehe `model.rs`). Der Lauf ist damit sofort
/// `Decision::Done`, sobald `run`/`resume` ihn öffnen — kein Agent wird dafür
/// gebraucht (die Tests unten übergeben ein leeres `FakeLlm`, das bei jedem
/// Aufruf hart panicken würde; das ist der Beleg, dass wirklich kein
/// Agentenlauf stattfindet). Gemeinsamer Aufbau für die Regressionstests zu
/// Befund 1 der Handprobe: der Ausgangsbranch wird nie zurückgesetzt.
fn create_projekt_git_isoliert_mit_erledigtem_item(ws: &std::path::Path) -> String {
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "AusgangsbranchTest",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--git-isolation",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: agentkit_work::WorkItem {
                id: "W-1".to_string(),
                run_id: "R-1".to_string(),
                title: "Erledigtes Review".to_string(),
                description: "Schon fertig.".to_string(),
                kind: agentkit_work::WorkItemKind::Review,
                status: agentkit_work::WorkItemStatus::Completed,
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
    drop(store);
    project_id
}

/// Regressionstest zu Befund 1 der Handprobe: ein hart beendeter Prozess
/// (SIGKILL, Absturz) lässt den Arbeitsbaum auf dem Item-Branch stehen — der
/// `Drop`-Guard aus `runner::GitAttemptCtx` läuft dann nicht mehr. Simuliert
/// wird das, indem der Item-Branch direkt über das `git`-Modul angelegt und
/// ausgecheckt wird (kein `GitAttemptCtx` beteiligt, also auch kein Drop, der
/// ihn aufräumen könnte) — exakt der Fußabdruck, den ein abgestürzter Prozess
/// hinterlässt. `agentkit work run` muss diesen EIGENEN Item-Branch danach
/// selbst auf den im Journal festgehaltenen Ausgangsbranch zurückholen.
///
/// Gegen den unveränderten Code (vor dieser Korrektur) schlägt dieser Test
/// fehl: `runner::run_integration_item` liest den Zielbranch der Integration
/// über `git::current_branch(workspace)` — stünde das Repository beim
/// Lauf-Start noch auf dem Item-Branch, würde die Integration in DIESEN
/// Branch hinein "abschließen", und der Arbeitsbaum bliebe danach auf
/// `work/<projekt>/W-1` stehen statt auf `main`.
#[test]
fn abgestuerzter_lauf_auf_eigenem_item_branch_wird_bei_run_auf_ausgangsbranch_zurueckgeholt() {
    let ws = tmp_dir("stray_branch_eigen");
    let ws_str = ws.to_string_lossy().to_string();
    let base = agentkit_work::git::init_repo_with_commit(&ws_str);
    let project_id = create_projekt_git_isoliert_mit_erledigtem_item(&ws);

    let item_branch = agentkit_work::item_branch_name(&project_id, "W-1");
    agentkit_work::git::ensure_item_branch(&ws_str, &item_branch, &base).unwrap();
    assert_eq!(
        agentkit_work::git::current_branch(&ws_str).unwrap(),
        item_branch
    );

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    // Ein zusammenhängender Ausschnitt statt zweier unabhängiger `contains`
    // (Befund des Code-Reviews): so ist sichergestellt, dass BEIDE Fragmente
    // aus derselben Meldung stammen, nicht zufällig aus zwei verschiedenen
    // stderr-Zeilen (z. B. `warn_workspace_mismatch` nennt ebenfalls Branches).
    assert!(
        err.contains("zurück auf Ausgangsbranch 'main' gewechselt"),
        "stderr sollte den Wechsel nennen: {err}"
    );
    assert!(err.contains(&item_branch), "stderr: {err}");
    assert_eq!(
        agentkit_work::git::current_branch(&ws_str).unwrap(),
        "main",
        "das Repository muss nach 'run' wieder auf dem Ausgangsbranch stehen"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Gegenstück zum Test oben: steht das Repository auf einem FREMDEN Branch
/// (keiner, der zu diesem Projekt gehört), überschreibt `run` das NICHT
/// stillschweigend — das kann eine bewusste Entscheidung des Nutzers sein
/// (z. B. ein manueller Checkout zwischen zwei Aufrufen). Erwartet wird nur
/// eine deutliche Warnung auf stderr; der Lauf arbeitet auf diesem Branch
/// weiter.
#[test]
fn abgestuerzter_lauf_auf_fremdem_branch_bleibt_dort_und_warnt() {
    let ws = tmp_dir("stray_branch_fremd");
    let ws_str = ws.to_string_lossy().to_string();
    agentkit_work::git::init_repo_with_commit(&ws_str);
    let project_id = create_projekt_git_isoliert_mit_erledigtem_item(&ws);

    let out = std::process::Command::new("git")
        .args(["checkout", "-b", "feature/unabhaengig"])
        .current_dir(&ws_str)
        .output()
        .expect("git checkout ausführbar");
    assert!(out.status.success());

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    // Ein zusammenhängender Ausschnitt statt zweier unabhängiger `contains`
    // (Befund des Code-Reviews) — pinnt, dass BEIDE Fragmente aus derselben
    // Warnmeldung stammen.
    assert!(
        err.contains(
            "[WARNUNG] Repository steht auf Branch 'feature/unabhaengig', nicht auf dem \
             Ausgangsbranch 'main'"
        ),
        "stderr sollte auf dem fremden Branch warnen: {err}"
    );
    assert_eq!(
        agentkit_work::git::current_branch(&ws_str).unwrap(),
        "feature/unabhaengig",
        "ein fremder Branch wird nicht stillschweigend gewechselt"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Regressionstest zum schwersten Befund des Code-Reviews: ein abgestürzter
/// Versuch committet nie (das übernimmt erst `record_success`), uncommittete
/// Änderungen auf dem stehen gebliebenen Item-Branch sind also der
/// REGELFALL, nicht die Ausnahme. Ein automatischer Checkout hätte sie —
/// sofern sie mit dem Ausgangsbranch nicht kollidieren — klaglos MIT
/// hinübergenommen: kein Git-Fehler, aber eine stille Verschiebung der
/// Provenance. `run` muss das als harten Fehler behandeln, nicht als
/// automatisch lösbaren Fall.
#[test]
fn abgestuerzter_lauf_mit_uncommitteten_aenderungen_auf_dem_item_branch_bricht_hart_ab() {
    let ws = tmp_dir("stray_branch_dirty");
    let ws_str = ws.to_string_lossy().to_string();
    let base = agentkit_work::git::init_repo_with_commit(&ws_str);
    let project_id = create_projekt_git_isoliert_mit_erledigtem_item(&ws);

    let item_branch = agentkit_work::item_branch_name(&project_id, "W-1");
    agentkit_work::git::ensure_item_branch(&ws_str, &item_branch, &base).unwrap();
    std::fs::write(ws.join("halbfertig.txt"), "nie committet").unwrap();

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}");
    assert!(
        err.contains("[FEHLER]") && err.contains("uncommitteten Änderungen"),
        "stderr: {err}"
    );
    // Kein Checkout versucht: Branch und Datei bleiben unangetastet stehen.
    assert_eq!(
        agentkit_work::git::current_branch(&ws_str).unwrap(),
        item_branch
    );
    assert!(ws.join("halbfertig.txt").exists());

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn create_mit_items_datei_legt_items_in_dateireihenfolge_mit_abhaengigkeiten_an() {
    let ws = tmp_dir("create_items");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Analyse",
                "description": "Analysiere das Problem.",
                "kind": "analysis"
            },
            {
                "title": "Umsetzung",
                "description": "Setze die Lösung um.",
                "kind": "implementation",
                "depends_on": ["W-1"]
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Vorhaben",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.items["W-1"].title, "Analyse");
    assert!(snapshot.items["W-1"].dependencies.is_empty());
    assert_eq!(snapshot.items["W-2"].title, "Umsetzung");
    assert_eq!(snapshot.items["W-2"].dependencies, vec!["W-1".to_string()]);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn items_datei_mit_vorwaerts_verweis_wird_mit_exit_1_und_indexnennung_abgelehnt() {
    let ws = tmp_dir("create_items_vorwaerts");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Zuerst",
                "description": "Verweist nach vorn — ungültig.",
                "kind": "analysis",
                "depends_on": ["W-2"]
            },
            {
                "title": "Danach",
                "description": "Kommt erst danach.",
                "kind": "implementation"
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, _out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Vorhaben2",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(
        err.contains("Position 1"),
        "Meldung soll den Index/die Position nennen: {err}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------ status

#[test]
fn status_mit_format_json_schreibt_genau_ein_parsbares_json_dokument() {
    let ws = tmp_dir("status_json");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, _err) = run_cli(
        &args(&[
            "create",
            "--title",
            "Statusprojekt",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success);
    let project_id = out.trim().to_string();

    let (code, out, err) = run_cli(
        &args(&["status", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "genau eine Zeile/ein Dokument auf stdout: {out:?}"
    );
    let doc: Value = serde_json::from_str(lines[0]).expect("gültiges JSON");
    assert_eq!(doc["project_id"], json!(project_id));
    assert_eq!(doc["run_id"], json!("R-1"));

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------- items

#[test]
fn items_kennzeichnet_item_mit_endgueltig_gescheiterter_abhaengigkeit_als_blockiert() {
    let ws = tmp_dir("items_blocked");
    let store_dir = ws.join(".agentkit").join("work").join("demo");
    let store = agentkit_work::WorkStore::open(&store_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::ProjectCreated {
            project: agentkit_work::WorkProject {
                id: "demo".into(),
                title: "Demo".into(),
                objective: "Ziel".into(),
                workspace: ws.to_string_lossy().to_string(),
                status: agentkit_work::ProjectStatus::Active,
                created_at_ms: 0,
                budget: agentkit_work::WorkBudget::default(),
                git_isolation: false,
            },
        })
        .unwrap();
    store
        .submit(agentkit_work::WorkEvent::RunStarted {
            run: agentkit_work::WorkRun {
                id: "R-1".into(),
                project_id: "demo".into(),
                status: agentkit_work::RunStatus::Running,
                started_at_ms: 0,
                completed_at_ms: None,
                base_revision: None,
                base_branch: None,
                completion_reason: None,
            },
        })
        .unwrap();
    let make_item = |id: &str, deps: Vec<&str>, max_attempts: u32| agentkit_work::WorkItem {
        id: id.into(),
        run_id: "R-1".into(),
        title: format!("Item {id}"),
        description: "Beschreibung".into(),
        kind: agentkit_work::WorkItemKind::Implementation,
        status: agentkit_work::WorkItemStatus::Pending,
        priority: 5,
        seq: agentkit_work::id_order(id),
        required_role: None,
        dependencies: deps.into_iter().map(String::from).collect(),
        acceptance_criteria: vec![],
        verification_policy: agentkit_work::VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        executor: agentkit_work::ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts,
        updated_at_ms: 0,
    };
    // W-1: max_attempts=1, wird direkt endgültig gescheitert (Failed, attempt_count>=max_attempts).
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: make_item("W-1", vec![], 1),
        })
        .unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemClaimed {
            item: "W-1".into(),
            agent: "worker-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 999_999_999,
            at_ms: 0,
        })
        .unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemFailed {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 0,
        })
        .unwrap();
    // W-2 hängt von W-1 ab — W-1 ist endgültig blockiert (Failed, keine Versuche mehr).
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: make_item("W-2", vec!["W-1"], 3),
        })
        .unwrap();
    // Erzwungen durch Befund 1 (Sperrdatei): 'items' unten öffnet denselben
    // Store erneut — ohne dieses `drop` hielte der direkte Store-Zugriff
    // hier oben die Sperre, und der CLI-Aufruf schlüge mit "gesperrt" fehl.
    drop(store);

    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&["items", "demo", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    let list = doc.as_array().expect("Liste");
    let w2 = list
        .iter()
        .find(|it| it["id"] == json!("W-2"))
        .expect("W-2 vorhanden");
    assert_eq!(w2["blocked"], json!(true), "{w2:?}");

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------ events

#[test]
fn events_mit_tail_2_liefert_genau_die_letzten_zwei_ereignisse() {
    let ws = tmp_dir("events_tail");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([{"title": "Einziges", "description": "Beschreibung", "kind": "analysis"}])
            .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    // Journal danach: project_created, run_started, work_item_created — 3 Zeilen.
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "TailDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "events",
            &project_id,
            "-w",
            &ws_str,
            "--tail",
            "2",
            "--format",
            "json",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    let list = doc.as_array().expect("Liste");
    assert_eq!(list.len(), 2, "{list:?}");
    assert_eq!(list[1]["event"]["kind"], json!("work_item_created"));
    assert_eq!(list[0]["event"]["kind"], json!("run_started"));

    std::fs::remove_dir_all(&ws).ok();
}

/// Der CLI-seitige Regressionstest zu Befund 1: vor der Korrektur ersetzte
/// jeder `store.checkpoint()`-Aufruf das gesamte Journal durch eine einzige
/// Snapshot-Zeile — der Runner checkpointet nach JEDEM Work Item, also zeigte
/// `agentkit work events` nach einem vollständigen Lauf nur noch genau eine
/// Zeile ("snapshot"). Nach dem Fix bleibt die gesamte Zeitleiste erhalten.
#[test]
fn events_liefert_nach_vollstaendigem_lauf_die_gesamte_zeitleiste_inklusive_checkpoints() {
    let ws = tmp_dir("events_nach_lauf");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "EventsNachLauf",
            "--objective",
            "Teste die Zeitleiste nach einem vollständigen Lauf.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis als Artefakt ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "work_artifact",
            &json!({
                "kind": "code",
                "filename": "ergebnis.txt",
                "content": "fertig!",
                "summary": "Ergebnis abgelegt"
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei geschrieben.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    // Großzügiges --tail: es geht darum, dass MEHR als eine Zeile da ist,
    // nicht um eine exakte Zeilenzahl (die hinge an Runner-Interna).
    let (code, out, err) = run_cli(
        &args(&["events", &project_id, "-w", &ws_str, "--tail", "1000"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() > 1,
        "nach einem vollständigen Lauf muss mehr als eine Journal-Zeile sichtbar sein: {out}"
    );
    for expected in [
        "work_item_claimed",
        "work_item_completed",
        "checkpoint_created",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(expected)),
            "erwartete Ereignisart '{expected}' fehlt in der Zeitleiste:\n{out}"
        );
    }

    std::fs::remove_dir_all(&ws).ok();
}

/// Regressionstest zu Befund 1 des Vereinfachungs-Reviews: `WorkEvent::
/// GitCommitted` wurde ersatzlos entfernt (No-op in `state::apply`, reine
/// Dopplung des `ArtifactCreated`-Artefakts mit `ArtifactKind::GitCommit`).
/// Die sprechende Zeitleisten-Zeile, die vorher das eigene Ereignis lieferte,
/// baut `cmd_events` jetzt direkt aus diesem Artefakt (Anzeigelogik statt
/// Journal-Schema) — dieser Test belegt, dass `agentkit work events` nach
/// einem git-isolierten Commit tatsächlich eine Zeile mit Commit UND Branch
/// zeigt, nicht nur die generische `artifact_created`-Zeile.
#[test]
fn events_zeigt_nach_git_commit_eine_sprechende_zeile_mit_commit_und_branch() {
    let ws = tmp_dir("events_git_commit");
    let ws_str = ws.to_string_lossy().to_string();
    agentkit_work::git::init_repo_with_commit(&ws_str);
    // Bewusst KEIN `.gitignore` für `.agentkit/`: `git::is_clean` nimmt das
    // eigene, gerade offene Journal dieser Laufzeit über eine
    // Pathspec-Ausschlussregel selbst aus der Sauberkeitsprüfung aus (siehe
    // `git.rs`) — ein früherer Workaround, der sich hier selbst ein
    // `.gitignore` committete, ist damit nicht mehr nötig und wäre nur noch
    // ein Test, der den eigentlichen Fehler verdeckt hätte.

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "EventsGitCommit",
            "--objective",
            "Lege eine Datei an und committe sie git-isoliert.",
            "-w",
            &ws_str,
            "--git-isolation",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Datei anlegen",
                "description": "Lege feature.txt an.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "write_file",
            &json!({"path": "feature.txt", "content": "neu"}).to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei angelegt und committet.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;

    let (code, _out, err) = run_cli(&args(&["run", &project_id, "-w", &ws_str]), deps_with(llm));
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, out, err) = run_cli(
        &args(&["events", &project_id, "-w", &ws_str, "--tail", "1000"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    // W-1 ist das automatisch angelegte Planungs-Item ("Vorhaben zerlegen"),
    // das `work_add_item` aufruft — unser eigenes Item über `write_file` ist
    // W-2, siehe die tatsächliche Zeitleiste im Fehlschlagsfall dieses Tests.
    let expected_branch = format!("work/{project_id}/W-2");
    assert!(
        out.lines().any(|l| l.contains("git_commit: Commit ")
            && l.contains(&format!("Branch '{expected_branch}'"))),
        "erwarte eine sprechende git_commit-Zeile mit Branch '{expected_branch}': {out}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------- retry

#[test]
fn retry_auf_nicht_gescheitertes_item_wird_abgelehnt() {
    let ws = tmp_dir("retry_reject");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "RetryDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: agentkit_work::WorkItem {
                id: "W-1".into(),
                run_id: "R-1".into(),
                title: "Pending Item".into(),
                description: "Beschreibung".into(),
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
    drop(store);

    let (code, _out, err) = run_cli(
        &args(&["retry", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("nicht 'failed'"), "{err}");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn retry_auf_gescheitertes_item_mit_verbleibenden_versuchen_setzt_es_auf_pending() {
    let ws = tmp_dir("retry_accept");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "RetryDemo2",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemCreated {
            item: agentkit_work::WorkItem {
                id: "W-1".into(),
                run_id: "R-1".into(),
                title: "Failed Item".into(),
                description: "Beschreibung".into(),
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
            item: "W-1".into(),
            agent: "worker-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 999_999_999,
            at_ms: 0,
        })
        .unwrap();
    store
        .submit(agentkit_work::WorkEvent::WorkItemFailed {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 0,
        })
        .unwrap();
    drop(store);

    let (code, out, err) = run_cli(
        &args(&["retry", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("'pending'"), "{out}");

    let store2 = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store2.snapshot().items["W-1"].status,
        agentkit_work::WorkItemStatus::Pending
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------ budget

#[test]
fn budget_mit_max_steps_aendert_nur_dieses_feld_und_laesst_die_uebrigen_stehen() {
    let ws = tmp_dir("budget_max_steps");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "BudgetFeld",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "budget",
            &project_id,
            "-w",
            &ws_str,
            "--max-steps",
            "80",
            "--format",
            "json",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    let default_budget = agentkit_work::WorkBudget::default();
    assert_eq!(doc["max_steps_per_attempt"], json!(80));
    assert_eq!(
        doc["max_attempts_per_item"],
        json!(default_budget.max_attempts_per_item),
        "unveränderte Felder bleiben beim Default: {doc}"
    );
    assert_eq!(doc["max_wall_time_secs"], Value::Null);
    assert_eq!(doc["max_work_items"], Value::Null);

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store
            .snapshot()
            .project
            .as_ref()
            .unwrap()
            .budget
            .max_steps_per_attempt,
        80
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn budget_ohne_flags_zeigt_nur_an_und_journalt_nichts() {
    let ws = tmp_dir("budget_readonly");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "BudgetReadonly",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let seq_vorher = agentkit_work::WorkStore::open(&project_dir)
        .unwrap()
        .snapshot()
        .seq;

    let (code, out, err) = run_cli(&args(&["budget", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("max_steps_per_attempt"), "{out}");

    let seq_nachher = agentkit_work::WorkStore::open(&project_dir)
        .unwrap()
        .snapshot()
        .seq;
    assert_eq!(
        seq_vorher, seq_nachher,
        "reines Anzeigen darf die Sequenznummer nicht bewegen"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Regressionstest zum Modellierungsfehler: die CLI hat `--max-steps` früher
/// über ein ZWEITES `ProjectCreated` umgesetzt. Seit `state::apply` ein
/// zweites `ProjectCreated` ablehnt, würde ein Rückfall auf den alten Weg den
/// Lauf mit `ExitCode::GeneralError` scheitern lassen. Der Fix journalt
/// stattdessen `BudgetUpdated`; der Lauf muss also weiterhin normal
/// durchlaufen UND das Budget tatsächlich ändern.
///
/// Mechanische Anpassung (Befund 2 der Handprobe): das LLM-Skript legt jetzt
/// über `work_add_item` ein echtes Folge-Item an und schließt es ab, statt
/// nur einen Planungszug ohne jedes Folge-Item zu fahren. Mit der Korrektur
/// zu Befund 2 (`scheduler::decide`) wäre Letzteres `CompletionReason::Blocked`
/// (Exit `GeneralError`) — unabhängig von diesem Test hier, der ausschließlich
/// `BudgetUpdated` vs. ein zweites `ProjectCreated` prüfen soll. Die
/// Assertions selbst (Exit-Code, `max_steps_per_attempt`) sind unverändert.
#[test]
fn run_mit_max_steps_journalt_budget_updated_statt_eines_zweiten_project_created() {
    let ws = tmp_dir("run_max_steps_regression");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "MaxStepsRegression",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::text("Erledigt.")],
    ])) as Arc<dyn Llm>;
    let (code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_str,
            "--max-steps",
            "5",
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert_eq!(
        code,
        ExitCode::Success,
        "stderr: {err} — ein zweites 'ProjectCreated' würde hier mit \
         ExitCode::GeneralError scheitern"
    );

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store
            .snapshot()
            .project
            .as_ref()
            .unwrap()
            .budget
            .max_steps_per_attempt,
        5
    );

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------------------- list

/// Regressionsschutz: `list` hatte bisher keinen eigenen Positivtest — die
/// bekannten Flags (`-w`, `--format`) müssen nach der Umstellung auf eine
/// pro-Unterkommando geprüfte Flag-Menge (Befund 1) weiterhin gültig bleiben.
#[test]
fn list_mit_bekannten_flags_bleibt_gueltig() {
    let ws = tmp_dir("list_positive");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, _out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ListDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, out, err) = run_cli(
        &args(&["list", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc.as_array().expect("Liste").len(), 1);

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------- pause

/// Regressionsschutz: `pause` hatte bisher keinen eigenen Positivtest.
#[test]
fn pause_mit_bekannten_flags_bleibt_gueltig() {
    let ws = tmp_dir("pause_positive");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "PauseDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    // Ein frischer Lauf ist 'running' — 'pause' lehnt das ab (siehe
    // `cmd_pause`-Kommentar). Für einen sauberen Positivtest der FLAGS wird
    // der Lauf zuerst direkt über den Store pausiert, danach greift 'pause'
    // erneut (erlaubt: ein bereits pausierter Lauf darf erneut pausiert werden).
    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::RunPaused {
            run: "R-1".to_string(),
            reason: "Testvorbereitung".to_string(),
            at_ms: 0,
        })
        .unwrap();
    drop(store);

    let (code, out, err) = run_cli(&args(&["pause", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("pausiert"), "{out}");

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------- unbekannte Flags

/// Befund 1 des Security-Reviews, von Hand nachgewiesen:
/// `agentkit work status test-zwei --dies-gibt-es-nicht` lieferte vor der
/// Korrektur eine normale Statusausgabe mit Exit 0 — kein Hinweis auf den
/// Tippfehler. Nach der Korrektur ist ein unbekanntes Flag ein Fehler, der
/// das Flag wörtlich nennt.
#[test]
fn status_mit_unbekanntem_flag_wird_mit_exit_1_und_flagname_abgelehnt() {
    let ws = tmp_dir("status_unknown_flag");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "UnknownFlagDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let (code, out, err) = run_cli(
        &args(&["status", &project_id, "-w", &ws_str, "--dies-gibt-es-nicht"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(
        out.is_empty(),
        "kein Ergebnis auf stdout bei Parse-Fehler: {out:?}"
    );
    assert!(
        err.contains("--dies-gibt-es-nicht"),
        "Meldung soll das unbekannte Flag wörtlich nennen: {err}"
    );
    assert!(
        err.contains("--help"),
        "Meldung soll auf --help verweisen: {err}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Erfasst denselben Befund auch bei `run` — der Umbau muss ALLE
/// Unterkommandos treffen, nicht nur `status`.
#[test]
fn run_mit_unbekanntem_flag_wird_mit_exit_1_abgelehnt() {
    let (code, _out, err) = run_cli(
        &args(&["run", "irgendein-projekt", "--dies-gibt-es-nicht"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("--dies-gibt-es-nicht"), "{err}");
}

/// Erfasst denselben Befund auch bei `create`.
#[test]
fn create_mit_unbekanntem_flag_wird_mit_exit_1_abgelehnt() {
    let (code, _out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "X",
            "--objective",
            "Y",
            "--dies-gibt-es-nicht",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("--dies-gibt-es-nicht"), "{err}");
}

/// Ein Flag, das bei EINEM Unterkommando gültig ist (`--items` bei `create`),
/// muss bei einem ANDEREN (`status`) trotzdem als unbekannt abgelehnt werden
/// — jedes Unterkommando kennt nur seine eigene Flag-Menge.
#[test]
fn status_mit_andernorts_gueltigem_aber_hier_unzulaessigem_flag_wird_abgelehnt() {
    let ws = tmp_dir("status_items_flag");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ItemsFlagDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let (code, _out, err) = run_cli(
        &args(&["status", &project_id, "--items", "irrelevant.json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("--items"), "{err}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Regressionsschutz gegen einen zu strengen Parser: alle bei `run` bekannten
/// Schalter (`-y`, `--steps`, `--demo`) zusammen dürfen nicht als unbekanntes
/// Flag abgelehnt werden.
#[test]
fn run_mit_allen_bekannten_schaltern_bleibt_gueltig() {
    let ws = tmp_dir("run_switches");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "SwitchDemo",
            "--objective",
            "Ziel, das direkt erledigt ist.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Nichts zu tun, alles bereits erledigt.",
    )]])) as Arc<dyn Llm>;
    let (_code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_str,
            "-y",
            "--steps",
            "--demo",
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert!(
        !err.contains("unbekanntes Flag"),
        "bekannte Schalter dürfen nicht als unbekanntes Flag abgelehnt werden: {err}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------------- --dry-run

/// Befund 2 des Security-Reviews: `--dry-run` griff bei `work run` bisher
/// gar nicht (die CLI setzte `dry_run: false` fest verdrahtet). Beobachtbare
/// Prüfnaht: ein destruktives Tool (`write_file`, per `is_likely_destructive`
/// erkannt) wird mit `--dry-run` NICHT ausgeführt, sondern durch
/// `ToolRegistry::dry_run_blocking` durch einen "[dry-run] … blockiert"-
/// Hinweistext ersetzt. Damit das im Test sichtbar wird, läuft `run` mit
/// `--steps` (sonst erscheinen einzelne Tool-Ereignisse gar nicht auf
/// stderr, siehe `render_progress`) — das ist die engste vorhandene Naht,
/// ohne `CodingAgentExecutor` direkt aus dem CLI-Test heraus zu konstruieren.
#[test]
fn run_mit_dry_run_blockiert_ein_destruktives_tool_und_erreicht_den_executor() {
    let ws = tmp_dir("run_dry_run");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([{
            "title": "Schreibversuch",
            "description": "Versucht, eine Datei zu schreiben.",
            "kind": "implementation"
        }])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "DryRunDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "write_file",
            &json!({"path": "sollte-nicht-existieren.txt", "content": "pwn"}).to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c2",
            "work_submit",
            &json!({"summary": "Versuch im Dry-Run.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;

    let (code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_str,
            "--dry-run",
            "--steps",
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(
        err.contains("[dry-run]") && err.contains("write_file"),
        "die Sperre muss den Executor erreicht haben (Tool-Ergebnis auf stderr): {err}"
    );
    assert!(
        !ws.join("sollte-nicht-existieren.txt").exists(),
        "--dry-run darf die Datei nicht wirklich anlegen"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ----------------------------------------------------- Workspace-Warnung

/// Befund 3 des Security-Reviews: `-w`/`--dir` dienen bei `run` nur zum
/// AUFFINDEN des Projekts, ausgeführt wird immer im PERSISTIERTEN
/// `project.workspace` — das ist gewollt, aber bisher stumm. Verschiebt sich
/// das Projektverzeichnis (hier simuliert über zwei verschiedene, beide real
/// existierende Verzeichnisse plus `--dir`, das die Root direkt adressiert),
/// muss eine deutliche Warnung mit BEIDEN Pfaden auf stderr erscheinen —
/// keine Verhaltensänderung, der Lauf läuft trotzdem weiter.
#[test]
fn abweichender_workspace_zwischen_aufruf_und_persistiertem_projekt_warnt_auf_stderr() {
    let ws_a = tmp_dir("mismatch_a");
    let ws_a_str = ws_a.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "MismatchDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_a_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let ws_b = tmp_dir("mismatch_b");
    let ws_b_str = ws_b.to_string_lossy().to_string();
    let root_dir = ws_a.join(".agentkit").join("work");
    let root_dir_str = root_dir.to_string_lossy().to_string();

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Nichts zu tun, alles bereits erledigt.",
    )]])) as Arc<dyn Llm>;
    let (_code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_b_str,
            "--dir",
            &root_dir_str,
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert!(err.contains("WARNUNG"), "{err}");
    assert!(
        err.contains(&ws_a_str) && err.contains(&ws_b_str),
        "Warnung soll BEIDE Pfade nennen: {err}"
    );

    std::fs::remove_dir_all(&ws_a).ok();
    std::fs::remove_dir_all(&ws_b).ok();
}

/// Beschreibt die vorher fehlende Funktionslücke: ein wegen `max_work_items`
/// pausierter Lauf hatte keinen Weg zurück, weil es kein Kommando gab, das
/// Budget zu ändern. `budget --max-items` ist genau dieser Weg.
#[test]
fn budget_max_items_erhoehen_bringt_einen_budget_exceeded_lauf_wieder_voran() {
    let ws = tmp_dir("budget_stillstand");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "BudgetStillstand",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--max-items",
            "1",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    // Erster Lauf: das implizite Planungs-Item zählt schon gegen
    // max_work_items=1 — Budget ist VOR jedem Versuch erschöpft.
    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("budget_exceeded"));
    assert_eq!(
        doc["attempts"],
        json!(0),
        "vor der Erhöhung darf kein einziger Versuch laufen: {doc}"
    );

    let (code, out, err) = run_cli(
        &args(&[
            "budget",
            &project_id,
            "-w",
            &ws_str,
            "--max-items",
            "5",
            "--format",
            "json",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["max_work_items"], json!(5));

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Nichts zu zerlegen, alles erledigt.",
    )]])) as Arc<dyn Llm>;
    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert!(
        doc["attempts"].as_u64().unwrap() > 0,
        "nach der Erhöhung muss der Lauf mindestens einen Versuch machen: {doc} (exit {code:?}, stderr: {err})"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Ersetzt `list_nennt_gesperrte_projekte_auf_stderr_statt_sie_stillschweigend_auszulassen`
/// (Code-Review-Nachtrag zu Befund 1 der Vorphase): DAMALS sperrte `work.lock`
/// auch lesende Kommandos, `list` musste ein gerade laufendes Projekt deshalb
/// überspringen und wenigstens auf stderr nennen. Befund 0 DIESER Phase hebt
/// genau das auf — `list` liest jetzt sperrfrei (`WorkStore::open_read_only`)
/// und zeigt ein gerade laufendes Projekt normal an, ohne stderr-Hinweis. Der
/// alte Test prüfte exakt das Gegenteil der jetzt gewollten Anforderung; er
/// wird hier durch den Regressionstest zur neuen Anforderung ersetzt, nicht
/// nur mechanisch angepasst (im Bericht genannt).
#[test]
fn list_zeigt_ein_gerade_laufendes_projekt_ohne_stderr_hinweis() {
    let ws = tmp_dir("list_locked");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ListLocked",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();
    let project_dir = ws.join(".agentkit").join("work").join(&project_id);

    // Simuliert ein laufendes `agentkit work run` in einem anderen Terminal:
    // ein zweiter Store hält die Schreibsperre, während `list` liest.
    let running = agentkit_work::WorkStore::open(&project_dir).unwrap();

    let (code, out, err) = run_cli(
        &args(&["list", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    let ids: Vec<&str> = doc
        .as_array()
        .expect("Liste")
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![project_id.as_str()],
        "das laufende Projekt muss trotz Schreibsperre in der Liste erscheinen"
    );
    assert!(
        !err.contains("gesperrt"),
        "kein 'gesperrt'-Hinweis mehr nötig, sperrfreies Lesen kennt den Fall nicht: {err}"
    );

    drop(running);
    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------- Befund 1: --force

/// `run --force` übernimmt eine zurückgebliebene `work.lock` gewaltsam — der
/// Ausweg, wenn ein früherer Prozess durch `SIGKILL`/Absturz gestorben ist,
/// ohne sie selbst zu entfernen. Simuliert das über `std::mem::forget`: der
/// Store fällt NICHT sauber aus dem Scope (kein `Drop`), die Sperrdatei
/// bleibt liegen wie nach einem harten Absturz.
#[test]
fn run_mit_force_uebernimmt_eine_zurueckgebliebene_sperre() {
    let ws = tmp_dir("force_run");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ForceRun",
            "--objective",
            "Ziel, das direkt erledigt ist.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();
    let project_dir = ws.join(".agentkit").join("work").join(&project_id);

    // Absturz-Ersatz: eine offene Sperre zurücklassen, ohne sie sauber
    // freizugeben.
    let leaked = agentkit_work::WorkStore::open(&project_dir).unwrap();
    std::mem::forget(leaked);

    // Ohne '--force' scheitert 'run' an der Sperre.
    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}");
    assert!(
        err.contains("gesperrt") || err.contains("work.lock"),
        "{err}"
    );

    // Mit '--force' übernimmt 'run' die Sperre und arbeitet normal weiter —
    // die Planung muss ein Folge-Item anlegen, sonst endet der Lauf legitim
    // 'blocked' (siehe `run_mit_planungs_item_ohne_folge_items_endet_blockiert_mit_exit_1`).
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Umsetzung",
                "description": "Setze das Ergebnis um.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::text("Erledigt.")],
    ])) as Arc<dyn Llm>;
    let (code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_str,
            "--force",
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert!(
        !err.contains("gesperrt"),
        "'--force' hätte die Sperre übernehmen müssen: {err}"
    );
    assert!(err.contains("WARNUNG") && err.contains("force"), "{err}");
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------- Befund 3: Lauf-lose Projekte

/// Befund 3 (Handprobe): `cmd_create` journalt `ProjectCreated` und
/// `RunStarted` GETRENNT — stirbt der Prozess dazwischen, bleibt ein Projekt
/// ohne Lauf zurück. Simuliert das, indem NUR `ProjectCreated` direkt über
/// den Store journalt wird (kein `create`-Aufruf, der immer beides macht).
#[test]
fn projekt_ohne_lauf_status_scheitert_nicht_und_run_traegt_r1_nach() {
    let ws = tmp_dir("kein_lauf");
    let ws_str = ws.to_string_lossy().to_string();
    let project_dir = ws.join(".agentkit").join("work").join("ohnelauf");
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    store
        .submit(agentkit_work::WorkEvent::ProjectCreated {
            project: agentkit_work::WorkProject {
                id: "ohnelauf".into(),
                title: "OhneLauf".into(),
                objective: "Ziel".into(),
                workspace: ws_str.clone(),
                status: agentkit_work::ProjectStatus::Active,
                created_at_ms: 0,
                budget: agentkit_work::WorkBudget::default(),
                git_isolation: false,
            },
        })
        .unwrap();
    drop(store);

    // 'status' darf trotz fehlenden Laufs nicht scheitern.
    let (code, out, err) = run_cli(
        &args(&["status", "ohnelauf", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["run_id"], Value::Null);

    // 'items' ebenso — leere Liste statt Fehler.
    let (code, out, err) = run_cli(
        &args(&["items", "ohnelauf", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc.as_array().expect("Liste").len(), 0);

    // 'run' trägt 'R-1' nach und arbeitet normal weiter statt zu scheitern.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis als Artefakt ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "work_artifact",
            &json!({
                "kind": "code",
                "filename": "ergebnis.txt",
                "content": "fertig!",
                "summary": "Ergebnis abgelegt"
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei geschrieben.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;
    let (code, out, err) = run_cli(
        &args(&["run", "ohnelauf", "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(
        err.contains("R-1") && err.contains("nachgetragen"),
        "Hinweis auf den nachgetragenen Lauf erwartet: {err}"
    );
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("all_items_done"));

    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert!(store.snapshot().runs.contains_key("R-1"));

    std::fs::remove_dir_all(&ws).ok();
}

// --------------------------------------------------- Befund 5: Workspace-Pfad

/// Befund 5 (Handprobe): `-w .` speicherte früher wörtlich den String "." in
/// `project.workspace` — er löst sich zur Laufzeit relativ zum JEWEILIGEN
/// Arbeitsverzeichnis auf. Nach der Korrektur speichert `create` den
/// kanonisierten, ABSOLUTEN Pfad. `--dir` überschreibt nur die Wurzel des
/// Projektverzeichnisses (siehe `work_root`), nicht den Workspace selbst —
/// der Test mutiert deshalb bewusst NICHT das Prozess-CWD (das wäre bei
/// `cargo test`s parallel laufenden Tests unsicher), sondern lässt "-w ."
/// einfach gegen das tatsächliche CWD des Testprozesses auflösen.
#[test]
fn create_mit_punkt_als_workspace_speichert_den_absoluten_pfad_statt_woertlich_punkt() {
    let ws = tmp_dir("dot_workspace");
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "DotWorkspace",
            "--objective",
            "Ziel",
            "-w",
            ".",
            "--dir",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let workspace = store.snapshot().project.as_ref().unwrap().workspace.clone();
    assert_ne!(
        workspace, ".",
        "gespeicherter Workspace darf nicht wörtlich '.' sein"
    );
    assert!(
        std::path::Path::new(&workspace).is_absolute(),
        "gespeicherter Workspace muss absolut sein: {workspace}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Befund 5, zweiter Teil: der Vergleich in `warn_workspace_mismatch` läuft
/// gegen den jetzt kanonisierten `project.workspace` — ein Projekt, das
/// (simuliert über einen physischen Verzeichnis-Wechsel via `--dir`) an
/// einer anderen Stelle aufgerufen wird als der beim Anlegen persistierte
/// Workspace, muss die Warnung zuverlässig auslösen.
#[test]
fn workspace_warnung_feuert_zuverlaessig_gegen_den_kanonisierten_persistierten_pfad() {
    let ws_a = tmp_dir("kanon_mismatch_a");
    let ws_a_str = ws_a.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "KanonMismatch",
            "--objective",
            "Ziel",
            "-w",
            &ws_a_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let root_dir = ws_a.join(".agentkit").join("work");
    let ws_b = tmp_dir("kanon_mismatch_b");
    let ws_b_str = ws_b.to_string_lossy().to_string();

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("Nichts zu tun.")]])) as Arc<dyn Llm>;
    let (_code, _out, err) = run_cli(
        &args(&[
            "run",
            &project_id,
            "-w",
            &ws_b_str,
            "--dir",
            &root_dir.to_string_lossy(),
            "--format",
            "json",
        ]),
        deps_with(llm),
    );
    assert!(err.contains("WARNUNG"), "{err}");
    assert!(err.contains(&ws_a_str) && err.contains(&ws_b_str), "{err}");

    std::fs::remove_dir_all(&ws_a).ok();
    std::fs::remove_dir_all(&ws_b).ok();
}

// -------------------------------------------------------- unbekanntes Verb

#[test]
fn unbekanntes_unterkommando_gibt_exit_1_und_hilfetext_auf_stderr() {
    let (code, out, err) = run_cli(&args(&["nonsens"]), deps_stub());
    assert_eq!(code, ExitCode::GeneralError);
    assert!(out.is_empty());
    assert!(err.contains("Unbekanntes Unterkommando"), "{err}");
    assert!(err.contains("agentkit work"), "Hilfetext fehlt: {err}");
}

// ------------------------------------------------------------------ run/e2e

/// Ende-zu-Ende über die CLI: `create` (ohne `--items`, damit der
/// Planungszug greift) dann `run` mit geskriptetem `FakeLlm` — Planungszug
/// legt ein Umsetzungs-Item an, das zweite legt das Ergebnis ab und schließt
/// mit `work_submit` ab.
#[test]
fn vollstaendiger_run_ueber_die_cli_endet_mit_exit_0_und_all_items_done() {
    let ws = tmp_dir("run_e2e");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "RunDemo",
            "--objective",
            "Teste den CLI-Runner.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis als Artefakt ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "work_artifact",
            &json!({
                "kind": "code",
                "filename": "ergebnis.txt",
                "content": "fertig!",
                "summary": "Ergebnis abgelegt"
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei geschrieben.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;

    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("all_items_done"));

    std::fs::remove_dir_all(&ws).ok();
}

/// Der CLI-seitige Regressionstest zu Befund 2: ein Planungs-Item, das
/// erfolgreich abschließt, aber KEIN einziges Folge-Item anlegt, darf nicht
/// mit Exit 0 durchgehen — das Vorhaben ist nachweislich unbearbeitet
/// geblieben. Vor der Korrektur lieferte dieser Lauf `all_items_done`/Exit 0.
#[test]
fn run_mit_planungs_item_ohne_folge_items_endet_blockiert_mit_exit_1() {
    let ws = tmp_dir("blocked_ohne_folge_items");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "BlockedOhneFolgeItems",
            "--objective",
            "Ziel, das der Agent nicht zerlegt.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    // Der Planungszug antwortet direkt mit Text, ohne 'work_add_item'
    // aufzurufen — das Vorhaben bleibt vollständig unzerlegt.
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Nichts zu tun, alles bereits erledigt.",
    )]])) as Arc<dyn Llm>;
    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(
        code,
        ExitCode::GeneralError,
        "ein Lauf ohne ein einziges Folge-Item darf nicht mit Exit 0 enden: {out}, stderr: {err}"
    );
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("blocked"));

    std::fs::remove_dir_all(&ws).ok();
}

/// Der Gegentest zu Befund 2, damit die Korrektur nicht überkorrigiert: ein
/// Lauf, dessen Planungs-Item mindestens ein Folge-Item anlegt, das
/// tatsächlich abgeschlossen wird, bleibt `all_items_done` mit Exit 0.
#[test]
fn run_mit_mindestens_einem_abgeschlossenen_umsetzungs_item_bleibt_all_items_done_mit_exit_0() {
    let ws = tmp_dir("gegentest_all_items_done");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "GegentestAllItemsDone",
            "--objective",
            "Ziel, das in genau ein Folge-Item zerlegt wird.",
            "-w",
            &ws_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Umsetzung",
                "description": "Setze das Ergebnis um.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::text("Erledigt.")],
    ])) as Arc<dyn Llm>;
    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(llm),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("all_items_done"));

    std::fs::remove_dir_all(&ws).ok();
}

// ----------------------------------------------------------- approve/reject

/// Baut das FakeLlm-Skript für EINEN Versuch, der `work_submit` aufruft und
/// danach mit einer Textantwort endet — dasselbe Zwei-Zug-Muster wie in den
/// übrigen `run`-CLI-Tests dieser Datei.
fn single_attempt_llm(summary: &str) -> Arc<dyn Llm> {
    Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_submit",
            &json!({"summary": summary, "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>
}

/// Legt ein Vorhaben mit zwei vorab definierten Items an: 'W-1' mit Policy
/// 'human_approval', 'W-2' hängt von 'W-1' ab (Standard-Policy 'none').
fn create_project_mit_human_approval_item(ws: &std::path::Path) -> String {
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Wartet auf Freigabe",
                "description": "Braucht eine manuelle Freigabe.",
                "kind": "implementation",
                "verification": "human_approval"
            },
            {
                "title": "Folgeschritt",
                "description": "Läuft erst nach der Freigabe von W-1.",
                "kind": "implementation",
                "depends_on": ["W-1"]
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ApproveDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    out.trim().to_string()
}

#[test]
fn run_mit_human_approval_item_endet_mit_exit_1_und_wartet_auf_freigabe() {
    let ws = tmp_dir("approve_run_awaits");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);

    let (code, out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(single_attempt_llm("W-1 erledigt.")),
    );
    assert_eq!(code, ExitCode::GeneralError, "stdout: {out}, stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("awaiting_verification"));

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store.snapshot().items["W-1"].status,
        agentkit_work::WorkItemStatus::AwaitingVerification
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn approve_schliesst_das_item_ab_und_der_lauf_kommt_mit_resume_voran() {
    let ws = tmp_dir("approve_success");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str]),
        deps_with(single_attempt_llm("W-1 erledigt.")),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}");

    let (code, out, err) = run_cli(
        &args(&["approve", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("freigegeben"), "{out}");

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store.snapshot().items["W-1"].status,
        agentkit_work::WorkItemStatus::Completed
    );
    drop(store);

    // 'resume' erholt sich, sieht W-2 jetzt ready (W-1 completed) und bringt
    // den Lauf zu Ende.
    let (code, out, err) = run_cli(
        &args(&["resume", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_with(single_attempt_llm("W-2 erledigt.")),
    );
    assert_eq!(code, ExitCode::Success, "stdout: {out}, stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("gültiges JSON");
    assert_eq!(doc["reason"], json!("all_items_done"));

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn approve_auf_ein_item_das_nicht_wartet_gibt_exit_1() {
    let ws = tmp_dir("approve_wrong_state");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);
    // W-1 ist noch 'pending', wartet also nicht auf irgendeine Freigabe.

    let (code, _out, err) = run_cli(
        &args(&["approve", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(
        err.contains("wartet nicht auf eine manuelle Freigabe"),
        "{err}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn reject_ohne_reason_wird_mit_exit_1_abgelehnt() {
    let ws = tmp_dir("reject_missing_reason");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);

    let (code, _out, err) = run_cli(
        &args(&["reject", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("--reason"), "{err}");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn reject_mit_reason_setzt_es_zurueck_und_der_grund_erscheint_im_naechsten_arbeitspaket() {
    let ws = tmp_dir("reject_success");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);

    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str]),
        deps_with(single_attempt_llm("W-1 erledigt.")),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}");

    let (code, out, err) = run_cli(
        &args(&[
            "reject",
            "W-1",
            "-p",
            &project_id,
            "-w",
            &ws_str,
            "--reason",
            "sieht nicht vollständig aus",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("'pending'"), "{out}");

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.items["W-1"].status,
        agentkit_work::WorkItemStatus::Pending
    );
    assert_eq!(snapshot.items["W-1"].attempt_count, 1);

    // Der Grund landet im Arbeitspaket des NÄCHSTEN Versuchs (§12) — direkt
    // über die öffentliche `AgentWorkPackage`-API geprüft, ohne einen echten
    // zweiten Agentenlauf zu brauchen.
    let pkg = agentkit_work::AgentWorkPackage::build(&snapshot, "W-1", &ws_str, 40).unwrap();
    assert!(
        pkg.render().contains("sieht nicht vollständig aus"),
        "{}",
        pkg.render()
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------- independent_agent / Promotion

#[test]
fn create_mit_items_datei_akzeptiert_independent_agent_policy() {
    let ws = tmp_dir("items_independent_agent");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Unabhängig geprüft",
                "description": "Braucht eine unabhängige Prüfung.",
                "kind": "implementation",
                "verification": "independent_agent"
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "IndependentAgentDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store.snapshot().items["W-1"].verification_policy,
        agentkit_work::VerificationPolicy::IndependentAgent
    );

    std::fs::remove_dir_all(&ws).ok();
}

// --------------------------------------------------------- Schwarm (Phase 6)

/// `{"swarm": "review"}` legt das Item mit `ExecutorKind::Swarm` an — die
/// Vorlage selbst wird von diesem Crate nicht geprüft (§13: welche Vorlagen
/// es gibt, weiß nur das Frontend).
#[test]
fn create_mit_items_datei_akzeptiert_swarm_executor() {
    let ws = tmp_dir("items_swarm_executor");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Review durch Schwarm",
                "description": "Braucht mehrere Perspektiven.",
                "kind": "review",
                "executor": {"swarm": "review"}
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "SchwarmDemo",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store.snapshot().items["W-1"].executor,
        agentkit_work::ExecutorKind::Swarm {
            template: "review".to_string()
        }
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Ein Item ohne `executor`-Feld bleibt beim Default `SingleAgent` — Phase 6
/// darf das Verhalten aus Phase 5 nicht ändern.
#[test]
fn create_mit_items_datei_ohne_executor_feld_bleibt_single_agent() {
    let ws = tmp_dir("items_ohne_executor");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {"title": "Normal", "description": "Ohne executor-Feld.", "kind": "implementation"}
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "OhneExecutorFeld",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    assert_eq!(
        store.snapshot().items["W-1"].executor,
        agentkit_work::ExecutorKind::SingleAgent
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Eine unbekannte `executor`-Form wird mit Exit 1 abgelehnt und nennt die
/// erlaubten Formen — derselbe Vertrag wie bei einer unbekannten
/// `verification`-Form.
#[test]
fn items_datei_mit_unbekannter_executor_form_wird_mit_exit_1_und_erlaubten_formen_abgelehnt() {
    let ws = tmp_dir("items_unbekannter_executor");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Kaputt",
                "description": "Unbekannte executor-Form.",
                "kind": "implementation",
                "executor": "auf_gut_glueck"
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, _out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "KaputterExecutor",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("single_agent"), "{err}");
    assert!(err.contains("swarm"), "{err}");

    std::fs::remove_dir_all(&ws).ok();
}

/// `items` zeigt die Schwarm-Vorlage eines Items in Text- und JSON-Form an
/// (§13) — ein Einzelagenten-Item bleibt in der Textzeile unmarkiert.
#[test]
fn items_zeigt_die_schwarm_vorlage_eines_items_an() {
    let ws = tmp_dir("items_zeigt_schwarm");
    let ws_str = ws.to_string_lossy().to_string();
    let items_path = ws.join("items.json");
    std::fs::write(
        &items_path,
        json!([
            {
                "title": "Normal",
                "description": "Einzelagent.",
                "kind": "implementation"
            },
            {
                "title": "Erkundung",
                "description": "Mehrere Perspektiven.",
                "kind": "discovery",
                "executor": {"swarm": "discovery"}
            }
        ])
        .to_string(),
    )
    .unwrap();
    let items_str = items_path.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ItemsZeigenSchwarm",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--items",
            &items_str,
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let project_id = out.trim().to_string();

    let (code, out, err) = run_cli(&args(&["items", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        !lines[0].contains("SCHWARM"),
        "Einzelagenten-Item soll keine Schwarm-Markierung zeigen: {}",
        lines[0]
    );
    assert!(lines[1].contains("[SCHWARM: discovery]"), "{}", lines[1]);

    let (code, out, err) = run_cli(
        &args(&["items", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(doc[0]["executor"], json!("single_agent"));
    assert_eq!(doc[1]["executor"], json!("swarm:discovery"));

    std::fs::remove_dir_all(&ws).ok();
}

/// `approve` promotet verifizierte Claims, wenn ein Wissensgraph angebunden
/// ist (§11, Phase 5b) — der einzige Weg, dieses Verhalten über die CLI zu
/// prüfen, statt nur über `recovery::recover_pending_promotions` (siehe
/// `agentkit_work/tests/recovery.rs`) oder den Runner direkt (siehe
/// `agentkit_work/tests/runner.rs`).
#[test]
fn approve_promotet_verifizierte_claims_wenn_ein_graph_angebunden_ist() {
    let ws = tmp_dir("approve_promotion");
    let ws_str = ws.to_string_lossy().to_string();
    let project_id = create_project_mit_human_approval_item(&ws);

    let gateway = Arc::new(FakeGraph::default());
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_claim",
            &json!({
                "claims": [
                    {"subject": "X", "predicate": "verursacht", "object": "Y"}
                ]
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c2",
            "work_submit",
            &json!({"summary": "W-1 erledigt.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;
    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str]),
        deps_with_graph(llm, gateway.clone()),
    );
    assert_eq!(code, ExitCode::GeneralError, "stderr: {err}"); // wartet auf Freigabe

    let (code, out, err) = run_cli(
        &args(&["approve", "W-1", "-p", &project_id, "-w", &ws_str]),
        deps_with_graph(Arc::new(FakeLlm::new(vec![])), gateway.clone()),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("freigegeben"), "{out}");

    let project_dir = ws.join(".agentkit").join("work").join(&project_id);
    let store = agentkit_work::WorkStore::open(&project_dir).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.items["W-1"].status,
        agentkit_work::WorkItemStatus::Completed
    );
    assert!(
        snapshot.items["W-1"].claims_promoted,
        "'approve' muss die aufgezeichneten Claims promoten"
    );
    assert!(!gateway.promoted.lock().unwrap().is_empty());

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------ Befund 0: sperrfreies Lesen / watch

fn create_simple_project(name: &str) -> (std::path::PathBuf, String) {
    let ws = tmp_dir(name);
    let ws_str = ws.to_string_lossy().to_string();
    let (code, out, err) = run_cli(
        &args(&[
            "create",
            "--title",
            "ReadOnly",
            "--objective",
            "Ziel",
            "-w",
            &ws_str,
            "--max-wall-time",
            "3600",
            "--max-items",
            "10",
        ]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    (ws, out.trim().to_string())
}

/// Regressionstest zu Befund 0 der Handprobe: `status`/`items`/`events`/
/// `list` müssen funktionieren, während ein ANDERER Store dasselbe Projekt
/// SCHREIBEND offen hält (genau die Lage während eines laufenden `agentkit
/// work run`) — schreibende Kommandos (`run`) scheitern dabei WEITERHIN an
/// der Sperre.
#[test]
fn status_items_events_list_funktionieren_waehrend_ein_anderer_store_schreibend_offen_ist() {
    let (ws, project_id) = create_simple_project("readonly_reads");
    let ws_str = ws.to_string_lossy().to_string();
    let project_dir = ws.join(".agentkit").join("work").join(&project_id);

    // Simuliert ein laufendes 'agentkit work run' in einem zweiten Terminal.
    let writer = agentkit_work::WorkStore::open(&project_dir).unwrap();

    let (code, _out, err) = run_cli(&args(&["status", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(
        code,
        ExitCode::Success,
        "status muss trotz Sperre lesen: {err}"
    );

    let (code, _out, err) = run_cli(&args(&["items", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(
        code,
        ExitCode::Success,
        "items muss trotz Sperre lesen: {err}"
    );

    let (code, _out, err) = run_cli(&args(&["events", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(
        code,
        ExitCode::Success,
        "events muss trotz Sperre lesen: {err}"
    );

    let (code, out, err) = run_cli(
        &args(&["list", "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(
        code,
        ExitCode::Success,
        "list muss trotz Sperre lesen: {err}"
    );
    assert!(out.contains(&project_id), "{out}");

    // Schreibende Kommandos scheitern weiterhin an der Sperre.
    let (code, _out, err) = run_cli(
        &args(&["run", &project_id, "-w", &ws_str, "-y"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::GeneralError);
    assert!(err.contains("gesperrt"), "{err}");

    drop(writer);
    std::fs::remove_dir_all(&ws).ok();
}

/// Auftrag A: `--format json` gibt bei `watch` GENAU EIN JSON-Dokument aus
/// und kehrt zurück — keine Endlosschleife (die würde diesen Test hängen
/// lassen).
#[test]
fn watch_format_json_schreibt_genau_ein_dokument_und_kehrt_zurueck() {
    let (ws, project_id) = create_simple_project("watch_json");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(
        &args(&["watch", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "genau ein Dokument erwartet: {out}");
    let doc: Value = serde_json::from_str(lines[0]).expect("gültiges JSON");
    assert_eq!(doc["project_id"], project_id);

    std::fs::remove_dir_all(&ws).ok();
}

/// Auftrag A: ohne Terminal an stdout verhält sich `watch` wie ein einmaliges
/// `status` — kein Endlos-Redraw in eine Pipe. Der Testprozess hat nie ein
/// echtes Terminal an stdout (die Testumgebung fängt es ab), `watch` erkennt
/// das über `std::io::stdout().is_terminal()`. Würde stattdessen doch
/// geloopt, würde dieser Test nie zurückkehren (Timeout).
#[test]
fn watch_ohne_tty_verhaelt_sich_wie_ein_einmaliges_status() {
    let (ws, project_id) = create_simple_project("watch_no_tty");
    let ws_str = ws.to_string_lossy().to_string();

    let (code, out, err) = run_cli(&args(&["watch", &project_id, "-w", &ws_str]), deps_stub());
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(out.contains("Projekt"), "{out}");
    assert!(out.contains("Work Items"), "{out}");
    assert!(out.contains("Wartet auf"), "{out}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Auftrag B / Auftrag A: der JSON-Inhalt von `status` UND `watch` muss den
/// Budgetverbrauch (Wandzeit, Items), die Zahl der Versuche und die Zahl der
/// Artefakte tragen — nach einem echten, abgeschlossenen Lauf mit mindestens
/// einem Versuch und einem Artefakt, nicht nur strukturell bei null.
#[test]
fn status_und_watch_json_enthalten_budgetverbrauch_versuche_und_artefakte() {
    let (ws, project_id) = create_simple_project("budget_usage_json");
    let ws_str = ws.to_string_lossy().to_string();

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis als Artefakt ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "work_artifact",
            &json!({
                "kind": "code",
                "filename": "ergebnis.txt",
                "content": "fertig!",
                "summary": "Ergebnis abgelegt"
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei geschrieben.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ])) as Arc<dyn Llm>;
    let (code, _out, err) = run_cli(&args(&["run", &project_id, "-w", &ws_str]), deps_with(llm));
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, out, err) = run_cli(
        &args(&["status", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(doc["elapsed_wall_secs"].as_u64().is_some(), "{doc}");
    assert!(doc["item_count"].as_u64().unwrap() >= 2, "{doc}"); // Planung + Umsetzung
    assert!(doc["attempts_total"].as_u64().unwrap() >= 2, "{doc}");
    assert!(doc["artifacts_total"].as_u64().unwrap() >= 1, "{doc}");

    let (code, out, err) = run_cli(
        &args(&["watch", &project_id, "-w", &ws_str, "--format", "json"]),
        deps_stub(),
    );
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    let usage = &doc["budget_usage"];
    assert!(usage["elapsed_wall_secs"].as_u64().is_some(), "{doc}");
    assert!(usage["item_count"].as_u64().unwrap() >= 2, "{doc}");
    assert!(usage["attempts_total"].as_u64().unwrap() >= 2, "{doc}");
    assert!(usage["artifacts_total"].as_u64().unwrap() >= 1, "{doc}");

    std::fs::remove_dir_all(&ws).ok();
}
