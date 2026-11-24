//! Zwei Ebenen: Projektwissen im Projekt-Bundle, Benutzer-Wissen im
//! Benutzer-Bundle — und eine Sicht über beide.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentkit::ToolRegistry;
use agentkit_graph::model::Actor;
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    register_federated_graph_tools, ClaimDraft, ClaimExport, ClaimStatus, GraphAccess, GraphError,
    GraphFederation, GraphLayer, GraphQuery, GraphScope, GraphTarget, GraphWriteCommand,
    SourceDraft, BUNDLE_INDEX,
};
use serde_json::json;

struct Verzeichnis(PathBuf);

impl Verzeichnis {
    fn neu(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graph_fed_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Verzeichnis(dir)
    }
    fn pfad(&self, teil: &str) -> PathBuf {
        self.0.join(teil)
    }
}

impl Drop for Verzeichnis {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Ein Projekt samt angebundenem Benutzer-Bundle, so wie die Executable es
/// öffnet: Legacy-Scopes aus dem Bestand, Benutzer-Bundle ohne es anzulegen.
fn projekt(projekt_dir: &Path, benutzer_dir: &Path, session: &str) -> GraphFederation {
    let store = Arc::new(GraphStore::open(projekt_dir).unwrap());
    let access = GraphAccess::project("agentkit", session, &store.snapshot().workspace_scopes());
    GraphFederation::single(store, access)
        .with_user(Arc::new(GraphStore::open_lazy(benutzer_dir).unwrap()))
}

/// Belegte Aussage schreiben und ins Projektwissen befördern.
fn lerne(fed: &GraphFederation, s: &str, p: &str, o: &str) -> String {
    let member = &fed.project;
    let id = member
        .store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                s,
                p,
                o,
                SourceDraft::new("test_run").excerpt("cargo test"),
            )),
            &member.access,
        )
        .unwrap()
        .claim_id
        .unwrap();
    member
        .store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &member.access,
        )
        .unwrap();
    id
}

fn mensch() -> Actor {
    Actor::human("dana")
}

#[test]
fn benutzer_wissen_aus_projekt_a_erscheint_in_projekt_b_projektwissen_nicht() {
    let dir = Verzeichnis::neu("a_b");
    let benutzer = dir.pfad("benutzer");

    let a = projekt(&dir.pfad("a"), &benutzer, "s-a");
    let allgemein = lerne(&a, "cargo nextest", "ist schneller als", "cargo test");
    lerne(
        &a,
        "Kundenmodul Abrechnung",
        "nutzt",
        "Tabelle rechnungen_v2",
    );
    assert!(
        !benutzer.exists(),
        "das Benutzer-Bundle entsteht erst mit der ersten Freigabe"
    );
    a.promote_to_user(&allgemein, mensch()).unwrap();
    assert!(benutzer.join(BUNDLE_INDEX).exists());

    // Projekt B: eigenes Bundle, dasselbe Benutzer-Bundle.
    let b = projekt(&dir.pfad("b"), &benutzer, "s-b");
    let treffer = b.search(&GraphQuery::text("Was weiß ich über cargo nextest?"), true);
    assert_eq!(treffer.claims, 1, "{}", treffer.text);
    assert!(treffer.text.contains("[u:C-1]"), "{}", treffer.text);
    assert!(treffer.text.contains("Quellen: u:S-1"), "{}", treffer.text);

    let fremd = b.search(&GraphQuery::text("Kundenmodul Abrechnung"), true);
    assert_eq!(fremd.claims, 0, "Projektwissen von A: {}", fremd.text);
}

#[test]
fn die_uebernahme_kopiert_bestaetigtes_mit_fremder_quelle_und_haelt_die_freigabe_fest() {
    let dir = Verzeichnis::neu("adopt");
    let a = projekt(&dir.pfad("a"), &dir.pfad("benutzer"), "s-a");
    let id = lerne(&a, "ripgrep", "ist schneller als", "grep");

    let receipt = a.promote_to_user(&id, mensch()).unwrap();
    let user = a.user.as_ref().unwrap();
    let index = user.store.snapshot();
    let claim = index.claim(receipt.claim_id.as_deref().unwrap()).unwrap();
    assert_eq!(claim.status, ClaimStatus::Confirmed);
    assert_eq!(claim.layer, GraphLayer::Canonical);
    assert_eq!(claim.scope, GraphScope::user());
    assert_eq!(claim.promoted_from, Some(GraphScope::project()));
    assert_eq!(claim.verified.len(), 1);
    assert_eq!(claim.verified[0].by, mensch());
    assert_eq!(
        index.source(&claim.source_ids[0]).unwrap().source_type,
        "test_run"
    );

    // Das Projekt-Bundle bleibt, wie es war.
    let im_projekt = a.project.store.snapshot().claim(&id).unwrap().clone();
    assert_eq!(im_projekt.scope, GraphScope::project());

    // Dieselbe Aussage noch einmal freigeben: kein Duplikat, eine weitere Bestätigung.
    let nochmal = a.promote_to_user(&id, Actor::human("eli")).unwrap();
    assert!(nochmal.deduplicated);
    let index = user.store.snapshot();
    assert_eq!(index.claim_count(), 1);
    assert_eq!(
        index
            .claim(nochmal.claim_id.as_deref().unwrap())
            .unwrap()
            .verified
            .len(),
        2
    );
}

