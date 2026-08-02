//! Benchmark-Läufe im Ergebnisbaum erkennen und zusammenfassen.
//!
//! Der Betrachter zeigt sonst SITZUNGEN — eine Trace-Datei je Task. Das ist
//! die richtige Einheit zum Zusehen, aber die falsche zum Überblicken: bei
//! einem Lauf mit 25 Instanzen will man zuerst wissen, was er insgesamt
//! ergeben hat, und erst dann in einen Task hineinsehen.
//!
//! Erkannt wird an Marker-Dateien, die die beiden Treiber ohnehin schreiben —
//! nicht an Verzeichnisnamen, die sich jederzeit ändern können:
//!
//! | Marker | Treiber |
//! |---|---|
//! | `metadata.json` mit `run_id` | `agentkit_bench.swebench.run_swebench` |
//! | `result.json` mit `n_total_trials` | Harbor (Terminal-Bench, Polyglot) |
//!
//! Das Ergebnis eines Laufs kommt aus dem, was der jeweilige Treiber ablegt:
//! `eval_local.json` (die lokale SWE-bench-Auswertung) bzw. Harbors
//! `result.json`. Dieses Crate rechnet NICHTS nach — es liest, was gemessen
//! wurde. Ein Betrachter, der eigene Zahlen erfindet, wäre schlimmer als
//! keiner.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// Wie tief unter der Wurzel nach Läufen gesucht wird. Beide Treiber legen
/// ihre Läufe unter `<benchmark>/<lauf>` ab, drei Ebenen sind Luft genug.
const MAX_TIEFE: u32 = 3;

/// Verzeichnisse, die innerhalb eines Laufs KEIN Task sind.
const KEINE_TASKS: &[&str] = &["graph", "logs", "work", "trace", ".eval-tmp"];

/// Ein Benchmark-Lauf.
#[derive(Debug, Clone, Serialize)]
pub struct BenchRun {
    /// Pfad relativ zur Wurzel — zugleich die Kennung in der Oberfläche.
    pub name: String,
    /// `swebench` oder `harbor`.
    pub kind: String,
    /// Was der Treiber über den Lauf notiert hat (Modell, Datensatz, Start …).
    pub meta: Value,
    pub tasks: Vec<BenchTask>,
    /// Zusammenfassung, sofern ausgewertet wurde (`null` = noch offen).
    pub summary: Option<BenchSummary>,
}

/// Ein Task innerhalb eines Laufs.
#[derive(Debug, Clone, Serialize)]
pub struct BenchTask {
    pub id: String,
    /// `resolved`, `unresolved`, `patch_failed`, `belohnung 1.0` … — was der
    /// Treiber gemessen hat, leer wenn nichts vorliegt.
    pub status: String,
    /// Sitzungsname der zugehörigen Trace-Datei (für den Sprung in den
    /// Verlauf), leer wenn kein Trace geschrieben wurde.
    pub session: String,
    /// Hat der Task ein Work-Projekt angelegt?
    pub work: bool,
}

/// Das Ergebnis eines Laufs in einer Zeile.
#[derive(Debug, Clone, Serialize)]
pub struct BenchSummary {
    pub total: usize,
    pub ok: usize,
    /// Woher die Zahl stammt — damit in der Oberfläche steht, ob sie aus der
    /// lokalen Auswertung oder von Harbor kommt.
    pub source: String,
}

/// Alle Läufe unter `wurzel`, jüngste zuerst.
///
/// Die Wurzel SELBST kann ein Lauf sein. Das ist kein Sonderfall, sondern der
/// Normalfall beim Live-Zusehen: dort richtet man den Betrachter genau auf das
/// Lauf-Verzeichnis, weil er nur dann die erste Sitzung von selbst aufgreift
/// (ein Verzeichnis mit alten Läufen lässt ihn an einer alten hängen). Wer nur
/// die Unterverzeichnisse absucht, zeigt ausgerechnet dann „keine
/// Benchmark-Läufe", wenn gerade einer läuft.
pub fn list_runs(wurzel: &Path) -> Vec<BenchRun> {
    if let Some(lauf) = lies_lauf(wurzel, wurzel) {
        return vec![lauf];
    }
    let mut out = Vec::new();
    sammle(wurzel, wurzel, 0, &mut out);
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

fn sammle(wurzel: &Path, akt: &Path, tiefe: u32, out: &mut Vec<BenchRun>) {
    if tiefe > MAX_TIEFE {
        return;
    }
    let Ok(entries) = std::fs::read_dir(akt) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(typ) = e.file_type() else { continue };
        if !typ.is_dir() {
            continue;
        }
        let pfad = e.path();
        if let Some(lauf) = lies_lauf(wurzel, &pfad) {
            out.push(lauf);
            // Nicht weiter absteigen: unterhalb eines Laufs liegen Tasks,
            // keine weiteren Läufe.
            continue;
        }
        sammle(wurzel, &pfad, tiefe + 1, out);
    }
}

