//! `agentkit graph …` — was der Mensch am Graphen tun kann, und nur er.

use std::path::{Path, PathBuf};

use agentkit_graph::cli::{dispatch, GraphCli};
use agentkit_graph::model::Actor;
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    trust, ClaimDraft, ClaimStatus, GraphAccess, GraphWriteCommand, SourceDraft, BUNDLE_INDEX,
};

struct Umgebung {
    wurzel: PathBuf,
    schluessel: String,
}

impl Umgebung {
    fn neu(name: &str) -> Self {
        let wurzel = std::env::temp_dir().join(format!("graph_cli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wurzel);
        std::fs::create_dir_all(&wurzel).unwrap();
        Umgebung {
            wurzel,
            schluessel: trust::new_key(),
        }
    }
    fn projekt(&self) -> PathBuf {
        self.wurzel.join("projekt")
    }
    fn benutzer(&self) -> PathBuf {
        self.wurzel.join("benutzer")
    }

    /// Ruft das Verb auf; `antwort` ist, was der Mensch auf eine Rückfrage sagt.
    fn graph(&self, args: &[&str], antwort: bool) -> (Result<(), String>, String) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let mut confirm = |_: &str| antwort;
        let mut out = Vec::new();
        let ergebnis = dispatch(
            &args,
            &mut GraphCli {
                project_dir: self.projekt(),
                user_dir: Some(self.benutzer()),
                human: Actor::human("dana"),
                owner_key: Some(self.schluessel.clone()),
                confirm: &mut confirm,
            },
            &mut out,
        );
        (ergebnis, String::from_utf8(out).unwrap())
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ergebnis, ausgabe) = self.graph(args, true);
        ergebnis.unwrap_or_else(|e| panic!("{args:?}: {e}"));
        ausgabe
    }
}

impl Drop for Umgebung {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.wurzel);
    }
}

/// Belegte Aussage ins Projektwissen von `dir`; liefert die Claim-ID.
fn lerne(dir: &Path, s: &str, p: &str, o: &str) -> String {
    let store = GraphStore::open(dir).unwrap();
    let access = GraphAccess::project("agentkit", "s-1", &[]);
    let id = store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                s,
                p,
                o,
                SourceDraft::new("test_run").excerpt("cargo test"),
            )),
            &access,
        )
        .unwrap()
        .claim_id
        .unwrap();
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &access,
        )
        .unwrap();
    id
}

#[test]
fn status_list_und_show_zeigen_den_stand_ohne_etwas_anzulegen() {
    let u = Umgebung::neu("lesen");
    let leer = u.ok(&["status"]);
    assert!(leer.contains("noch nicht angelegt"), "{leer}");
    assert!(u.ok(&["list"]).contains("keine Aussagen"));
    assert!(!u.projekt().exists() && !u.benutzer().exists());

    let id = lerne(&u.projekt(), "ripgrep", "ersetzt", "grep");
    let status = u.ok(&["status"]);
    assert!(status.contains("1 Aussagen dauerhaft"), "{status}");
    assert!(status.contains("NICHT bestätigt"), "{status}");
    assert!(u.ok(&["list"]).contains("(ripgrep) --[ersetzt]--> (grep)"));
    let show = u.ok(&["show", &id]);
    assert!(
        show.contains("cargo test") && show.contains("test_run"),
        "{show}"
    );

    u.ok(&["promote", "--user", &id]);
    assert!(u.ok(&["list", "--user"]).contains("[u:C-1]"));
    assert!(u
        .ok(&["show", "u:C-1"])
        .contains("bestätigt von human:dana"));
}

#[test]
fn forget_loescht_endgueltig_und_nur_nach_rueckfrage() {
    let u = Umgebung::neu("forget");
    let bleibt = lerne(&u.projekt(), "ripgrep", "ersetzt", "grep");
    let weg = lerne(&u.projekt(), "Deploy", "braucht", "VPN");

    let (nein, _) = u.graph(&["forget", &weg], false);
    assert!(nein.is_err());
    assert_eq!(
        GraphStore::open_read_only(u.projekt())
            .unwrap()
            .claim_count(),
        2
    );

    u.ok(&["forget", &weg]);
    let index = GraphStore::open_read_only(u.projekt()).unwrap();
    assert!(index.claim(&weg).is_none());
    assert!(index.claim(&bleibt).is_some());
    // Mit der Aussage gehen ihre Quelle und ihre verwaisten Endpunkte.
    assert_eq!(index.source_count(), 1);
    assert_eq!(index.entity_count(), 2);
    assert!(u.ok(&["doctor"]).contains("Keine Auffälligkeiten"));
    assert!(u.graph(&["forget", "C-99"], true).0.is_err());
}