#[test]
fn ohne_menschen_kommt_nichts_auf_die_benutzer_ebene() {
    let dir = Verzeichnis::neu("gate");
    let a = projekt(&dir.pfad("a"), &dir.pfad("benutzer"), "s-a");
    let bestaetigt = lerne(&a, "ripgrep", "ist schneller als", "grep");

    // Ein Agent als „Freigebender" zählt nicht.
    let abgelehnt = a.promote_to_user(&bestaetigt, Actor::agent("agentkit"));
    assert!(
        matches!(abgelehnt, Err(GraphError::Denied(_))),
        "{abgelehnt:?}"
    );

    // Vorläufiges wird auch vom Menschen nicht übersprungen.
    let vorlaeufig = a
        .project
        .store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "fd",
                "ersetzt",
                "find",
                SourceDraft::new("test_run"),
            )),
            &a.project.access,
        )
        .unwrap()
        .claim_id
        .unwrap();
    let zu_frueh = a.promote_to_user(&vorlaeufig, mensch());
    assert!(
        matches!(zu_frueh, Err(GraphError::Invalid(_))),
        "{zu_frueh:?}"
    );

    // Der Zugriff, den die Tools auf das Benutzer-Bundle bekommen, kann dort
    // gar nichts schreiben — auch nicht mit einem selbst gebauten Kommando.
    let user = a.user.as_ref().unwrap();
    let export =
        ClaimExport::read(&a.project.store.snapshot(), &bestaetigt, &a.project.access).unwrap();
    let ohne_ziel = user.store.submit(
        GraphWriteCommand::AdoptClaim {
            export: Box::new(export),
            approved_by: mensch(),
        },
        &user.access,
    );
    assert!(
        matches!(ohne_ziel, Err(GraphError::Denied(_))),
        "{ohne_ziel:?}"
    );
    assert!(!dir.pfad("benutzer").exists(), "nichts wurde angelegt");

    // Und kein Tool bietet den Weg an.
    let mut tools = ToolRegistry::new();
    register_federated_graph_tools(&mut tools, a.clone());
    let mut namen = tools.names();
    namen.sort();
    assert_eq!(
        namen,
        [
            "graph_compare",
            "graph_evidence",
            "graph_neighbors",
            "graph_promote",
            "graph_remember",
            "graph_retract",
            "graph_search"
        ]
    );
}

