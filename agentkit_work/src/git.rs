//! Dünne, synchrone Hülle um `std::process::Command` für die Git-Isolation
//! (Phase 7, §19). Genau die Operationen, die `runner.rs` braucht — kein
//! allgemeiner Git-Client, keine Abstraktion auf Vorrat (Guidelines §2/§4).
//!
//! Kein Shell-Aufruf: jede Funktion übergibt Argumente als Array direkt an
//! `git`, nie über `sh -c`/`cmd /C` — dieselbe Begründung wie bei
//! `runner::run_verification_command`. Jede Funktion gibt `Result<_, String>`
//! zurück; die Fehlermeldung enthält die kürzeste aussagekräftige Zeile von
//! `stderr` (bei Git meist die ERSTE nicht-leere Zeile, `fatal: …`/`error: …`
//! — anders als bei einem Testkommando, wo die LETZTE Zeile das Ergebnis
//! trägt, siehe `runner::last_meaningful_line`), nie den ganzen Dump.
//!
//! Kein `git2`/`libgit2` (Guidelines §4: keine neue Dependency ohne Not,
//! dieselbe Linie wie der Verzicht auf `rusqlite` in diesem Crate) — ein
//! installiertes `git`-Binary reicht, und `std::process::Command` hält den
//! C-Compiler draußen.

use std::process::{Command, Output};

/// Laufzeit-Identität für jeden Commit/Merge, den diese Runtime selbst
/// erzeugt. Bewusst NICHT die globale `user.name`/`user.email`-Konfiguration
/// der Maschine: eine frische Testumgebung (CI, Container, ein frisch
/// `git init`-tes Repo) hat oft keine gesetzt, und `git commit` bricht dann
/// hart mit "Please tell me who you are" ab — ein Vorhaben dürfte dann nie
/// git-isoliert laufen, ohne dass der Bediener erst manuell `git config`
/// aufruft. Jeder schreibende Aufruf übergibt die Identität deshalb PRO
/// AUFRUF (`-c user.name=… -c user.email=…`, siehe [`run_as_runtime`]),
/// unabhängig vom Zustand der Maschine oder des Repositories.
const RUNTIME_GIT_NAME: &str = "agentkit-work";
const RUNTIME_GIT_EMAIL: &str = "agentkit-work@localhost";

/// Führt `git <args>` im Workspace aus, ohne Shell — Argumente einzeln im
/// Array, kein zusammengesetzter String.
fn run(workspace: &str, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))
}

/// Wie [`run`], aber MIT der Laufzeit-Identität als Commit-Autor — für jede
/// Operation, die selbst einen Commit erzeugt (`commit`, `merge`).
fn run_as_runtime(workspace: &str, args: &[&str]) -> Result<Output, String> {
    let name_cfg = format!("user.name={RUNTIME_GIT_NAME}");
    let email_cfg = format!("user.email={RUNTIME_GIT_EMAIL}");
    let mut full: Vec<&str> = vec!["-c", &name_cfg, "-c", &email_cfg];
    full.extend_from_slice(args);
    run(workspace, &full)
}

/// Die kürzeste aussagekräftige Zeile aus `stderr` — bei Git meist die
/// ERSTE nicht-leere Zeile (`fatal: …`), weitere Zeilen sind meist nur
/// Zusatz-Hinweise (`hint: …`). Gekürzt auf eine knappe Länge, dasselbe
/// Muster wie `runner::last_meaningful_line`.
fn shortest_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("git-Kommando fehlgeschlagen (keine Fehlerausgabe)");
    agentkit::one_line(line, 200)
}

fn require_success(output: Output, context: &str) -> Result<String, String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!("{context}: {}", shortest_line(&output.stderr)))
    }
}