/// Ist `dir` ein Lauf? Dann lesen, sonst `None`.
fn lies_lauf(wurzel: &Path, dir: &Path) -> Option<BenchRun> {
    // Ist `dir` die Wurzel selbst, ist der relative Pfad leer — dann heißt der
    // Lauf wie sein Verzeichnis.
    let name = rel_name(wurzel, dir).or_else(|| Some(datei_name(dir)))?;
    if let Some(meta) = json_datei(&dir.join("metadata.json")) {
        if meta.get("run_id").is_some() {
            let tasks = swebench_tasks(wurzel, dir);
            let summary = swebench_summary(dir, tasks.len());
            return Some(BenchRun {
                name,
                kind: "swebench".into(),
                meta,
                tasks,
                summary,
            });
        }
    }
    if let Some(res) = json_datei(&dir.join("result.json")) {
        if res.get("n_total_trials").is_some() {
            let tasks = harbor_tasks(wurzel, dir, &res);
            let summary = harbor_summary(&res, tasks.len());
            let meta = json_datei(&dir.join("config.json")).unwrap_or(res);
            return Some(BenchRun {
                name,
                kind: "harbor".into(),
                meta,
                tasks,
                summary,
            });
        }
    }
    None
}

fn swebench_tasks(wurzel: &Path, dir: &Path) -> Vec<BenchTask> {
    // Status je Instanz aus der lokalen Auswertung, falls sie gelaufen ist.
    let stati = json_datei(&dir.join("eval_local.json"))
        .and_then(|v| v.get("results").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let status_von = |id: &str| -> String {
        stati
            .iter()
            .find(|r| r.get("instance_id").and_then(Value::as_str) == Some(id))
            .and_then(|r| r.get("status").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    unterverzeichnisse(dir)
        .into_iter()
        .map(|p| {
            let id = datei_name(&p);
            BenchTask {
                status: status_von(&id),
                session: erste_trace(wurzel, &p).unwrap_or_default(),
                work: p.join("work").is_dir(),
                id,
            }
        })
        .collect()
}

fn swebench_summary(dir: &Path, total: usize) -> Option<BenchSummary> {
    let ev = json_datei(&dir.join("eval_local.json"))?;
    Some(BenchSummary {
        total: ev
            .get("total")
            .and_then(Value::as_u64)
            .unwrap_or(total as u64) as usize,
        ok: ev.get("resolved").and_then(Value::as_u64).unwrap_or(0) as usize,
        source: "eval_local".into(),
    })
}

fn harbor_tasks(wurzel: &Path, dir: &Path, res: &Value) -> Vec<BenchTask> {
    // Harbor legt die Belohnungen je Trial unter stats.evals.<name>.reward_stats
    // ab: { "1.0": [ids…], "0.0": [ids…] }.
    let belohnungen = res
        .get("stats")
        .and_then(|s| s.get("evals"))
        .and_then(Value::as_object)
        .and_then(|m| m.values().next().cloned())
        .and_then(|e| e.get("reward_stats").cloned())
        .and_then(|r| r.get("reward").cloned());
    let belohnung_von = |id: &str| -> String {
        let Some(obj) = belohnungen.as_ref().and_then(Value::as_object) else {
            return String::new();
        };
        for (wert, ids) in obj {
            if ids
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i.as_str() == Some(id)))
            {
                return format!("reward {wert}");
            }
        }
        String::new()
    };
    unterverzeichnisse(dir)
        .into_iter()
        .map(|p| {
            let id = datei_name(&p);
            BenchTask {
                status: belohnung_von(&id),
                session: erste_trace(wurzel, &p).unwrap_or_default(),
                work: p.join("agent").join("work").is_dir(),
                id,
            }
        })
        .collect()
}

fn harbor_summary(res: &Value, total: usize) -> Option<BenchSummary> {
    let stats = res.get("stats")?;
    let obj = stats.get("evals")?.as_object()?.values().next()?;
    let reward = obj.get("reward_stats")?.get("reward")?.as_object()?;
    // „ok" ist bei Harbor eine volle Belohnung; alles andere zählt nicht.
    let ok = reward
        .get("1.0")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    Some(BenchSummary {
        total: res
            .get("n_total_trials")
            .and_then(Value::as_u64)
            .unwrap_or(total as u64) as usize,
        ok,
        source: "harbor".into(),
    })
}

// ------------------------------------------------------------------ Helfer

fn unterverzeichnisse(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| !KEINE_TASKS.contains(&datei_name(p).as_str()))
        .collect();
    out.sort();
    out
}

/// Die erste Trace-Datei unterhalb von `dir`, als Sitzungsname relativ zur
/// Wurzel — der Anker, über den die Oberfläche in den Verlauf springt.
fn erste_trace(wurzel: &Path, dir: &Path) -> Option<String> {
    let treffer = crate::trace::list_traces(dir);
    let erste = treffer.first()?;
    rel_name(wurzel, &dir.join(&erste.name))
}

fn json_datei(pfad: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(pfad).ok()?).ok()
}

fn datei_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Pfad relativ zur Wurzel, `/`-getrennt — dieselbe Schreibweise wie die
/// Sitzungsnamen, damit ein Task-Eintrag direkt als `run=` taugt.
fn rel_name(wurzel: &Path, pfad: &Path) -> Option<String> {
    let rel = pfad.strip_prefix(wurzel).ok()?;
    let teile: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (!teile.is_empty()).then(|| teile.join("/"))
}