/// `C-1` gibt es in beiden Bundles. Ohne Präfix ist das Projekt gemeint, mit
/// `u:` das Benutzer-Wissen — in jedem Tool.
#[test]
fn qualifizierte_ids_werden_in_allen_tools_richtig_aufgeloest() {
    let dir = Verzeichnis::neu("ids");
    let benutzer = dir.pfad("benutzer");
    let a = projekt(&dir.pfad("a"), &benutzer, "s-a");
    let aus_a = lerne(&a, "Rust", "kompiliert mit", "cargo");
    a.promote_to_user(&aus_a, mensch()).unwrap();

    let b = projekt(&dir.pfad("b"), &benutzer, "s-b");
    let in_b = lerne(&b, "Rust", "wird geprüft mit", "clippy");
    assert_eq!(in_b, "C-1", "dieselbe ID wie im Benutzer-Bundle");
    let mut tools = ToolRegistry::new();
    register_federated_graph_tools(&mut tools, b.clone());
    let call = |name: &str, args: serde_json::Value| tools.call(name, args).unwrap();

    // graph_search: beide, das Projekt zuerst.
    let suche = call("graph_search", json!({"query": "Rust"}));
    let (projekt_pos, benutzer_pos) = (suche.find("[C-1]"), suche.find("[u:C-1]"));
    assert!(projekt_pos.is_some() && benutzer_pos.is_some(), "{suche}");
    assert!(projekt_pos < benutzer_pos, "Projekt vor Benutzer: {suche}");

    // graph_neighbors: ein Name gilt beiden, eine ID genau einem Bundle.
    let name = call("graph_neighbors", json!({"entity": "Rust"}));
    assert!(name.contains("clippy") && name.contains("cargo"), "{name}");
    let projekt_id = call("graph_neighbors", json!({"entity": "E-1"}));
    assert!(
        projekt_id.contains("clippy") && !projekt_id.contains("cargo"),
        "{projekt_id}"
    );
    let benutzer_id = call("graph_neighbors", json!({"entity": "u:E-1"}));
    assert!(
        benutzer_id.contains("cargo") && !benutzer_id.contains("clippy"),
        "{benutzer_id}"
    );

    // graph_compare: dieselbe Regel — der Name sammelt aus beiden, die ID aus einem.
    lerne(&b, "Go", "wird geprüft mit", "vet");
    let beide = call("graph_compare", json!({"entities": ["Rust", "Go"]}));
    assert!(beide.contains("cargo [u:C-1]"), "{beide}");
    assert!(beide.contains("clippy [C-1]"), "{beide}");
    let nur_projekt = call("graph_compare", json!({"entities": ["E-1", "Go"]}));
    assert!(!nur_projekt.contains("cargo"), "{nur_projekt}");
    let nur_benutzer = call("graph_compare", json!({"entities": ["u:E-1", "Go"]}));
    assert!(
        nur_benutzer.contains("cargo") && !nur_benutzer.contains("clippy"),
        "{nur_benutzer}"
    );

    // graph_evidence: je Bundle die eigene Quelle, IDs qualifiziert.
    let evidenz = |id: &str| -> serde_json::Value {
        serde_json::from_str(&call("graph_evidence", json!({"claim_id": id}))).unwrap()
    };
    assert_eq!(evidenz("C-1")["sources"][0]["source_id"], "S-1");
    assert_eq!(evidenz("C-1")["sources"][0]["run"], "s-b");
    assert_eq!(evidenz("u:C-1")["sources"][0]["source_id"], "u:S-1");
    assert_eq!(evidenz("u:C-1")["sources"][0]["run"], "s-a");

    // graph_promote / graph_retract: das Benutzer-Wissen ist für Tools tabu.
    for (tool, args) in [
        ("graph_promote", json!({"claim_id": "u:C-1"})),
        (
            "graph_retract",
            json!({"claim_id": "u:C-1", "reason": "stimmt nicht"}),
        ),
    ] {
        let out = call(tool, args);
        assert!(out.starts_with("ERROR:"), "{tool}: {out}");
    }
    let user = b.user.as_ref().unwrap().store.snapshot();
    assert_eq!(user.claim("C-1").unwrap().status, ClaimStatus::Confirmed);

    // graph_retract ohne Präfix trifft das Projekt — und nur das.
    let out = call(
        "graph_retract",
        json!({"claim_id": "C-1", "reason": "veraltet"}),
    );
    assert!(out.contains("retracted"), "{out}");
    let nachher = call("graph_search", json!({"query": "Rust"}));
    assert!(
        !nachher.contains("[C-1]") && nachher.contains("[u:C-1]"),
        "{nachher}"
    );

    // graph_remember schreibt ins Projekt, auch wenn das Subjekt im
    // Benutzer-Wissen bekannt ist.
    let raw = call(
        "graph_remember",
        json!({"subject": "Rust", "predicate": "hat", "object": "Editionen"}),
    );
    let id = serde_json::from_str::<serde_json::Value>(&raw).unwrap()["claim_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!id.starts_with("u:"), "{raw}");
    assert_eq!(b.user.as_ref().unwrap().store.snapshot().claim_count(), 1);
}

#[test]
fn ohne_benutzer_bundle_ist_eine_u_id_einfach_unbekannt() {
    let store = Arc::new(GraphStore::in_memory());
    let fed = GraphFederation::single(store, GraphAccess::project("agentkit", "s", &[]));
    let mut tools = ToolRegistry::new();
    register_federated_graph_tools(&mut tools, fed);
    let out = tools
        .call("graph_evidence", json!({"claim_id": "u:C-1"}))
        .unwrap();
    assert!(out.contains("keine Quellen"), "{out}");
}

/// Das Budget einer Abfrage gilt für den ganzen Ausschnitt, nicht je Bundle.
#[test]
fn projekt_und_benutzer_teilen_sich_das_budget() {
    let dir = Verzeichnis::neu("budget");
    let a = projekt(&dir.pfad("a"), &dir.pfad("benutzer"), "s-a");
    for i in 0..3 {
        let id = lerne(
            &a,
            "Rust",
            &format!("hat Eigenschaft {i}"),
            &format!("Wert {i}"),
        );
        a.promote_to_user(&id, mensch()).unwrap();
    }
    let knapp = a.search(&GraphQuery::text("Rust").limits(4, 4000), false);
    assert_eq!(knapp.claims, 4, "{}", knapp.text);
    assert_eq!(knapp.text.matches("[u:").count(), 1, "{}", knapp.text);
    assert!(knapp.text.contains("weggelassen"), "{}", knapp.text);
}

/// Die Identität des Projektwissens hängt am Bundle, nicht am Pfad.
#[test]
fn ein_verschobener_checkout_findet_sein_projektwissen() {
    let dir = Verzeichnis::neu("umzug");
    let benutzer = dir.pfad("benutzer");
    {
        let alt = projekt(&dir.pfad("alt/.agentkit/graph"), &benutzer, "s-1");
        lerne(&alt, "Build", "läuft über", "make release");
    }
    std::fs::rename(dir.pfad("alt"), dir.pfad("neu")).unwrap();

    let neu = projekt(&dir.pfad("neu/.agentkit/graph"), &benutzer, "s-2");
    let treffer = neu.search(&GraphQuery::text("Wie läuft der Build?"), false);
    assert_eq!(treffer.claims, 1, "{}", treffer.text);
}

/// Ein Bundle aus der Zeit pfadbasierter Scope-IDs: sein Wissen bleibt
/// sichtbar, nichts wird umgeschrieben, Neues geht in den festen Scope.
#[test]
fn ein_altes_bundle_mit_pfad_scope_zeigt_sein_wissen_weiter() {
    let dir = Verzeichnis::neu("legacy");
    let bundle = dir.pfad("graph");
    let alter_scope = GraphScope::workspace("/home/dana/projekte/alt");
    {
        let store = GraphStore::open(&bundle).unwrap();
        let access = GraphAccess::session("agentkit", &alter_scope.id, "s-alt");
        let id = store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    "Deploy",
                    "braucht",
                    "VPN",
                    SourceDraft::new("document"),
                )),
                &access,
            )
            .unwrap()
            .claim_id
            .unwrap();
        store
            .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &access)
            .unwrap();
    }

    let fed = projekt(&bundle, &dir.pfad("benutzer"), "s-neu");
    assert_eq!(
        fed.project.access.promote,
        Some(GraphTarget::canonical(GraphScope::project()))
    );
    let treffer = fed.search(&GraphQuery::text("Was braucht der Deploy?"), false);
    assert_eq!(treffer.claims, 1, "{}", treffer.text);

    let neu = lerne(&fed, "Deploy", "dauert", "zehn Minuten");
    let index = fed.project.store.snapshot();
    assert_eq!(index.claim(&neu).unwrap().scope, GraphScope::project());
    assert_eq!(
        index.claim("C-1").unwrap().scope,
        alter_scope,
        "unverändert"
    );
    // Beides zusammen in einer Antwort.
    let beide = fed.search(&GraphQuery::text("Deploy"), false);
    assert_eq!(beide.claims, 2, "{}", beide.text);
}