/// Ob `workspace` innerhalb eines Git-Arbeitsbaums liegt — die Prüfung hinter
/// `agentkit work create --git-isolation`: ein Vorhaben außerhalb eines
/// Git-Repos soll mit einer klaren deutschen Meldung abgelehnt werden, nicht
/// mit einem kryptischen Git-Fehler mitten im ersten Lauf.
pub fn is_repo(workspace: &str) -> bool {
    run(workspace, &["rev-parse", "--is-inside-work-tree"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Aktueller Commit (`git rev-parse HEAD`).
pub fn current_commit(workspace: &str) -> Result<String, String> {
    require_success(
        run(workspace, &["rev-parse", "HEAD"])?,
        "aktueller Commit nicht ermittelbar",
    )
}

/// Aktueller Branch (`git rev-parse --abbrev-ref HEAD`) — liefert das
/// wörtliche `"HEAD"`, wenn der Arbeitsbaum losgelöst (detached) ist.
pub fn current_branch(workspace: &str) -> Result<String, String> {
    require_success(
        run(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?,
        "aktueller Branch nicht ermittelbar",
    )
}

/// Ob der Arbeitsbaum sauber ist — keine offene, uncommittete Änderung
/// (verfolgt oder nicht). Grundlage für die harte Ablehnung vor dem Anlegen
/// eines Item-Branches (§19): fremde uncommittete Änderungen dürfen nicht in
/// einen Item-Commit geraten.
///
/// `.agentkit/` (die eigene Buchführung dieser Laufzeit — Journal und
/// Artefakte, siehe `agentkit_work/README.md` Abschnitt „Git-Isolation") ist
/// dabei bewusst ausgenommen: `agentkit work create` legt `work.jsonl` schon
/// VOR dem ersten Lauf im Workspace an, also unterhalb desselben
/// Arbeitsbaums. Ohne Ausnahme wäre der Arbeitsbaum nie sauber, sobald ein
/// Vorhaben existiert, und jeder `--git-isolation`-Lauf ab dem zweiten Item
/// schlüge mit „Arbeitsbaum nicht sauber" fehl — das Feature wäre ohne ein
/// vom Bediener selbst gepflegtes `.gitignore` unbenutzbar.
///
/// Der Ausschluss läuft über eine Pathspec-Ausschlussregel (`:(exclude)…`),
/// nicht per Nachfilterung der Textausgabe: `git status --porcelain` selbst
/// bietet genau diesen Mechanismus, um Pfade von vornherein weg­zulassen —
/// dieselbe Pathspec-Magie verwendet `commit_all` unten beim Staging. Das ist
/// robuster als ein String-Filter auf die Statuszeilen (der bei geänderten
/// Formaten oder umbenannten/verschobenen Pfaden brechen könnte) und drückt
/// die Absicht direkt in der Git-Anfrage aus statt im Ergebnis nachträglich.
pub fn is_clean(workspace: &str) -> Result<bool, String> {
    let out = require_success(
        run(
            workspace,
            &["status", "--porcelain", "--", ".", ":(exclude).agentkit"],
        )?,
        "Arbeitsbaum-Status nicht ermittelbar",
    )?;
    Ok(out.trim().is_empty())
}

/// Legt `branch` bei `start_point` an und wechselt sofort darauf — für den
/// ERSTEN Versuch eines Items. Existiert der Branch schon (ein Retry
/// desselben Items, siehe [`ensure_item_branch`]), scheitert das mit einem
/// Git-Fehler ("already exists") — bewusst kein Silent-Overwrite.
fn create_branch(workspace: &str, branch: &str, start_point: &str) -> Result<(), String> {
    require_success(
        run(workspace, &["checkout", "-b", branch, start_point])?,
        &format!("Branch '{branch}' nicht anlegbar"),
    )?;
    Ok(())
}

/// Wechselt auf einen vorhandenen Branch/eine Revision.
pub fn checkout(workspace: &str, branch_or_rev: &str) -> Result<(), String> {
    require_success(
        run(workspace, &["checkout", branch_or_rev])?,
        &format!("Wechsel auf '{branch_or_rev}' fehlgeschlagen"),
    )?;
    Ok(())
}

/// Ob `branch` als lokaler Branch existiert (`git rev-parse --verify --quiet
/// refs/heads/<branch>`) — `--quiet` unterdrückt Git-eigene Fehlerausgabe für
/// den erwarteten "existiert nicht"-Fall, der hier kein Fehler ist.
fn branch_exists(workspace: &str, branch: &str) -> Result<bool, String> {
    let refname = format!("refs/heads/{branch}");
    let output = run(workspace, &["rev-parse", "--verify", "--quiet", &refname])?;
    Ok(output.status.success())
}

/// Bereitet den Item-Branch für EINEN Versuch vor (§19): existiert er schon
/// (ein vorheriger, gescheiterter Versuch DESSELBEN Items — ein
/// gescheiterter Versuch verwirft seine Änderungen und kehrt zum
/// Ausgangsstand zurück, siehe `runner::GitAttemptCtx`, der Branch bleibt
/// also sauber am `start_point` stehen), wird nur gewechselt; sonst wird er
/// frisch angelegt. So braucht ein Retry keinen Löschen-und-neu-Anlegen-Umweg.
pub fn ensure_item_branch(workspace: &str, branch: &str, start_point: &str) -> Result<(), String> {
    if branch_exists(workspace, branch)? {
        checkout(workspace, branch)
    } else {
        create_branch(workspace, branch, start_point)
    }
}

/// Setzt den Arbeitsbaum HART auf `start_point` zurück und entfernt nicht
/// verfolgte Dateien — der Rollback nach einem gescheiterten Versuch (§19):
/// verworfen statt aufgehoben, damit der nächste Versuch sauber startet; die
/// Diagnose steht ohnehin im Journal (Failure) und in den Artefakten, nicht
/// im Arbeitsbaum.
///
/// `.agentkit` ist von `git clean` explizit ausgeschlossen (`-e .agentkit`):
/// es enthält das eigene, gerade OFFENE Journal dieser Laufzeit (siehe
/// `agentkit_work/README.md`) — ein `clean`, das es mit entfernt, würde dem
/// laufenden Prozess seine eigene Datenbasis unter den Füßen wegziehen.
pub fn discard_changes(workspace: &str, start_point: &str) -> Result<(), String> {
    require_success(
        run(workspace, &["reset", "--hard", start_point])?,
        &format!("Zurücksetzen auf '{start_point}' fehlgeschlagen"),
    )?;
    require_success(
        run(workspace, &["clean", "-fd", "-e", ".agentkit"])?,
        "Aufräumen nicht verfolgter Dateien fehlgeschlagen",
    )?;
    Ok(())
}

/// Stagt alles außer `.agentkit` und committet, falls dabei überhaupt etwas
/// gestagt wurde. `Ok(None)`, wenn der Versuch nichts geändert hat — das ist
/// KEIN Fehler (eine Analyse ändert oft keine Dateien), sonst
/// `Ok(Some(commit_id))`.
///
/// Der Ausschluss von `.agentkit` (die eigene Buchführung dieser Laufzeit,
/// siehe [`is_clean`] oben für dieselbe Begründung und dieselbe
/// Pathspec-Ausschlussregel) sitzt direkt IM Staging-Befehl, nicht als
/// nachträglicher `reset` auf den bereits gestagten Pfad: ein Item-Commit
/// soll die fachliche Änderung enthalten, nicht das Journal, das sie gerade
/// beschreibt — sonst würde jeder Commit um den kompletten Laufzeitzustand
/// wachsen, und zwei Item-Branches, die beide `work.jsonl` ändern, hätten bei
/// JEDEM Merge einen garantierten Konflikt im Journal.
pub fn commit_all(workspace: &str, message: &str) -> Result<Option<String>, String> {
    require_success(
        run(workspace, &["add", "-A", "--", ".", ":(exclude).agentkit"])?,
        "Staging fehlgeschlagen",
    )?;

    let staged = run(workspace, &["diff", "--cached", "--quiet"])?;
    if staged.status.success() {
        // Exit 0 heißt: keine Differenz zum letzten Commit gestagt.
        return Ok(None);
    }

    require_success(
        run_as_runtime(workspace, &["commit", "-m", message])?,
        "Commit fehlgeschlagen",
    )?;
    current_commit(workspace).map(Some)
}

/// Ausgang eines Merge-Versuchs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    Conflict,
}

/// Merged `branch` in den aktuellen Branch (`--no-ff`, damit der Merge immer
/// einen eigenen Commit erzeugt und die Integrationsgeschichte sichtbar
/// bleibt). Ein Konflikt ist KEIN `Err` — der Aufrufer soll die betroffenen
/// Dateien sehen können ([`conflicted_files`]) und selbst entscheiden, den
/// Merge abzubrechen ([`abort_merge`]); nur ein anderer, harter Git-Fehler
/// (unbekannter Branch, korruptes Repo, …) wird als `Err` gemeldet.
pub fn merge(workspace: &str, branch: &str, message: &str) -> Result<MergeOutcome, String> {
    let output = run_as_runtime(workspace, &["merge", "--no-ff", "-m", message, branch])?;
    if output.status.success() {
        return Ok(MergeOutcome::Merged);
    }
    // Ein Merge kann aus zwei Gründen scheitern: einem echten Konflikt (dann
    // stehen die betroffenen Dateien im Status) oder einem harten Git-Fehler
    // (z. B. der Branch existiert nicht). Konfliktdateien unterscheiden die
    // beiden Fälle zuverlässiger als der Exit-Code allein.
    match conflicted_files(workspace) {
        Ok(files) if !files.is_empty() => Ok(MergeOutcome::Conflict),
        _ => Err(format!(
            "Merge von '{branch}' fehlgeschlagen: {}",
            shortest_line(&output.stderr)
        )),
    }
}

/// Bricht einen laufenden Merge-Konflikt ab und stellt den Stand vor dem
/// Merge-Versuch wieder her.
pub fn abort_merge(workspace: &str) -> Result<(), String> {
    require_success(
        run(workspace, &["merge", "--abort"])?,
        "Merge-Abbruch fehlgeschlagen",
    )?;
    Ok(())
}

/// Dateien mit ungelöstem Merge-Konflikt (`git diff --name-only
/// --diff-filter=U`) — nur während eines laufenden, noch nicht
/// abgebrochenen Konflikts aussagekräftig.
pub fn conflicted_files(workspace: &str) -> Result<Vec<String>, String> {
    let out = require_success(
        run(workspace, &["diff", "--name-only", "--diff-filter=U"])?,
        "Konfliktdateien nicht ermittelbar",
    )?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Test-Fixture: initialisiert `workspace` (muss schon als Verzeichnis
/// existieren) als frisches Git-Repository mit genau einem Commit
/// (`readme.txt`) und gibt dessen Commit-ID zurück — offline und
/// deterministisch, keine globale `user.name`/`user.email`-Konfiguration
/// nötig (siehe [`RUNTIME_GIT_NAME`]/[`RUNTIME_GIT_EMAIL`]).
///
/// Befund 3 des Reviews: exakt diese „lege ein Wegwerf-Repo mit einem Commit
/// an"-Sequenz stand vorher DREIMAL fast identisch ausgeschrieben — hier
/// (`tmp_repo`), in `tests/runner.rs` (`git_repo`) und inline in
/// `tests/cli.rs`. EIN Ort statt drei Kopien (Rule of Three, Guidelines §2/§3).
///
/// `pub`, hinter `cfg(test)` ODER dem Feature `test-support`: Integrationstests
/// unter `tests/` sind eigene Crates und sehen nur die öffentliche API dieses
/// Crates, kein `cfg(test)`-Item aus den Unit-Tests dieses Moduls — eine
/// private Funktion würde sie nicht erreichen. Das Feature ist über eine
/// selbstreferenzierende Dev-Dependency in `Cargo.toml` nur während `cargo
/// test` aktiv, nie im Default-Build (siehe dort für die Begründung, warum
/// das weniger neue Oberfläche schafft als eine dauerhaft öffentliche
/// Test-Hilfsfunktion).
#[cfg(any(test, feature = "test-support"))]
pub fn init_repo_with_commit(workspace: &str) -> String {
    run(workspace, &["init", "--initial-branch=main", "-q"]).expect("git init im Test-Fixture");
    std::fs::write(
        std::path::Path::new(workspace).join("readme.txt"),
        "erste Zeile\n",
    )
    .expect("readme.txt im Test-Fixture schreibbar");
    run(workspace, &["add", "-A"]).expect("git add im Test-Fixture");
    require_success(
        run_as_runtime(workspace, &["commit", "-m", "initial"]).expect("git commit ausführbar"),
        "initial",
    )
    .expect("git commit im Test-Fixture");
    current_commit(workspace).expect("aktueller Commit im Test-Fixture")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tmp_repo(name: &str) -> std::path::PathBuf {
        static NR: AtomicUsize = AtomicUsize::new(0);
        let nr = NR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agentkit_work_git_{name}_{}_{nr}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.to_string_lossy().to_string();
        init_repo_with_commit(&ws);
        dir
    }

    #[test]
    fn frisches_repo_ist_sauber_und_is_repo_erkennt_es() {
        let dir = tmp_repo("clean");
        let ws = dir.to_string_lossy().to_string();
        assert!(is_repo(&ws));
        assert!(is_clean(&ws).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ein_nicht_repo_verzeichnis_wird_erkannt() {
        let dir = std::env::temp_dir().join(format!(
            "agentkit_work_git_kein_repo_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = dir.to_string_lossy().to_string();
        assert!(!is_repo(&ws));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uncommittete_aenderung_macht_den_arbeitsbaum_unsauber() {
        let dir = tmp_repo("unsauber");
        let ws = dir.to_string_lossy().to_string();
        std::fs::write(dir.join("neu.txt"), "x").unwrap();
        assert!(!is_clean(&ws).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_item_branch_legt_an_und_zweiter_aufruf_wechselt_nur() {
        let dir = tmp_repo("branch");
        let ws = dir.to_string_lossy().to_string();
        let start = current_commit(&ws).unwrap();
        ensure_item_branch(&ws, "work/demo/W-1", &start).unwrap();
        assert_eq!(current_branch(&ws).unwrap(), "work/demo/W-1");
        checkout(&ws, "main").unwrap();
        // Zweiter Aufruf (Retry-Simulation): der Branch existiert schon,
        // darf also nicht erneut ANGELEGT werden, nur gewechselt.
        ensure_item_branch(&ws, "work/demo/W-1", &start).unwrap();
        assert_eq!(current_branch(&ws).unwrap(), "work/demo/W-1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_all_erzeugt_commit_bei_aenderung_und_none_ohne_aenderung() {
        let dir = tmp_repo("commit");
        let ws = dir.to_string_lossy().to_string();
        assert_eq!(commit_all(&ws, "nichts geändert").unwrap(), None);

        std::fs::write(dir.join("neu.txt"), "inhalt").unwrap();
        let commit = commit_all(&ws, "neue datei").unwrap();
        assert!(commit.is_some());
        assert_eq!(current_commit(&ws).unwrap(), commit.unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_all_laesst_agentkit_verzeichnis_unangetastet() {
        let dir = tmp_repo("agentkit_ausschluss");
        let ws = dir.to_string_lossy().to_string();
        std::fs::create_dir_all(dir.join(".agentkit").join("work")).unwrap();
        std::fs::write(
            dir.join(".agentkit").join("work").join("work.jsonl"),
            "sollte nie committet werden\n",
        )
        .unwrap();
        std::fs::write(dir.join("code.txt"), "echte aenderung").unwrap();
        commit_all(&ws, "commit ohne .agentkit").unwrap();
        let tracked = require_success(
            run(&ws, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap(),
            "ls-tree",
        )
        .unwrap();
        assert!(!tracked.contains(".agentkit"), "{tracked}");
        assert!(tracked.contains("code.txt"), "{tracked}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zwei_nicht_ueberlappende_branches_mergen_konfliktfrei() {
        let dir = tmp_repo("merge_ok");
        let ws = dir.to_string_lossy().to_string();
        let start = current_commit(&ws).unwrap();

        ensure_item_branch(&ws, "work/demo/W-1", &start).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        commit_all(&ws, "W-1").unwrap();
        checkout(&ws, "main").unwrap();

        ensure_item_branch(&ws, "work/demo/W-2", &start).unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        commit_all(&ws, "W-2").unwrap();
        checkout(&ws, "main").unwrap();

        assert_eq!(
            merge(&ws, "work/demo/W-1", "merge W-1").unwrap(),
            MergeOutcome::Merged
        );
        assert_eq!(
            merge(&ws, "work/demo/W-2", "merge W-2").unwrap(),
            MergeOutcome::Merged
        );
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
        assert!(is_clean(&ws).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zwei_branches_mit_gleicher_zeile_erzeugen_konflikt_und_abort_stellt_sauberen_stand_wieder_her(
    ) {
        let dir = tmp_repo("merge_konflikt");
        let ws = dir.to_string_lossy().to_string();
        let start = current_commit(&ws).unwrap();

        ensure_item_branch(&ws, "work/demo/W-1", &start).unwrap();
        std::fs::write(dir.join("readme.txt"), "geaendert von W-1\n").unwrap();
        commit_all(&ws, "W-1").unwrap();
        checkout(&ws, "main").unwrap();

        ensure_item_branch(&ws, "work/demo/W-2", &start).unwrap();
        std::fs::write(dir.join("readme.txt"), "geaendert von W-2\n").unwrap();
        commit_all(&ws, "W-2").unwrap();
        checkout(&ws, "main").unwrap();

        assert_eq!(
            merge(&ws, "work/demo/W-1", "merge W-1").unwrap(),
            MergeOutcome::Merged
        );
        let outcome = merge(&ws, "work/demo/W-2", "merge W-2").unwrap();
        assert_eq!(outcome, MergeOutcome::Conflict);
        let files = conflicted_files(&ws).unwrap();
        assert_eq!(files, vec!["readme.txt".to_string()]);

        abort_merge(&ws).unwrap();
        assert!(is_clean(&ws).unwrap(), "nach dem Abbruch sauber");
        assert_eq!(current_branch(&ws).unwrap(), "main");
        std::fs::remove_dir_all(&dir).ok();
    }
}
