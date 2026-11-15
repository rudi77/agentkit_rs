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
use crate::model::{Actor, ClaimStatus, GraphClaim, GraphLayer};
use crate::store::{GraphIndex, GraphStore};

pub const HELP: &str = "\
agentkit graph — den Wissensgraphen als Mensch ansehen und pflegen

Aufruf:
  agentkit graph status              Stand: Aussagen je Ebene, Ort, bestätigt?
  agentkit graph list [--user] [--all]
                                     Dauerhafte Aussagen des Projekts (--user:
                                     des Benutzer-Wissens; --all: auch
                                     Vorläufiges, Ersetztes, Zurückgezogenes)
  agentkit graph show <ID>           Eine Aussage mit ihren Quellen
  agentkit graph forget <ID>         Aussage ENDGÜLTIG löschen
  agentkit graph merge <VON> <NACH>  Entity VON in Entity NACH zusammenführen
  agentkit graph rename <ID> <NAME>  Entity umbenennen (alter Name bleibt Alias)
  agentkit graph doctor              Bundles auf Unstimmigkeiten prüfen
  agentkit graph trust               Das Projekt-Bundle als eigenes bestätigen
                                     (für ein geklontes Repository mit Graph)
  agentkit graph promote --user <ID> Bestätigte Aussage des Projekts ins
                                     Benutzer-Wissen heben. Sie ist danach in
                                     JEDEM Projekt sichtbar.

IDs mit dem Präfix u: (u:C-17, u:E-3) meinen das Benutzer-Wissen, alle anderen
das Projekt. forget, merge, rename und promote fragen nach und laufen deshalb
nur an einem Terminal.

Optionen:
  --graph DIR          Projekt-Bundle (Standard: <workspace>/.agentkit/graph)
  -w, --workspace DIR  Workspace, zu dem das Projekt-Bundle gehört (Standard: .)
  -h, --help           Diese Hilfe";

/// Was die Executable dem Verb mitgibt.
pub struct GraphCli<'a> {
    pub project_dir: PathBuf,
    /// `None`, wenn es kein Konfigurationsverzeichnis gibt.
    pub user_dir: Option<PathBuf>,
    /// Wer am Terminal sitzt — steht als Freigebender an der Aussage.
    pub human: Actor,
    /// Schlüssel des Benutzers für [`crate::trust`]; `None` ohne
    /// Konfigurationsverzeichnis.
    pub owner_key: Option<String>,
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
        ["status"] => status(cli, out),
        ["list", flags @ ..] if flags.iter().all(|f| matches!(*f, "--user" | "--all")) => list(
            cli,
            flags.contains(&"--user"),
            flags.contains(&"--all"),
            out,
        ),
        ["show", id] => show(id, cli, out),
        ["forget", id] => forget(id, cli, out),
        ["merge", from, into] => merge(from, into, cli, out),
        ["rename", id, name @ ..] if !name.is_empty() => rename(id, &name.join(" "), cli, out),
        ["doctor"] => doctor(cli, out),
        ["trust"] => trust(cli, out),
        ["promote", "--user", id] => promote_to_user(id, cli, out),
        [] => Err(format!("Unterkommando fehlt.\n\n{HELP}")),
        [known @ ("status" | "list" | "show" | "forget" | "merge" | "rename" | "doctor"
        | "trust" | "promote"), ..] => Err(format!("falsche Argumente für '{known}'.\n\n{HELP}")),
        [other, ..] => Err(format!("unbekanntes Unterkommando '{other}'.\n\n{HELP}")),
    }
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

fn user_dir<'a>(cli: &'a GraphCli) -> Result<&'a PathBuf, String> {
    cli.user_dir.as_ref().ok_or_else(|| {
        "kein Konfigurationsverzeichnis — es gibt keinen Ort für das Benutzer-Wissen".to_string()
    })
}