/// `agentkit graph promote --user`: nur mit einem Ja des Menschen, und er
/// sieht vorher, was er freigibt.
#[test]
fn das_cli_hebt_nur_nach_rueckfrage_auf_die_benutzer_ebene() {
    use agentkit_graph::cli::{dispatch, GraphCli};

    let dir = Verzeichnis::neu("cli");
    let (projekt_dir, benutzer) = (dir.pfad("a"), dir.pfad("benutzer"));
    let id = lerne(
        &projekt(&projekt_dir, &benutzer, "s-a"),
        "ripgrep",
        "ersetzt",
        "grep",
    );
    let args = vec!["promote".to_string(), "--user".to_string(), id];
    let aufruf = |antwort: bool| {
        let mut gefragt = String::new();
        let mut confirm = |frage: &str| {
            gefragt = frage.to_string();
            antwort
        };
        let mut out = Vec::new();
        let ergebnis = dispatch(
            &args,
            &mut GraphCli {
                project_dir: projekt_dir.clone(),
                user_dir: Some(benutzer.clone()),
                human: mensch(),
                owner_key: None,
                confirm: &mut confirm,
            },
            &mut out,
        );
        (ergebnis, gefragt, String::from_utf8(out).unwrap())
    };

    let (nein, frage, _) = aufruf(false);
    assert!(nein.is_err());
    assert!(frage.contains("(ripgrep) --[ersetzt]--> (grep)"), "{frage}");
    assert!(frage.contains("cargo test"), "die Belegstelle: {frage}");
    assert!(!benutzer.exists(), "ohne Ja entsteht nichts");

    let (ja, _, ausgabe) = aufruf(true);
    ja.unwrap();
    assert!(ausgabe.contains("u:C-1"), "{ausgabe}");
    let b = projekt(&dir.pfad("b"), &benutzer, "s-b");
    assert_eq!(b.search(&GraphQuery::text("ripgrep"), false).claims, 1);
}
