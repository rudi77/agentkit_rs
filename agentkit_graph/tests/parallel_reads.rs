//! Nebenläufigkeit: viele Leser, ein Schreiber, keine globale Lesesperre.
//!
//! Das ist die Anforderung, an der die Speicherwahl hängt — deshalb steht sie
//! hier als eigener Test und nicht als Kommentar in der README.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use agentkit_graph::retrieval::{self, GraphQuery};
use agentkit_graph::store::GraphStore;
use agentkit_graph::{ClaimDraft, GraphAccess, GraphWriteCommand, SourceDraft};

fn store_mit_grundwissen() -> (Arc<GraphStore>, GraphAccess) {
    let store = Arc::new(GraphStore::in_memory());
    let access = GraphAccess::session("tester", "ws", "run-1");
    for i in 0..20 {
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    "MCP Client",
                    "nutzt",
                    &format!("Session {i}"),
                    SourceDraft::new("document"),
                )),
                &access,
            )
            .unwrap();
    }
    (store, access)
}

#[test]
fn zehn_leser_und_ein_schreiber_arbeiten_gleichzeitig() {
    let (store, access) = store_mit_grundwissen();
    let readers = 10;
    let writes = 50;
    let barrier = Arc::new(Barrier::new(readers + 1));
    let gelesen = Arc::new(AtomicUsize::new(0));

    thread::scope(|scope| {
        for _ in 0..readers {
            let store = store.clone();
            let access = access.clone();
            let barrier = barrier.clone();
            let gelesen = gelesen.clone();
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..200 {
                    let index = store.snapshot();
                    let sub = retrieval::search(
                        &index,
                        &access.view,
                        &GraphQuery::text("Was nutzt der MCP Client?"),
                    );
                    assert!(!sub.is_empty());
                    gelesen.fetch_add(sub.claims.len(), Ordering::Relaxed);
                }
            });
        }

        let store_w = store.clone();
        let access_w = access.clone();
        scope.spawn(move || {
            barrier.wait();
            for i in 0..writes {
                store_w
                    .submit(
                        GraphWriteCommand::RecordClaim(ClaimDraft::new(
                            "MCP Client",
                            "blockiert",
                            &format!("Aufruf {i}"),
                            SourceDraft::new("tool_result"),
                        )),
                        &access_w,
                    )
                    .unwrap();
            }
        });
    });

    // Keine verlorene Mutation: 20 Grundaussagen + 50 neue, Revision lückenlos.
    assert_eq!(store.stats().claims, 20 + writes);
    assert_eq!(store.revision(), (20 + writes) as u64);
    assert!(gelesen.load(Ordering::Relaxed) > 0);
}

#[test]
fn ein_snapshot_aendert_sich_nicht_unter_dem_leser() {
    let (store, access) = store_mit_grundwissen();
    let vorher = store.snapshot();
    let anzahl_vorher = vorher.claim_count();

    store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "Neu",
                "kommt",
                "dazu",
                SourceDraft::new("tool_result"),
            )),
            &access,
        )
        .unwrap();

    // Der bereits gehaltene Snapshot bleibt exakt, was er war …
    assert_eq!(vorher.claim_count(), anzahl_vorher);
    assert_eq!(vorher.revision(), 20);
    // … der nächste sieht die Mutation.
    assert_eq!(store.snapshot().claim_count(), anzahl_vorher + 1);
    assert_eq!(store.revision(), 21);
}

#[test]
fn parallele_schreiber_verlieren_keine_mutation() {
    let store = Arc::new(GraphStore::in_memory());
    let writers = 4;
    let pro_writer = 25;

    thread::scope(|scope| {
        for w in 0..writers {
            let store = store.clone();
            scope.spawn(move || {
                // Jeder Schreiber ist ein eigener Agent mit eigenem Scope …
                let access = GraphAccess::swarm(&format!("agent-{w}"), "ws", "run-4711");
                for i in 0..pro_writer {
                    store
                        .submit(
                            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                                &format!("Agent {w}"),
                                "beobachtet",
                                &format!("Befund {w}-{i}"),
                                SourceDraft::new("tool_result"),
                            )),
                            &access,
                        )
                        .unwrap();
                }
            });
        }
    });

    // … und alle schreiben in denselben gemeinsamen Working Graph.
    let stats = store.stats();
    assert_eq!(stats.claims, writers * pro_writer);
    assert_eq!(stats.revision, (writers * pro_writer) as u64);

    // Jede Aussage trägt ihren echten Autor.
    let index = store.snapshot();
    for claim in index.claims() {
        assert!(claim.created_by.starts_with("agent-"));
    }
}