/// Der Store, den eine ID meint: mit `u:` das Benutzer-Bundle, sonst das
/// Projekt. Schreibend geöffnet, aber ohne ein fehlendes Bundle anzulegen.
fn store_for<'a>(cli: &GraphCli, id: &'a str) -> Result<(GraphStore, &'a str), String> {
    let id = id.trim();
    let (dir, local) = match id.strip_prefix(USER_PREFIX) {
        Some(local) => (user_dir(cli)?, local),
        None => (&cli.project_dir, id),
    };
    let store = GraphStore::open_lazy(dir).map_err(|e| e.to_string())?;
    Ok((store, local))
}

fn prefix_of(id: &str) -> &'static str {
    if id.trim().starts_with(USER_PREFIX) {
        USER_PREFIX
    } else {
        ""
    }
}

fn claim_line(index: &GraphIndex, claim: &GraphClaim, prefix: &str) -> String {
    let name = |id: &str| {
        index
            .entity(id)
            .map_or_else(|| id.to_string(), |e| e.canonical_name.clone())
    };
    format!(
        "[{prefix}{}] ({}) --[{}]--> ({})  {} · {:.2} · {}@{}",
        claim.id,
        name(&claim.subject),
        claim.predicate,
        name(&claim.object),
        claim.status,
        claim.confidence,
        claim.layer,
        claim.scope
    )
}

/// Eine Aussage zählt als Wissen, wenn sie dauerhaft ist und weder ersetzt
/// noch zurückgezogen.
fn is_knowledge(claim: &GraphClaim) -> bool {
    claim.layer == GraphLayer::Canonical && claim.status == ClaimStatus::Confirmed
}

fn status(cli: &GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let project = GraphStore::open_read_only(&cli.project_dir).map_err(|e| e.to_string())?;
    let durable = project.claims().filter(|c| is_knowledge(c)).count();
    let working = project
        .claims()
        .filter(|c| c.layer == GraphLayer::Working)
        .count();
    let confirmed = match &cli.owner_key {
        _ if !cli.project_dir.join(crate::BUNDLE_INDEX).exists() => "noch nicht angelegt",
        Some(key) if crate::trust::is_signed(&cli.project_dir, key) => "bestätigt",
        _ => "NICHT bestätigt (agentkit graph trust)",
    };
    writeln!(
        out,
        "Projekt:  {} — {durable} Aussagen dauerhaft, {working} vorläufig, {} insgesamt ({confirmed})",
        cli.project_dir.display(),
        project.claim_count()
    )
    .map_err(io)?;
    match &cli.user_dir {
        Some(dir) => {
            let user = GraphStore::open_read_only(dir).map_err(|e| e.to_string())?;
            writeln!(
                out,
                "Benutzer: {} — {} Aussagen",
                dir.display(),
                user.claims().filter(|c| is_knowledge(c)).count()
            )
        }
        None => writeln!(out, "Benutzer: kein Konfigurationsverzeichnis"),
    }
    .map_err(io)
}

fn list(cli: &GraphCli, user: bool, all: bool, out: &mut dyn Write) -> Result<(), String> {
    let (dir, prefix) = if user {
        (user_dir(cli)?, USER_PREFIX)
    } else {
        (&cli.project_dir, "")
    };
    let index = GraphStore::open_read_only(dir).map_err(|e| e.to_string())?;
    let mut shown = 0;
    for claim in index.claims().filter(|c| all || is_knowledge(c)) {
        writeln!(out, "{}", claim_line(&index, claim, prefix)).map_err(io)?;
        shown += 1;
    }
    if shown == 0 {
        writeln!(out, "(keine Aussagen)").map_err(io)?;
    }
    Ok(())
}