#[test]
fn merge_und_rename_pflegen_entities() {
    let u = Umgebung::neu("merge");
    lerne(&u.projekt(), "rg", "ersetzt", "grep");
    lerne(&u.projekt(), "ripgrep", "ersetzt", "grep");
    lerne(&u.projekt(), "ripgrep", "ist geschrieben in", "Rust");

    // E-1 = rg, E-3 = ripgrep.
    u.ok(&["merge", "E-1", "E-3"]);
    let index = GraphStore::open_read_only(u.projekt()).unwrap();
    assert!(index.entity("E-1").is_none());
    assert!(index
        .entity("E-3")
        .unwrap()
        .aliases
        .contains(&"rg".to_string()));
    // „rg ersetzt grep" und „ripgrep ersetzt grep" sind jetzt EINE Aussage
    // mit beiden Quellen.
    assert_eq!(index.claim_count(), 2);
    let ersetzt = index.claims().find(|c| c.predicate == "ersetzt").unwrap();
    assert_eq!(ersetzt.source_ids.len(), 2);
    assert_eq!(ersetzt.status, ClaimStatus::Confirmed);

    u.ok(&["rename", "E-3", "ripgrep", "(rg)"]);
    let index = GraphStore::open_read_only(u.projekt()).unwrap();
    let entity = index.entity("E-3").unwrap();
    assert_eq!(entity.canonical_name, "ripgrep (rg)");
    assert!(
        entity.aliases.contains(&"ripgrep".to_string()),
        "alter Name bleibt Alias"
    );
    assert!(u.ok(&["doctor"]).contains("Keine Auffälligkeiten"));

    // Nach dem Umbau schreibt ein neuer Store weiter, ohne eine ID doppelt zu vergeben.
    let neu = lerne(&u.projekt(), "fd", "ersetzt", "find");
    assert_eq!(
        GraphStore::open_read_only(u.projekt())
            .unwrap()
            .claim_count(),
        3
    );
    assert!(GraphStore::open_read_only(u.projekt())
        .unwrap()
        .claim(&neu)
        .is_some());
}

#[test]
fn doctor_meldet_reste_eines_abgebrochenen_commits() {
    let u = Umgebung::neu("doctor");
    lerne(&u.projekt(), "A", "nutzt", "B");
    std::fs::write(u.projekt().join("graph.lock"), "1").unwrap();
    let (ergebnis, ausgabe) = u.graph(&["doctor"], true);
    assert!(ergebnis.is_err());
    assert!(ausgabe.contains("graph.lock"), "{ausgabe}");
}

/// Ein Bundle, das der Benutzer selbst anlegt, trägt sein Zeichen; ein
/// fremdes erst nach `trust`.
#[test]
fn eigene_bundles_sind_gezeichnet_fremde_erst_nach_trust() {
    let u = Umgebung::neu("trust");
    // Fremd: liegt schon da, ohne Zeichen.
    lerne(&u.projekt(), "A", "nutzt", "B");
    assert!(!trust::is_signed(&u.projekt(), &u.schluessel));
    let ausgabe = u.ok(&["trust"]);
    assert!(ausgabe.contains("Bestätigt"), "{ausgabe}");
    assert!(trust::is_signed(&u.projekt(), &u.schluessel));
    assert!(u.ok(&["status"]).contains("(bestätigt)"));

    // Eigen: entsteht beim ersten Schreiben eines Stores mit Schlüssel.
    let eigen = u.wurzel.join("eigen");
    let store = GraphStore::open_lazy(&eigen)
        .unwrap()
        .with_owner_key(&u.schluessel);
    assert!(!eigen.exists());
    store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "A",
                "nutzt",
                "B",
                SourceDraft::new("test_run"),
            )),
            &GraphAccess::project("agentkit", "s", &[]),
        )
        .unwrap();
    assert!(eigen.join(BUNDLE_INDEX).exists());
    assert!(trust::is_signed(&eigen, &u.schluessel));
    assert!(!trust::is_signed(&eigen, &trust::new_key()));
}

#[test]
fn falsche_aufrufe_nennen_die_hilfe() {
    let u = Umgebung::neu("hilfe");
    for args in [&[][..], &["quatsch"], &["show"], &["promote", "C-1"]] {
        let (ergebnis, _) = u.graph(args, true);
        let fehler = ergebnis.unwrap_err();
        assert!(
            fehler.contains("agentkit graph status"),
            "{args:?}: {fehler}"
        );
    }
}
