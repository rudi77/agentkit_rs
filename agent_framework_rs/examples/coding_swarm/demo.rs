//! Coding-Swarm-Demo — ein Software-Dev-Team offline (FakeLlm, kein Netz).
//!
//!     cargo run --example coding_swarm --no-default-features
//!
//! Ein Tech-Lead-Orchestrator führt die Team-Rollen aus `examples/coding_swarm/roles`
//! über das `task`-Tool: architect (Analyse, read-only) → developer (Fix, schreibend)
//! → tester + reviewer (PARALLEL aus einer Antwort, beide read-only). Alle Events
//! laufen in denselben EventBus, getaggt mit ihrer `source` — der sichtbare Beweis
//! der Delegation und der Nebenläufigkeit. Der Fix passiert wirklich: die Demo legt
//! ein Mini-Workspace mit fehlerhafter `app.py` an, und der developer-Sub-Agent
//! editiert die Datei über die Sandbox-Tools.

use std::sync::Arc;

use agentkit::events::DONE;
use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{
    add_task_tool, builtin_roles, load_roles_from_dir, merge_roles, Agent, EventBus, McpHub,
    RunHandle, Strategy, ToolRegistry,
};

fn main() {
    // Mini-Workspace mit einem offensichtlichen Bug anlegen.
    let ws = std::env::temp_dir().join(format!("agentkit_coding_swarm_{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("Workspace anlegen");
    std::fs::write(ws.join("app.py"), "def add(a, b):\n    return a - b\n")
        .expect("app.py schreiben");
    let ws_str = ws.to_str().expect("Workspace-Pfad");

    // Team-Rollen aus dem Beispiel-Ordner laden (überschreiben gleichnamige Builtins).
    let roles_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/coding_swarm/roles");
    let roles = merge_roles(
        builtin_roles(),
        load_roles_from_dir(roles_dir.to_str().expect("Rollen-Pfad")),
    );
    assert!(
        roles.iter().any(|r| r.name == "architect"),
        "Rollen aus {} nicht gefunden",
        roles_dir.display()
    );

    // Skript der Sub-Agenten (ein FakeLlm für alle, Turns in Abfolge der Delegation).
    // tester + reviewer laufen PARALLEL und ziehen ihre Turns in beliebiger Reihenfolge —
    // deshalb sind ihre beiden Turns identisch (je genau EIN Text-Turn, kein Tool).
    let sub_llm = Arc::new(FakeLlm::new(vec![
        // architect: liest die Datei, liefert den Plan.
        vec![Chunk::tool(0, "a0", "read_file", r#"{"path":"app.py"}"#)],
        vec![Chunk::text(
            "Befund: add() subtrahiert statt zu addieren (app.py:2). \
             Änderung: in add() 'return a - b' durch 'return a + b' ersetzen. \
             Verifikation: add(2, 3) muss 5 ergeben.",
        )],
        // developer: setzt den Fix wirklich um (edit_file in der Sandbox).
        vec![Chunk::tool(
            0,
            "d0",
            "edit_file",
            r#"{"path":"app.py","old":"return a - b","new":"return a + b"}"#,
        )],
        vec![Chunk::text(
            "Fix umgesetzt: app.py:2 addiert jetzt ('+' statt '-').",
        )],
        // tester + reviewer (parallel, identische Ein-Turn-Skripte).
        vec![Chunk::text("Geprüft: keine Befunde.")],
        vec![Chunk::text("Geprüft: keine Befunde.")],
    ]));

    // Skript des Tech-Leads: architect → developer → tester+reviewer (EINE Antwort,
    // zwei task-Aufrufe → parallel) → Abschlussbericht.
    let orch_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "t0",
            "task",
            r#"{"subagent_type":"architect","prompt":"add() in app.py liefert falsche Summen. Lokalisiere die Ursache und liefere einen minimalen Fix-Plan."}"#,
        )],
        vec![Chunk::tool(
            0,
            "t1",
            "task",
            r#"{"subagent_type":"developer","prompt":"Ersetze in app.py in add() 'return a - b' durch 'return a + b'. Verifikation: add(2, 3) == 5."}"#,
        )],
        vec![
            Chunk::tool(
                0,
                "t2",
                "task",
                r#"{"subagent_type":"tester","prompt":"Prüfe, ob add() in app.py jetzt korrekt addiert."}"#,
            ),
            Chunk::tool(
                1,
                "t3",
                "task",
                r#"{"subagent_type":"reviewer","prompt":"Reviewe die Änderung an app.py: minimaler Additions-Fix in add()."}"#,
            ),
        ],
        vec![Chunk::text(
            "Team-Ergebnis: app.py gefixt (add() addiert wieder), Test und Review ohne Befunde.",
        )],
    ]));

    // Orchestrator verdrahten: task-Tool VOR dem Build registrieren, RunHandle teilen.
    let run = RunHandle::new();
    let mut tools = ToolRegistry::new();
    add_task_tool(
        &mut tools,
        run.clone(),
        sub_llm,
        ws_str,
        false,
        None,
        roles,
        Arc::new(McpHub::empty()),
    );

    let teamlead = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/coding_swarm/teamlead.md"),
    )
    .expect("teamlead.md lesen");

    let bus = EventBus::new();
    let q = bus.subscribe();
    let consumer = std::thread::spawn(move || {
        while let Ok(ev) = q.recv() {
            let src = if ev.source.is_empty() {
                "LEAD"
            } else {
                &ev.source
            };
            println!("[{src}] {}", ev.etype);
            if ev.etype == DONE && ev.source.is_empty() {
                break;
            }
        }
    });

    let mut lead = Agent::builder(orch_llm)
        .tools(tools)
        .system(&teamlead)
        .strategy(Strategy::Plain)
        .run_handle(run)
        .build();
    let answer = lead.run_on_bus(
        "Bug: add() in app.py liefert falsche Summen. Lass dein Team das beheben.",
        &bus,
        -1,
        None,
        "",
    );
    consumer.join().unwrap();

    let fixed = std::fs::read_to_string(ws.join("app.py")).expect("app.py lesen");
    println!("\n=== Antwort des Tech-Leads ===\n{answer}");
    println!("\n=== app.py nach dem Team-Einsatz ===\n{fixed}");
    assert!(fixed.contains("return a + b"), "Fix ist nicht angekommen");
    std::fs::remove_dir_all(&ws).ok();
}