fn show(id: &str, cli: &GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let (store, local) = store_for(cli, id)?;
    let index = store.snapshot();
    let claim = index
        .claim(local)
        .ok_or_else(|| format!("Aussage '{}' gibt es nicht", id.trim()))?;
    let prefix = prefix_of(id);
    writeln!(out, "{}", claim_line(&index, claim, prefix)).map_err(io)?;
    writeln!(out, "  erzeugt von {}", claim.created_by).map_err(io)?;
    if let Some(from) = &claim.promoted_from {
        writeln!(out, "  befördert aus {from}").map_err(io)?;
    }
    for v in &claim.verified {
        writeln!(out, "  bestätigt von {}", v.by).map_err(io)?;
    }
    if let Some(newer) = &claim.superseded_by {
        writeln!(out, "  ersetzt durch {prefix}{newer}").map_err(io)?;
    }
    if let Some(r) = &claim.retracted {
        writeln!(out, "  zurückgezogen von {}: {}", r.by, r.reason).map_err(io)?;
    }
    for source in claim.source_ids.iter().filter_map(|s| index.source(s)) {
        writeln!(
            out,
            "  Quelle {prefix}{} ({}{}): {}",
            source.id,
            source.source_type,
            source
                .tool_call_id
                .as_deref()
                .map(|c| format!(", {c}"))
                .unwrap_or_default(),
            source.excerpt.as_deref().unwrap_or("—")
        )
        .map_err(io)?;
    }
    Ok(())
}

fn ask(cli: &mut GraphCli, frage: &str) -> Result<(), String> {
    if (cli.confirm)(frage) {
        Ok(())
    } else {
        Err("nicht bestätigt — nichts geändert".to_string())
    }
}

fn forget(id: &str, cli: &mut GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let (store, local) = store_for(cli, id)?;
    let index = store.snapshot();
    let claim = index
        .claim(local)
        .ok_or_else(|| format!("Aussage '{}' gibt es nicht", id.trim()))?;
    ask(
        cli,
        &format!(
            "{}\nDiese Aussage ENDGÜLTIG löschen? (Zurückziehen mit Grund ließe sie als Evidenz stehen.)",
            claim_line(&index, claim, prefix_of(id))
        ),
    )?;
    store
        .rewrite(|index, revision| crate::maintain::forget_claim(index, local, revision))
        .map_err(|e| e.to_string())?;
    writeln!(out, "Gelöscht: {}.", id.trim()).map_err(io)
}

fn merge(from: &str, into: &str, cli: &mut GraphCli, out: &mut dyn Write) -> Result<(), String> {
    if prefix_of(from) != prefix_of(into) {
        return Err("beide Entities müssen im selben Bundle liegen".to_string());
    }
    let (store, from_local) = store_for(cli, from)?;
    let into_local = into.trim().trim_start_matches(USER_PREFIX);
    let index = store.snapshot();
    let name = |id: &str| {
        index
            .entity(id)
            .map(|e| e.canonical_name.clone())
            .ok_or_else(|| format!("Entity '{id}' gibt es nicht"))
    };
    let frage = format!(
        "'{}' ({}) in '{}' ({}) zusammenführen? Alle Aussagen über die erste gelten danach der zweiten.",
        name(from_local)?,
        from.trim(),
        name(into_local)?,
        into.trim()
    );
    ask(cli, &frage)?;
    store
        .rewrite(|index, revision| {
            crate::maintain::merge_entities(index, from_local, into_local, revision)
        })
        .map_err(|e| e.to_string())?;
    writeln!(out, "Zusammengeführt: {} → {}.", from.trim(), into.trim()).map_err(io)
}

fn rename(id: &str, name: &str, cli: &mut GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let (store, local) = store_for(cli, id)?;
    let alt = store
        .snapshot()
        .entity(local)
        .map(|e| e.canonical_name.clone())
        .ok_or_else(|| format!("Entity '{}' gibt es nicht", id.trim()))?;
    ask(
        cli,
        &format!("'{alt}' ({}) in '{name}' umbenennen?", id.trim()),
    )?;
    store
        .rewrite(|index, revision| crate::maintain::rename_entity(index, local, name, revision))
        .map_err(|e| e.to_string())?;
    writeln!(out, "Umbenannt: '{alt}' → '{name}'.").map_err(io)
}

