//! `agentkit graph …` — der Zugang des MENSCHEN zum Graphen.
//!
//! Die Tools (`crate::tools`) sind die Sicht des Modells; was hier steht, darf
//! das Modell gerade nicht: Wissen auf die Benutzer-Ebene heben. Die
//! Executable reicht nur durch — Verzeichnisse, wer der Mensch ist und wie man
//! ihn fragt. Dadurch ist das Verb ohne Terminal testbar.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::access::GraphAccess;
use crate::federation::{GraphFederation, USER_PREFIX};
use crate::model::Actor;
use crate::store::GraphStore;

pub const HELP: &str = "\
agentkit graph — den Wissensgraphen als Mensch pflegen

Aufruf:
  agentkit graph promote --user <ID>   Bestätigte Aussage des Projekts ins
                                       Benutzer-Wissen heben. Sie ist danach in
                                       JEDEM Projekt sichtbar — deshalb fragt das
                                       Kommando nach und läuft nur am Terminal.

Optionen:
  --graph DIR        Projekt-Bundle (Standard: <workspace>/.agentkit/graph)
  -w, --workspace DIR  Workspace, zu dem das Projekt-Bundle gehört (Standard: .)
  -h, --help         Diese Hilfe";

/// Was die Executable dem Verb mitgibt.
pub struct GraphCli<'a> {
    pub project_dir: PathBuf,
    /// `None`, wenn es kein Konfigurationsverzeichnis gibt.
    pub user_dir: Option<PathBuf>,
    /// Wer am Terminal sitzt — steht als Freigebender an der Aussage.
    pub human: Actor,
    /// Stellt dem Menschen eine Ja/Nein-Frage. Muss `false` liefern, wenn
    /// niemand da ist, der antworten kann (kein Terminal): eine Freigabe, die
    /// ein Skript — oder ein Agent über seine Shell — sich selbst erteilt, ist
    /// keine.
    pub confirm: &'a mut dyn FnMut(&str) -> bool,
}

/// Führt ein Unterkommando aus; die Ausgabe geht nach `out`.
pub fn dispatch(args: &[String], cli: &mut GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["promote", "--user", id] => promote_to_user(id, cli, out),
        ["promote", ..] => Err("Aufruf: agentkit graph promote --user <ID>".to_string()),
        [] => Err(format!("Unterkommando fehlt.\n\n{HELP}")),
        [other, ..] => Err(format!("unbekanntes Unterkommando '{other}'.\n\n{HELP}")),
    }
}

/// Projekt-Bundle lesend, Benutzer-Bundle so, dass es beim ersten Schreiben
/// entsteht.
fn open(cli: &GraphCli) -> Result<GraphFederation, String> {
    let project = GraphStore::open_frozen(&cli.project_dir).map_err(|e| e.to_string())?;
    let legacy = project.snapshot().workspace_scopes();
    let access = GraphAccess::project(cli.human.as_str(), "cli", &legacy);
    let access = GraphAccess::read_only(cli.human.as_str(), access.view.without_working());
    let fed = GraphFederation::single(Arc::new(project), access);
    let user_dir = cli
        .user_dir
        .as_ref()
        .ok_or("kein Konfigurationsverzeichnis — es gibt keinen Ort für das Benutzer-Wissen")?;
    let user = GraphStore::open_lazy(user_dir).map_err(|e| e.to_string())?;
    Ok(fed.with_user(Arc::new(user)))
}

fn promote_to_user(id: &str, cli: &mut GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let fed = open(cli)?;
    let (export, _) = fed
        .evidence(id)
        .filter(|_| !id.trim().starts_with(USER_PREFIX))
        .ok_or_else(|| {
            format!(
                "Aussage '{}' gibt es im Projektwissen von {} nicht",
                id.trim(),
                cli.project_dir.display()
            )
        })?;

    // Der Mensch sieht, WAS er freigibt — samt der Belegstellen, die mitgehen.
    let mut frage = format!(
        "({}) --[{}]--> ({})\n  Status: {} | Vertrauen: {:.2}\n",
        export.subject.canonical_name,
        export.claim.predicate,
        export.object.canonical_name,
        export.claim.status,
        export.claim.confidence
    );
    for source in export.sources.iter().filter(|s| s.is_foreign()) {
        frage.push_str(&format!(
            "  Quelle {} ({}): {}\n",
            source.id,
            source.source_type,
            source.excerpt.as_deref().unwrap_or("—")
        ));
    }
    frage.push_str(
        "Diese Aussage ins Benutzer-Wissen übernehmen? Sie ist danach in JEDEM Projekt sichtbar.",
    );
    if !(cli.confirm)(&frage) {
        return Err("nicht freigegeben — nichts geändert".to_string());
    }

    let receipt = fed
        .promote_to_user(id, cli.human.clone())
        .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "Übernommen als {USER_PREFIX}{}{}.",
        receipt.claim_id.unwrap_or_default(),
        if receipt.deduplicated {
            " (war schon da — die Freigabe ist als weitere Bestätigung vermerkt)"
        } else {
            ""
        }
    )
    .map_err(|e| e.to_string())
}