/// Bestätigt das Projekt-Bundle als eigenes — der ausdrückliche Weg für alle,
/// die nie interaktiv starten. Ohne Rückfrage: das Kommando IST die Antwort.
fn trust(cli: &GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let key = cli
        .owner_key
        .as_deref()
        .ok_or("kein Konfigurationsverzeichnis — dort läge der Schlüssel")?;
    if !cli.project_dir.join(crate::BUNDLE_INDEX).exists() {
        return Err(format!(
            "in {} liegt kein Graph-Bundle",
            cli.project_dir.display()
        ));
    }
    crate::trust::sign(&cli.project_dir, key).map_err(|e| e.to_string())?;
    writeln!(
        out,
        "Bestätigt: {} wird künftig gelesen.",
        cli.project_dir.display()
    )
    .map_err(io)
}

/// Was an einem Bundle nicht stimmt. Liest nur.
fn findings(dir: &std::path::Path, label: &str) -> Vec<String> {
    let index = match GraphStore::open_read_only(dir) {
        Ok(index) => index,
        Err(e) => return vec![format!("{label}: nicht lesbar — {e}")],
    };
    let mut out = Vec::new();
    for claim in index.claims() {
        for endpoint in [&claim.subject, &claim.object] {
            if index.entity(endpoint).is_none() {
                out.push(format!(
                    "{label}: {} zeigt auf die fehlende Entity {endpoint}",
                    claim.id
                ));
            }
        }
        for source in &claim.source_ids {
            if index.source(source).is_none() {
                out.push(format!(
                    "{label}: {} nennt die fehlende Quelle {source}",
                    claim.id
                ));
            }
        }
        if let Some(newer) = claim
            .superseded_by
            .as_ref()
            .filter(|n| index.claim(n).is_none())
        {
            out.push(format!(
                "{label}: {} ist ersetzt durch {newer}, das es nicht gibt",
                claim.id
            ));
        }
        if (claim.status == ClaimStatus::Retracted) != claim.retracted.is_some() {
            out.push(format!(
                "{label}: {} — Status und Rückzugsvermerk passen nicht zusammen",
                claim.id
            ));
        }
    }
    // Reste eines abgebrochenen Commits.
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                pending.push(path);
            } else if name.starts_with("graph.lock") || name.contains(".tmp-") {
                out.push(format!(
                    "{label}: {} liegt noch da — läuft dort ein Prozess, oder ist einer abgestürzt?",
                    path.display()
                ));
            }
        }
    }
    out
}

fn doctor(cli: &GraphCli, out: &mut dyn Write) -> Result<(), String> {
    let mut all = findings(&cli.project_dir, "Projekt");
    if let Some(dir) = &cli.user_dir {
        all.extend(findings(dir, "Benutzer"));
    }
    if all.is_empty() {
        return writeln!(out, "Keine Auffälligkeiten.").map_err(io);
    }
    for line in &all {
        writeln!(out, "{line}").map_err(io)?;
    }
    Err(format!("{} Auffälligkeit(en)", all.len()))
}

/// Projekt-Bundle lesend, Benutzer-Bundle so, dass es beim ersten Schreiben
/// entsteht.
fn open(cli: &GraphCli) -> Result<GraphFederation, String> {
    let project = GraphStore::open_frozen(&cli.project_dir).map_err(|e| e.to_string())?;
    let legacy = project.snapshot().workspace_scopes();
    let access = GraphAccess::project(cli.human.as_str(), "cli", &legacy);
    let access = GraphAccess::read_only(cli.human.as_str(), access.view.without_working());
    let fed = GraphFederation::single(Arc::new(project), access);
    let user = GraphStore::open_lazy(user_dir(cli)?).map_err(|e| e.to_string())?;
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
