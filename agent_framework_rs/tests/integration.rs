//! Tests ohne Netz — Tools, Memory, Events, MCP-Konvertierung und der Agent-Loop
//! mit einem FakeLlm, das OpenAI-Streaming-Chunks nachstellt. Spiegelt
//! `agent_framework/tests/test_agentkit.py`.

use std::sync::Arc;

use agentkit::events::{DONE, FINAL, TEXT_DELTA, TOOL_CALL, TOOL_RESULT};
use agentkit::llm::Chunk;
use agentkit::mcp::mcp_tools_to_schemas;
use agentkit::testing::FakeLlm;
use agentkit::{add_subagent, new_cancel, AgentEvent, EventBus, EventData, Strategy};
use agentkit::{
    Agent, CodingTools, LongTermMemory, Plan, ShortTermMemory, Skills, Step, ToolRegistry,
};
use serde_json::{json, Value};

// ------------------------------------------------------------------ Tools
#[test]
fn tool_add_and_call() {
    let mut reg = ToolRegistry::new();
    reg.add(
        "add",
        "Addiert zwei Zahlen.",
        json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
        |args: Value| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        },
    );
    let schema = &reg.schemas().unwrap()[0]["function"];
    assert_eq!(schema["name"], "add");
    assert_eq!(schema["description"], "Addiert zwei Zahlen.");
    assert_eq!(reg.call("add", json!({"a":2,"b":3})).unwrap(), "5");
}

#[test]
fn tool_unknown_is_soft_error() {
    let reg = ToolRegistry::new();
    assert!(reg.call("nope", json!({})).unwrap().contains("ERROR"));
}

#[test]
fn destructive_heuristic_flags_writers_not_readers() {
    use agentkit::is_likely_destructive;
    for d in [
        "write_file",
        "edit_file",
        "run_shell",
        "remember",
        "delete_x",
        "mcp__fs__put_object",
        "createIssue",
    ] {
        assert!(
            is_likely_destructive(d),
            "{d} sollte als zerstörerisch gelten"
        );
    }
    // "kill" steckt in "skill": eine reine Substring-Suche hätte den kompletten
    // Skills-Lesepfad unter --dry-run blockiert.
    for safe in [
        "read_file",
        "list_files",
        "recall",
        "add",
        "wetter",
        "read_skill",
        "list_skills",
        "expand_context_ref",
        "git_diff",
    ] {
        assert!(
            !is_likely_destructive(safe),
            "{safe} sollte erlaubt bleiben"
        );
    }
}

#[test]
fn dry_run_blocks_destructive_keeps_schemas_and_readers() {
    let mut reg = ToolRegistry::new();
    reg.add(
        "write_file",
        "Schreibt eine Datei.",
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        |_args: Value| Ok("WIRKLICH GESCHRIEBEN".to_string()),
    );
    reg.add(
        "read_file",
        "Liest eine Datei.",
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        |_args: Value| Ok("inhalt".to_string()),
    );

    let dry = reg.dry_run_blocking(agentkit::is_likely_destructive);
    // Schemas unverändert (Modell sieht denselben Werkzeugkasten).
    assert_eq!(dry.schemas().unwrap().len(), 2);
    // Schreib-Tool wird NICHT ausgeführt, sondern nur als Hinweis gemeldet.
    let blocked = dry.call("write_file", json!({"path": "x"})).unwrap();
    assert!(blocked.contains("[dry-run]") && !blocked.contains("WIRKLICH GESCHRIEBEN"));
    // Lese-Tool bleibt aktiv.
    assert_eq!(
        dry.call("read_file", json!({"path": "x"})).unwrap(),
        "inhalt"
    );
}

#[test]
fn json_mode_roundtrip_via_extract() {
    // Ein Modell, das JSON in einen Code-Fence verpackt — extract_json holt es heraus.
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "```json\n{\"status\": \"ok\", \"n\": 42}\n```",
    )]]));
    let mut agent = Agent::builder(llm)
        .system(agentkit::JSON_SYSTEM)
        .strategy(Strategy::Plain)
        .build();
    let raw = agent.run("Gib JSON");
    let clean = agentkit::extract_json(&raw).expect("gültiges JSON erwartet");
    assert_eq!(clean, r#"{"n":42,"status":"ok"}"#);
}

// ----------------------------------------------------------------- Memory

/// Ein Verlauf mit allen vier Rollen — Grundlage der Export-Tests.
fn memory_mit_werkzeuglauf() -> ShortTermMemory {
    let mut m = ShortTermMemory::new(Some("Du bist ein Coding-Agent."));
    m.add_user("Was ist 2+3?");
    m.add(agentkit::to_assistant_dict(
        None,
        &[json!({"id":"c1","type":"function",
                 "function":{"name":"add","arguments":"{\"a\":2,\"b\":3}"}})],
    ));
    m.add(json!({"role":"tool","tool_call_id":"c1","content":"5"}));
    m.add(agentkit::to_assistant_dict(
        Some("Das Ergebnis ist 5."),
        &[],
    ));
    m
}

#[test]
fn export_markdown_zeigt_zuege_rollen_und_werkzeuge() {
    let md = memory_mit_werkzeuglauf().to_markdown(true);
    assert!(md.contains("## System-Prompt"), "{md}");
    assert!(md.contains("## Zug 1 · Du"), "{md}");
    assert!(md.contains("Was ist 2+3?"));
    // Tool-Aufruf mit Argumenten und das zugehörige Ergebnis.
    assert!(md.contains("**add**"), "{md}");
    assert!(md.contains(r#"{"a":2,"b":3}"#), "{md}");
    assert!(md.contains("Ergebnis:"), "{md}");
    assert!(md.contains("Das Ergebnis ist 5."));
    // Volle Fassung kürzt nichts.
    assert!(!md.contains("Zeichen gekürzt"));
}

#[test]
fn export_markdown_kuerzt_ohne_full() {
    let mut m = memory_mit_werkzeuglauf();
    m.add(json!({"role":"tool","tool_call_id":"c2","content":"x".repeat(5000)}));
    let kurz = m.to_markdown(false);
    assert!(kurz.contains("Zeichen gekürzt"), "{kurz}");
    assert!(kurz.len() < 3000, "Kurzfassung war {} Zeichen", kurz.len());
    // Voll bleibt vollständig.
    assert!(m.to_markdown(true).contains(&"x".repeat(5000)));
}

/// Ein Tool-Ergebnis, das selbst ``` enthält (z. B. eine gelesene
/// Markdown-Datei), darf den umschließenden Code-Block nicht aufbrechen.
#[test]
fn export_markdown_verschachtelt_code_zaeune() {
    let mut m = ShortTermMemory::new(None);
    m.add_user("lies readme");
    m.add(json!({"role":"tool","tool_call_id":"c1",
                 "content":"# Titel\n```rust\nfn main() {}\n```\nEnde"}));
    let md = m.to_markdown(true);
    // Der äußere Zaun ist länger als der innere und der Inhalt bleibt drin.
    assert!(md.contains("````"), "{md}");
    assert!(md.contains("fn main() {}"), "{md}");
}

/// `/undo` nimmt Datei-Änderungen zurück: eine neu angelegte Datei wird
/// gelöscht, eine überschriebene bekommt ihren alten Inhalt zurück — in
/// umgekehrter Reihenfolge.
#[test]
fn checkpoints_nehmen_dateiaenderungen_zurueck() {
    let dir = std::env::temp_dir().join(format!("agentkit_undo_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let tools = CodingTools::new(dir.to_str().unwrap(), false);
    std::fs::write(dir.join("alt.txt"), "urspruenglich").unwrap();

    assert_eq!(tools.checkpoint_count(), 0);
    tools.write_file("neu.txt", "frisch").unwrap();
    tools.write_file("alt.txt", "ueberschrieben").unwrap();
    assert_eq!(tools.checkpoint_count(), 2);
    assert_eq!(tools.checkpoint_paths(), vec!["alt.txt", "neu.txt"]);

    // Jüngste zuerst: alt.txt bekommt seinen Inhalt zurück.
    assert!(tools.undo_last().unwrap().contains("wiederhergestellt"));
    assert_eq!(
        std::fs::read_to_string(dir.join("alt.txt")).unwrap(),
        "urspruenglich"
    );
    assert!(dir.join("neu.txt").exists(), "noch nicht dran");

    // Dann neu.txt — die gab es vorher nicht, wird also gelöscht.
    assert!(tools.undo_last().unwrap().contains("gelöscht"));
    assert!(!dir.join("neu.txt").exists());

    assert!(tools.undo_last().is_none(), "Stapel ist leer");
    std::fs::remove_dir_all(&dir).ok();
}

/// Der Undo-Stapel ist gedeckelt: ein langer Lauf darf nicht jede Vorversion
/// bis zum Prozessende im Speicher halten. Die ältesten Einträge fallen raus,
/// die jüngsten bleiben rücknehmbar.
#[test]
fn checkpoint_stapel_ist_gedeckelt() {
    let dir = std::env::temp_dir().join(format!("agentkit_undocap_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let tools = CodingTools::new(dir.to_str().unwrap(), false);

    // Deutlich mehr Änderungen als der Deckel zulässt.
    for i in 0..200 {
        tools.write_file(&format!("f{i}.txt"), "inhalt").unwrap();
    }
    let n = tools.checkpoint_count();
    assert!(n <= 50, "Stapel unbegrenzt gewachsen: {n}");

    // Die JÜNGSTE Änderung ist noch da — gedeckelt heißt nicht nutzlos.
    assert_eq!(tools.checkpoint_paths().first().unwrap(), "f199.txt");
    assert!(tools.undo_last().unwrap().contains("gelöscht"));
    assert!(!dir.join("f199.txt").exists());

    std::fs::remove_dir_all(&dir).ok();
}

/// Auch wenige, aber sehr große Dateien dürfen den Stapel nicht aufblähen —
/// dafür gibt es neben der Anzahl- die Byte-Grenze.
#[test]
fn checkpoint_stapel_deckelt_auch_grosse_dateien() {
    let dir = std::env::temp_dir().join(format!("agentkit_undobytes_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let tools = CodingTools::new(dir.to_str().unwrap(), false);

    // 20 × ~0,9 MB Vorversion = weit über der Byte-Grenze, aber unter der
    // Anzahl-Grenze: nur die Byte-Grenze kann hier greifen.
    let gross = "x".repeat(900 * 1024);
    for i in 0..20 {
        let name = format!("g{i}.txt");
        std::fs::write(dir.join(&name), &gross).unwrap();
        tools.write_file(&name, "klein").unwrap();
    }
    let n = tools.checkpoint_count();
    assert!(n < 20, "Byte-Grenze hat nicht gegriffen: {n} Einträge");

    std::fs::remove_dir_all(&dir).ok();
}

/// Ein `edit_file`, das gar nichts ändert (Muster nicht gefunden oder
/// mehrdeutig), darf KEINEN Checkpoint hinterlassen — sonst nähme `/undo`
/// eine Änderung zurück, die nie stattgefunden hat.
#[test]
fn abgelehnter_edit_erzeugt_keinen_checkpoint() {
    let dir = std::env::temp_dir().join(format!("agentkit_undo2_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let tools = CodingTools::new(dir.to_str().unwrap(), false);
    std::fs::write(dir.join("d.txt"), "eins zwei zwei").unwrap();

    // Muster gibt es nicht.
    assert!(tools
        .edit_file("d.txt", "drei", "x")
        .unwrap()
        .contains("ERROR"));
    assert_eq!(tools.checkpoint_count(), 0);
    // Muster ist mehrdeutig.
    assert!(tools
        .edit_file("d.txt", "zwei", "x")
        .unwrap()
        .contains("ERROR"));
    assert_eq!(tools.checkpoint_count(), 0);

    // Der echte Edit dagegen schon.
    tools.edit_file("d.txt", "eins", "drei").unwrap();
    assert_eq!(tools.checkpoint_count(), 1);
    tools.undo_last().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("d.txt")).unwrap(),
        "eins zwei zwei"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `AGENTKIT.md` im Workspace landet im System-Prompt — sonst wäre die
/// Datei stille Dekoration. Eine leere Datei zählt als nicht vorhanden.
#[test]
fn projekt_instruktionen_landen_im_system_prompt() {
    let dir = std::env::temp_dir().join(format!("agentkit_proj_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let ws = dir.to_str().unwrap();
    let datei = dir.join(agentkit::PROJECT_INSTRUCTIONS);

    // Ohne Datei: nichts im Prompt.
    assert!(agentkit::load_project_instructions(ws).is_none());

    // Leer bzw. nur Leerraum zählt nicht.
    std::fs::write(&datei, "   \n\n").unwrap();
    assert!(agentkit::load_project_instructions(ws).is_none());

    std::fs::write(&datei, "Immer auf Deutsch antworten.").unwrap();
    assert_eq!(
        agentkit::load_project_instructions(ws).as_deref(),
        Some("Immer auf Deutsch antworten.")
    );

    // Und der gebaute Agent trägt sie im System-Prompt.
    let cfg = agentkit::CodingAgentConfig {
        workspace: ws,
        strategy: Strategy::Plain,
        max_steps: 4,
        skills: None,
        agents: None,
        memory: None,
        subagents: false,
        system: None,
        verify: false,
        shell_timeout: 5,
        dry_run: false,
        extra_tools: None,
        helper_ctx_budget: None,
    };
    let (agent, ..) = agentkit::build_coding_agent(
        Arc::new(FakeLlm::new(vec![])),
        &cfg,
        Arc::new(|_: &str| true),
        Arc::new(agentkit::McpHub::empty()),
    );
    let sys = agent.memory.messages[0]["content"].as_str().unwrap();
    assert!(sys.contains("Immer auf Deutsch antworten."), "{sys}");
    assert!(sys.contains(agentkit::PROJECT_INSTRUCTIONS));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn turn_starts_findet_die_user_nachrichten() {
    let mut m = memory_mit_werkzeuglauf();
    assert_eq!(m.turn_starts(), vec![1]); // Index 0 ist der System-Prompt
    m.add_user("und mal 4?");
    assert_eq!(m.turn_starts(), vec![1, 5]);
    assert!(ShortTermMemory::new(None).turn_starts().is_empty());
}

/// `/rewind n` schneidet VOR Zug n ab: der Zug und alles danach fällt weg,
/// der System-Prompt und die früheren Züge bleiben.
#[test]
fn rewind_schneidet_vor_dem_zug_ab() {
    let mut m = memory_mit_werkzeuglauf(); // System + Zug 1 (mit Tool-Lauf)
    m.add_user("und mal 4?");
    m.add(agentkit::to_assistant_dict(Some("Das sind 20."), &[]));
    assert_eq!(m.messages.len(), 7);

    // Vor Zug 2 zurück: die letzten beiden Nachrichten fallen weg.
    assert_eq!(m.rewind_to_turn(2), Some(2));
    assert_eq!(m.messages.len(), 5);
    assert_eq!(m.turn_starts(), vec![1]);
    assert_eq!(m.messages[0]["role"], "system");

    // Vor Zug 1: nur der System-Prompt bleibt stehen.
    assert_eq!(m.rewind_to_turn(1), Some(4));
    assert_eq!(m.messages.len(), 1);
    assert_eq!(m.messages[0]["role"], "system");
}

#[test]
fn rewind_auf_unbekannten_zug_aendert_nichts() {
    let mut m = memory_mit_werkzeuglauf();
    let vorher = m.messages.len();
    assert_eq!(m.rewind_to_turn(9), None);
    assert_eq!(m.rewind_to_turn(0), None); // Züge sind 1-basiert
    assert_eq!(m.messages.len(), vorher);
}

/// Der gesicherte Ast von `/fork` muss sich wieder als Session laden lassen —
/// sonst wäre das Wegbranchen eine Einbahnstraße.
#[test]
fn fork_ast_ist_wieder_ladbar() {
    let ast = std::env::temp_dir().join(format!("agentkit_fork_{}.json", std::process::id()));
    let ast = ast.to_str().unwrap();

    let mut m = memory_mit_werkzeuglauf();
    m.add_user("und mal 4?");
    let voll = m.messages.len();
    m.save(ast).unwrap();
    m.rewind_to_turn(2).unwrap();
    assert!(m.messages.len() < voll);

    let geladen = ShortTermMemory::load(ast).unwrap();
    assert_eq!(geladen.messages.len(), voll);
    assert_eq!(geladen.turn_starts().len(), 2);
    std::fs::remove_file(ast).ok();
}

/// Ohne ctxman geht der Rewind über die Agenten-Ebene durch; mit aktivem
/// ctxman lehnt er ab, statt nur den Spiegel zu kürzen (siehe
/// `Agent::rewind_to_turn`). Hier der Normalfall — der ctxman-Fall braucht
/// das Feature und steht in den ctxman-Tests.
#[test]
fn agent_rewind_kuerzt_ohne_ctxman() {
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(vec![])))
        .strategy(Strategy::Plain)
        .system("sys")
        .build();
    agent.memory.add_user("erste frage");
    agent
        .memory
        .add(agentkit::to_assistant_dict(Some("a"), &[]));
    agent.memory.add_user("zweite frage");

    assert_eq!(agent.rewind_check(), agentkit::RewindOutcome::Done(0));
    assert_eq!(
        agent.rewind_to_turn(2),
        agentkit::RewindOutcome::Done(1),
        "Zug 2 besteht nur aus der User-Nachricht"
    );
    assert_eq!(agent.memory.turn_starts().len(), 1);
    assert_eq!(agent.rewind_to_turn(9), agentkit::RewindOutcome::NoSuchTurn);
    assert!(!agent.context_managed());
}

/// Die Zug-Nummern in `/export` und `/rewind` MÜSSEN dieselben sein: wer
/// „Zug 3" im Export liest, tippt `/rewind 3`. Beide leiten die Nummerierung
/// getrennt her (Zähler im Renderer, `turn_starts` im Speicher) — hier
/// festgenagelt, damit sie nicht auseinanderlaufen.
#[test]
fn export_und_rewind_zaehlen_zuege_gleich() {
    let mut m = memory_mit_werkzeuglauf();
    m.add_user("und mal 4?");
    m.add(agentkit::to_assistant_dict(Some("20."), &[]));
    m.add_user("und durch 2?");

    let md = m.to_markdown(true);
    let ueberschriften = md.matches("## Zug ").count();
    assert_eq!(ueberschriften, m.turn_starts().len());
    for n in 1..=m.turn_starts().len() {
        assert!(
            md.contains(&format!("## Zug {n} · Du")),
            "Zug {n} fehlt:\n{md}"
        );
    }
}

/// `/compact <hinweis>` muss den Hinweis wirklich in den Summarize-Prompt
/// tragen — sonst wäre das Argument reine Dekoration.
#[test]
fn compact_hinweis_landet_im_prompt() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct PromptSpion {
        gesehen: Mutex<Vec<String>>,
    }
    impl agentkit::Llm for PromptSpion {
        fn complete(
            &self,
            messages: &[Value],
            _tools: Option<&[Value]>,
        ) -> Result<agentkit::Message, String> {
            self.gesehen
                .lock()
                .unwrap()
                .push(messages[0]["content"].as_str().unwrap_or("").to_string());
            Ok(agentkit::Message {
                content: Some("Zusammenfassung".to_string()),
                tool_calls: Vec::new(),
            })
        }
        fn stream(
            &self,
            _messages: &[Value],
            _tools: Option<&[Value]>,
        ) -> Result<agentkit::llm::ChunkStream, String> {
            Err("nicht benutzt".to_string())
        }
    }

    let mut mem = ShortTermMemory::new(Some("sys"));
    for i in 0..8 {
        mem.add_user(&format!("frage {i}"));
    }
    let spion = PromptSpion::default();
    assert!(mem.compact_with_hint(&spion, 4, Some("behalte die API-Details")));
    let prompt = &spion.gesehen.lock().unwrap()[0];
    assert!(prompt.contains("behalte die API-Details"), "{prompt}");

    // Ohne Hinweis bleibt der Prompt der alte (kein leeres "Achte dabei").
    let mut mem2 = ShortTermMemory::new(Some("sys"));
    for i in 0..8 {
        mem2.add_user(&format!("frage {i}"));
    }
    let spion2 = PromptSpion::default();
    mem2.compact_with_hint(&spion2, 4, None);
    assert!(!spion2.gesehen.lock().unwrap()[0].contains("Achte dabei"));
}

/// `/compact` verdichtet auf Kommando, auch wenn das Budget noch nicht
/// erreicht ist — sonst hätte der Befehl keinen Sinn.
#[test]
fn agent_compact_now_verdichtet_ohne_budget_druck() {
    let llm = Arc::new(FakeLlm::new(vec![]));
    let mut agent = Agent::builder(llm)
        .strategy(Strategy::Plain)
        .system("sys")
        .token_budget(1_000_000) // weit weg vom Druck
        .build();
    for i in 0..10 {
        agent.memory.add_user(&format!("frage {i}"));
    }
    let vorher = agent.memory.messages.len();
    assert!(agent.compact_now(None));
    assert!(agent.memory.messages.len() < vorher);
    // System-Prompt überlebt.
    assert_eq!(agent.memory.messages[0]["role"], "system");
}

#[test]
fn short_term_compaction_keeps_system_and_tail() {
    let mut mem = ShortTermMemory::new(Some("SYS"));
    for i in 0..10 {
        mem.add(json!({"role":"user","content":format!("nachricht {i}")}));
    }
    let llm = FakeLlm::new(vec![]);
    let compacted = mem.compact(&llm, 3);
    assert!(compacted);
    assert_eq!(mem.messages[0], json!({"role":"system","content":"SYS"}));
    assert_eq!(mem.messages[1]["role"], "system"); // Compaction-Notiz
    assert_eq!(mem.messages.last().unwrap()["content"], "nachricht 9");
    assert_eq!(mem.messages.len(), 1 + 1 + 3);
}

#[test]
fn long_term_memory_roundtrip() {
    let dir = std::env::temp_dir().join(format!("agentkit_ltm_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mem.jsonl");
    let p = path.to_str().unwrap();
    let ltm = LongTermMemory::new(p);
    ltm.remember("Rudi mag Kaffee am Morgen", vec![]);
    ltm.remember("Das Projekt heißt fsod", vec![]);
    assert!(ltm.recall("kaffee", 3).contains("Kaffee"));
    // Persistenz: neue Instanz liest die Datei.
    let again = LongTermMemory::new(p);
    assert_eq!(again.len(), 2);
    let mut reg = ToolRegistry::new();
    ltm.register_tools(&mut reg);
    assert!(reg.has("remember") && reg.has("recall"));
    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------------- Events
#[test]
fn eventbus_fans_out_to_all_subscribers() {
    let bus = EventBus::new();
    let a = bus.subscribe();
    let b = bus.subscribe();
    bus.publish(AgentEvent::new("step", EventData::Step { step: 1 }));
    assert_eq!(a.recv().unwrap().data, EventData::Step { step: 1 });
    assert_eq!(b.recv().unwrap().data, EventData::Step { step: 1 });
}

// -------------------------------------------------------------------- MCP
#[test]
fn mcp_tools_to_schemas_works() {
    let tools = vec![
        json!({"name":"add","description":"adds","inputSchema":{"type":"object","properties":{}}}),
    ];
    let out = mcp_tools_to_schemas(&tools);
    assert_eq!(out[0]["function"]["name"], "add");
    assert_eq!(out[0]["type"], "function");
}

#[test]
fn load_mcp_config_parses_servers() {
    let dir = std::env::temp_dir().join(format!("agentkit_mcpcfg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".mcp.json");
    std::fs::write(
        &path,
        r#"{"mcpServers": {
            "git": {"command": "uvx", "args": ["mcp-server-git", "--repo", "."],
                    "env": {"TOKEN": "x"}},
            "fs":  {"command": "node", "args": ["server.js"], "disabled": true}
        }}"#,
    )
    .unwrap();

    let specs = agentkit::load_mcp_config(path.to_str().unwrap()).unwrap();
    // Alphabetisch sortiert: fs, git.
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].name, "fs");
    assert!(specs[0].disabled);
    assert_eq!(specs[1].name, "git");
    assert_eq!(specs[1].command, "uvx");
    assert_eq!(specs[1].args, vec!["mcp-server-git", "--repo", "."]);
    assert_eq!(specs[1].env, vec![("TOKEN".to_string(), "x".to_string())]);

    // Discovery findet die .mcp.json im "Workspace".
    let found = agentkit::discover_mcp_config(dir.to_str().unwrap());
    assert!(found.is_some());

    // Leerer Hub: register_enabled ist ein No-Op (keine Tools), is_empty stimmt.
    let hub = agentkit::McpHub::empty();
    assert!(hub.is_empty());
    let mut reg = ToolRegistry::new();
    hub.register_enabled(&mut reg);
    assert!(reg.schemas().is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_mcp_config_rejects_missing_command() {
    let dir = std::env::temp_dir().join(format!("agentkit_mcpbad_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".mcp.json");
    std::fs::write(&path, r#"{"mcpServers": {"x": {"args": []}}}"#).unwrap();
    let err = agentkit::load_mcp_config(path.to_str().unwrap()).unwrap_err();
    assert!(
        err.contains("command"),
        "Fehler nennt fehlendes command: {err}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------- Agent-Loop
fn agent_with_tool() -> Agent {
    let mut reg = ToolRegistry::new();
    reg.add(
        "add",
        "Addiert zwei Zahlen.",
        json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
        |args: Value| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        },
    );
    let turns = vec![
        vec![Chunk::tool(
            0,
            "c1",
            "add",
            &json!({"a":2,"b":3}).to_string(),
        )],
        vec![Chunk::text("Das Ergebnis "), Chunk::text("ist 5.")],
    ];
    Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(reg)
        .strategy(Strategy::Plain)
        .build()
}

#[test]
fn agent_runs_tool_then_answers() {
    let mut agent = agent_with_tool();
    let mut events = Vec::new();
    agent.run_cb("Was ist 2+3?", None, |ev| events.push(ev));
    let types: Vec<&str> = events.iter().map(|e| e.etype).collect();
    assert!(types.contains(&TOOL_CALL) && types.contains(&TOOL_RESULT));
    let tr = events.iter().find(|e| e.etype == TOOL_RESULT).unwrap();
    assert_eq!(
        tr.data,
        EventData::ToolResult {
            name: "add".into(),
            result: "5".into()
        }
    );
    let final_ev = events.iter().find(|e| e.etype == FINAL).unwrap();
    assert_eq!(
        final_ev.data,
        EventData::Final("Das Ergebnis ist 5.".into())
    );
}

#[test]
fn agent_run_returns_final_string() {
    let mut agent = agent_with_tool();
    assert_eq!(agent.run("Was ist 2+3?"), "Das Ergebnis ist 5.");
}

#[test]
fn agent_strategy_injects_preamble() {
    let agent = Agent::builder(Arc::new(FakeLlm::new(vec![])))
        .strategy(Strategy::Plan)
        .system("Sei knapp.")
        .build();
    let sys = agent.memory.messages[0]["content"].as_str().unwrap();
    assert!(sys.contains("Plan-and-Execute") && sys.contains("Sei knapp."));
}

#[test]
fn agent_cancel_before_start() {
    let mut agent = agent_with_tool();
    let cancel = new_cancel();
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut events = Vec::new();
    agent.run_cb("egal", Some(&cancel), |ev| events.push(ev));
    assert_eq!(events[0].etype, "cancelled");
}

#[test]
fn agent_run_on_bus_emits_done() {
    let mut agent = agent_with_tool();
    let bus = EventBus::new();
    let q = bus.subscribe();
    agent.run_on_bus("Was ist 2+3?", &bus, 7, None, "");
    let mut seen = Vec::new();
    while let Ok(ev) = q.try_recv() {
        seen.push(ev);
    }
    assert_eq!(seen.last().unwrap().etype, DONE);
    assert!(seen.iter().all(|e| e.task_id == 7));
}

// ---------------------------------------------------------------- Planning
#[test]
fn plan_update_and_render() {
    let plan = Plan::new();
    let out = plan.update(vec![
        Step {
            step: "Code schreiben".into(),
            status: "done".into(),
        },
        Step {
            step: "Tests".into(),
            status: "in_progress".into(),
        },
        Step {
            step: "Aufräumen".into(),
            status: "pending".into(),
        },
    ]);
    assert!(out.contains("[x] 1. Code schreiben"));
    assert!(out.contains("[~] 2. Tests"));
    assert!(out.contains("[ ] 3. Aufräumen"));
}

#[test]
fn plan_registers_tool_and_fires_callback() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let seen = Arc::new(AtomicUsize::new(0));
    let s2 = seen.clone();
    let plan = Plan::with_on_update(move |steps| s2.store(steps.len(), Ordering::SeqCst));
    let mut reg = ToolRegistry::new();
    plan.register_tool(&mut reg);
    assert!(reg.has("update_plan"));
    reg.call(
        "update_plan",
        json!({"steps":[{"step":"A","status":"pending"}]}),
    )
    .unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

// ----------------------------------------------------------- Coding-Tools
#[test]
fn coding_tools_sandbox_and_io() {
    let dir = std::env::temp_dir().join(format!("agentkit_ct_{}", std::process::id()));
    let mut reg = ToolRegistry::new();
    CodingTools::new(dir.to_str().unwrap(), false).register(&mut reg, None);
    assert!(reg
        .call(
            "write_file",
            json!({"path":"a.txt","content":"x".repeat(20)})
        )
        .unwrap()
        .contains("20 Zeichen"));
    assert_eq!(
        reg.call("read_file", json!({"path":"a.txt"})).unwrap(),
        "x".repeat(20)
    );
    assert!(reg
        .call("list_files", json!({"path":"."}))
        .unwrap()
        .contains("a.txt"));
    reg.call("write_file", json!({"path":"b.txt","content":"hallo welt"}))
        .unwrap();
    reg.call(
        "edit_file",
        json!({"path":"b.txt","old":"welt","new":"agent"}),
    )
    .unwrap();
    assert_eq!(
        reg.call("read_file", json!({"path":"b.txt"})).unwrap(),
        "hallo agent"
    );
    // Sandbox-Ausbruch -> Err (im Agent-Loop würde daraus ein ERROR-Ergebnis).
    assert!(reg
        .call("read_file", json!({"path":"../../etc/passwd"}))
        .is_err());
    std::fs::remove_dir_all(&dir).ok();
}

/// Ein Symlink im Workspace, der nach außen zeigt, ist KEIN Schlupfloch: die
/// lexikalische Normalisierung folgt ihm nicht, deshalb prüft `safe()` zusätzlich
/// den real aufgelösten Pfad. Vorher ließen sich fremde Dateien darüber lesen und
/// überschreiben.
#[test]
#[cfg(unix)]
fn coding_tools_reject_symlink_out_of_sandbox() {
    let base = std::env::temp_dir().join(format!("agentkit_link_{}", std::process::id()));
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let geheim = base.join("geheim.txt");
    std::fs::write(&geheim, "GEHEIM").unwrap();
    std::os::unix::fs::symlink(&geheim, ws.join("link.txt")).unwrap();
    // Verzeichnis-Link: auch der Umweg über einen Ordner muss scheitern.
    std::os::unix::fs::symlink(&base, ws.join("raus")).unwrap();

    let mut reg = ToolRegistry::new();
    CodingTools::new(ws.to_str().unwrap(), false).register(&mut reg, None);

    assert!(reg.call("read_file", json!({"path":"link.txt"})).is_err());
    assert!(reg
        .call("write_file", json!({"path":"link.txt","content":"weg"}))
        .is_err());
    assert!(reg
        .call("read_file", json!({"path":"raus/geheim.txt"}))
        .is_err());
    // Neue Datei im Link-Ziel anlegen: scheitert am realen Elternverzeichnis.
    assert!(reg
        .call("write_file", json!({"path":"raus/neu.txt","content":"x"}))
        .is_err());
    assert_eq!(std::fs::read_to_string(&geheim).unwrap(), "GEHEIM");

    // glob/grep laufen nicht über den Link hinaus.
    let treffer = reg
        .call("glob_files", json!({"pattern":"**/*.txt"}))
        .unwrap();
    assert!(!treffer.contains("geheim"), "{treffer}");

    // Reguläre Dateien bleiben erreichbar.
    reg.call("write_file", json!({"path":"drin.txt","content":"ok"}))
        .unwrap();
    assert_eq!(
        reg.call("read_file", json!({"path":"drin.txt"})).unwrap(),
        "ok"
    );

    std::fs::remove_dir_all(&base).ok();
}

#[test]
#[cfg(not(windows))]
fn coding_tools_run_shell_no_approval() {
    let dir = std::env::temp_dir().join(format!("agentkit_sh_{}", std::process::id()));
    let mut reg = ToolRegistry::new();
    CodingTools::new(dir.to_str().unwrap(), false).register(&mut reg, None);
    let out = reg
        .call("run_shell", json!({"command":"echo hallo"}))
        .unwrap();
    assert!(out.contains("hallo") && out.contains("exit=0"));
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------------------------ Skills
fn write_skill(root: &std::path::Path, folder: &str, name: &str, description: &str, body: &str) {
    let d = root.join(folder);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{body}\n"),
    )
    .unwrap();
}

#[test]
fn skills_index_only_frontmatter() {
    let dir = std::env::temp_dir().join(format!("agentkit_sk1_{}", std::process::id()));
    write_skill(&dir, "alpha", "alpha", "Macht A", "GEHEIMER LANGER BODY");
    write_skill(&dir, "beta", "beta", "Macht B", "Schritt 1.");
    let sk = Skills::new(dir.to_str().unwrap());
    let idx = sk.index();
    let names: std::collections::HashSet<_> = idx.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"].into_iter().collect());
    assert!(!sk.list_skills().contains("GEHEIMER LANGER BODY"));
    assert!(idx.iter().any(|s| s.description == "Macht A"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn skills_read_full_body_on_demand() {
    let dir = std::env::temp_dir().join(format!("agentkit_sk2_{}", std::process::id()));
    write_skill(&dir, "alpha", "alpha", "Macht A", "GEHEIMER LANGER BODY");
    let sk = Skills::new(dir.to_str().unwrap());
    assert!(sk.read_skill("alpha").contains("GEHEIMER LANGER BODY"));
    assert!(sk.read_skill("gibtsnicht").contains("kein Skill"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn skills_read_by_folder_name_when_frontmatter_differs() {
    let dir = std::env::temp_dir().join(format!("agentkit_sk3_{}", std::process::id()));
    write_skill(
        &dir,
        "ordner-x",
        "anzeige-name",
        "Beschreibung",
        "Schritt 1.",
    );
    let sk = Skills::new(dir.to_str().unwrap());
    assert!(sk.read_skill("anzeige-name").contains("anzeige-name"));
    assert!(sk.read_skill("ordner-x").contains("anzeige-name"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn agent_skills_param_registers_tools() {
    let agent = Agent::builder(Arc::new(FakeLlm::new(vec![])))
        .skills(Skills::new("./does-not-matter"))
        .strategy(Strategy::Plain)
        .build();
    assert!(agent.tools.has("list_skills") && agent.tools.has("read_skill"));
}

// ------------------------------------------------------- Parallel + Subagents
#[test]
fn parallel_tools_preserve_order_and_pairing() {
    let mut reg = ToolRegistry::new();
    reg.add(
        "slow",
        "Verdoppelt x.",
        json!({"type":"object","properties":{"x":{"type":"integer"}},"required":["x"]}),
        |args: Value| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok((args["x"].as_i64().unwrap_or(0) * 2).to_string())
        },
    );
    let turn1 = vec![
        Chunk::tool(0, "t0", "slow", "{\"x\": 1}"),
        Chunk::tool(1, "t1", "slow", "{\"x\": 2}"),
        Chunk::tool(2, "t2", "slow", "{\"x\": 3}"),
    ];
    let turn2 = vec![Chunk::text("fertig")];
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(vec![turn1, turn2])))
        .tools(reg)
        .strategy(Strategy::Plain)
        .parallel_tools(true)
        .build();
    let mut results = Vec::new();
    // Aufruf und Ergebnis je Korrelations-ID mitschreiben — die Zuordnung darf
    // NICHT an der Reihenfolge hängen: drei gleichnamige Tools in einem Schritt
    // sind für einen Konsumenten sonst nicht auseinanderzuhalten.
    let mut aufrufe: Vec<(String, String)> = Vec::new();
    let mut ergebnisse: Vec<(String, String)> = Vec::new();
    agent.run_cb("rechne", None, |ev| {
        match &ev.data {
            EventData::ToolCall { args, .. } => {
                aufrufe.push((ev.call_id.clone(), args["x"].to_string()))
            }
            EventData::ToolResult { result, .. } => {
                results.push(result.clone());
                ergebnisse.push((ev.call_id.clone(), result.clone()));
            }
            _ => {}
        };
    });
    assert_eq!(results, vec!["2", "4", "6"]);
    let tool_ids: Vec<String> = agent
        .memory
        .messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tool_ids, vec!["t0", "t1", "t2"]);

    // Dieselben IDs, die das Modell vergeben hat, stehen in den Ereignissen —
    // und zwar so, dass sich aus ihnen allein rekonstruieren lässt, welches
    // Ergebnis zu welchem Argument gehört.
    assert_eq!(
        aufrufe,
        vec![
            ("t0".into(), "1".into()),
            ("t1".into(), "2".into()),
            ("t2".into(), "3".into())
        ]
    );
    for (id, arg) in &aufrufe {
        let (_, ergebnis) = ergebnisse
            .iter()
            .find(|(eid, _)| eid == id)
            .unwrap_or_else(|| panic!("kein Ergebnis mit call_id {id}"));
        let erwartet = (arg.parse::<i64>().unwrap() * 2).to_string();
        assert_eq!(ergebnis, &erwartet, "call_id {id} paart falsch");
    }
}

#[test]
fn add_subagent_registers_delegate_tool() {
    let mut orch = ToolRegistry::new();
    let sub_llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("Steckbrief Wien")]]));
    add_subagent(
        &mut orch,
        "delegate",
        "Delegiert einen Auftrag.",
        sub_llm,
        None,
        Some("Recherche."),
        Strategy::Plain,
        None,
    );
    assert!(orch.has("delegate"));
    assert_eq!(
        orch.call("delegate", json!({"auftrag":"Wien"})).unwrap(),
        "Steckbrief Wien"
    );
}

/// Jeder Agent, der auf einem Bus läuft, legt am Ende seinen Kontext daneben —
/// der Haupt-Agent UND jeder Sub-Agent. Vorher schrieb nur die CLI diesen
/// Datensatz und nur für den Haupt-Agenten; der Kontext eines Sub-Agenten war
/// nirgends zu sehen, obwohl er mit dem Tool-Aufruf verschwindet, der ihn
/// erzeugt hat.
#[test]
fn jeder_agent_legt_seinen_kontext_auf_den_bus() {
    let bus = EventBus::new();
    let q = bus.subscribe();

    let sub_llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("Steckbrief Wien")]]));
    let mut orch_tools = ToolRegistry::new();
    add_subagent(
        &mut orch_tools,
        "delegate",
        "Delegiert.",
        sub_llm,
        None,
        Some("Recherche."),
        Strategy::Plain,
        Some(bus.clone()),
    );
    let orch_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "d0", "delegate", "{\"auftrag\": \"Wien\"}")],
        vec![Chunk::text("fertig")],
    ]));
    let mut orchestrator = Agent::builder(orch_llm)
        .tools(orch_tools)
        .strategy(Strategy::Plain)
        .build();
    orchestrator.run_on_bus("Vergleiche Wien.", &bus, -1, None, "");

    let mut kontexte: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    while let Ok(ev) = q.try_recv() {
        if let EventData::Structured { kind, payload } = &ev.data {
            if kind == agentkit::CONTEXT_SNAPSHOT {
                kontexte.insert(ev.source.clone(), payload.clone());
            }
        }
    }

    let haupt = kontexte.get("").expect("Kontext des Haupt-Agenten fehlt");
    let sub = kontexte
        .get("delegate:Wien")
        .expect("Kontext des Sub-Agenten fehlt");

    // Der Datensatz trägt die Nachrichten selbst, nicht nur Zahlen — sonst
    // wüsste man wieder nur, WIE VIEL im Kontext steht, nicht WAS.
    let sub_texte = sub["messages"].to_string();
    assert!(
        sub_texte.contains("Recherche.") && sub_texte.contains("Steckbrief Wien"),
        "der Sub-Agenten-Kontext trägt weder seinen System-Prompt noch seine Antwort: {sub_texte}"
    );
    assert!(haupt["messages"].to_string().contains("Vergleiche Wien."));
    assert!(haupt["report"]["total"].as_u64().is_some());
    assert_eq!(haupt["messages_from"], 0);
}

/// Der Kontext-Datensatz trägt nur den ZUWACHS: die ganze Historie nach jedem
/// Zug erneut zu schicken wäre in einem langen Gespräch quadratisch. Wer liest,
/// hängt bei `messages_from` an.
#[test]
fn der_kontext_datensatz_traegt_nur_den_zuwachs() {
    let bus = EventBus::new();
    let q = bus.subscribe();
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::text("erste Antwort")],
        vec![Chunk::text("zweite Antwort")],
    ]));
    let mut agent = Agent::builder(llm).strategy(Strategy::Plain).build();

    agent.run_on_bus("erste Frage", &bus, -1, None, "");
    agent.run_on_bus("zweite Frage", &bus, -1, None, "");

    let mut daten = Vec::new();
    while let Ok(ev) = q.try_recv() {
        if let EventData::Structured { kind, payload } = &ev.data {
            if kind == agentkit::CONTEXT_SNAPSHOT {
                daten.push(payload.clone());
            }
        }
    }
    assert_eq!(daten.len(), 2);

    assert_eq!(daten[0]["messages_from"], 0);
    let ab = daten[1]["messages_from"].as_u64().unwrap();
    assert_eq!(
        ab,
        daten[0]["messages_total"].as_u64().unwrap(),
        "der zweite Datensatz setzt nicht dort an, wo der erste endete"
    );
    // Und er wiederholt den ersten Zug nicht.
    let zweiter = daten[1]["messages"].to_string();
    assert!(zweiter.contains("zweite Frage"));
    assert!(
        !zweiter.contains("erste Frage"),
        "der zweite Datensatz schickt den ersten Zug noch einmal: {zweiter}"
    );
}

#[test]
fn subagent_forwards_events_to_shared_bus() {
    let bus = EventBus::new();
    let q = bus.subscribe();

    let sub_llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("Steckbrief Wien")]]));
    let mut orch_tools = ToolRegistry::new();
    add_subagent(
        &mut orch_tools,
        "delegate",
        "Delegiert.",
        sub_llm,
        None,
        Some("Recherche."),
        Strategy::Plain,
        Some(bus.clone()),
    );

    let orch_llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "d0", "delegate", "{\"auftrag\": \"Wien\"}")],
        vec![Chunk::text("Tabelle fertig")],
    ]));
    let mut orchestrator = Agent::builder(orch_llm)
        .tools(orch_tools)
        .strategy(Strategy::Plain)
        .build();
    let final_answer = orchestrator.run_on_bus("Vergleiche Wien.", &bus, -1, None, "");

    let mut seen = Vec::new();
    while let Ok(ev) = q.try_recv() {
        seen.push(ev);
    }

    let sources: std::collections::HashSet<&str> = seen.iter().map(|e| e.source.as_str()).collect();
    assert!(sources.contains("delegate:Wien"));
    assert!(sources.contains(""));

    let sub_finals: Vec<&AgentEvent> = seen
        .iter()
        .filter(|e| e.source == "delegate:Wien" && e.etype == FINAL)
        .collect();
    assert!(!sub_finals.is_empty());
    assert_eq!(
        sub_finals[0].data,
        EventData::Final("Steckbrief Wien".into())
    );

    let tool_results: Vec<&AgentEvent> = seen
        .iter()
        .filter(|e| e.source.is_empty() && e.etype == TOOL_RESULT)
        .collect();
    assert_eq!(
        tool_results[0].data,
        EventData::ToolResult {
            name: "delegate".into(),
            result: "Steckbrief Wien".into()
        }
    );

    assert!(seen
        .iter()
        .any(|e| e.etype == DONE && e.source == "delegate:Wien"));
    let last = seen.last().unwrap();
    assert_eq!(last.etype, DONE);
    assert_eq!(last.source, "");
    assert_eq!(final_answer, "Tabelle fertig");
}

// ----------------------------------------------- Coding: glob_files & grep
#[test]
fn coding_glob_and_grep() {
    let dir = std::env::temp_dir().join(format!("agentkit_glob_{}", std::process::id()));
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    ct.write_file("a.py", "import os\nprint(1)\n").unwrap();
    ct.write_file("src/b.py", "x = 1\n").unwrap();
    ct.write_file("src/c.txt", "hallo\n").unwrap();
    ct.write_file(".git/config", "geheim import\n").unwrap(); // Ignore-Ordner

    // **/*.py findet rekursiv, überspringt .git und c.txt.
    let py = ct.glob_files("**/*.py", ".", 200).unwrap();
    assert!(py.contains("a.py") && py.contains("src/b.py"), "war: {py}");
    assert!(!py.contains("c.txt") && !py.contains("config"));

    // *.py nur auf oberster Ebene.
    let top = ct.glob_files("*.py", ".", 200).unwrap();
    assert!(
        top.contains("a.py") && !top.contains("src/b.py"),
        "war: {top}"
    );

    // grep liefert pfad:zeile: text und respektiert den Ignore-Ordner.
    let hits = ct.grep("import", ".", "**/*", 200).unwrap();
    assert!(hits.contains("a.py:1: import os"), "war: {hits}");
    assert!(!hits.contains("config")); // .git wird übersprungen

    // Ungültiges Regex -> klare Fehlermeldung (kein Panic).
    assert!(ct
        .grep("(", ".", "**/*", 200)
        .unwrap()
        .contains("ungültiges Regex"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn coding_register_only_readonly() {
    let dir = std::env::temp_dir().join(format!("agentkit_ro_{}", std::process::id()));
    let mut reg = ToolRegistry::new();
    CodingTools::new(dir.to_str().unwrap(), false)
        .register(&mut reg, Some(agentkit::READ_ONLY_TOOLS));
    for t in ["list_files", "glob_files", "grep", "read_file"] {
        assert!(reg.has(t), "read-only-Tool fehlt: {t}");
    }
    for t in ["write_file", "edit_file", "run_shell"] {
        assert!(!reg.has(t), "schreibendes Tool sollte fehlen: {t}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------- Rollen / task-Tool
#[test]
fn body_after_frontmatter_splits_correctly() {
    let t = "---\nname: x\ndescription: d\n---\nDer Body.\nZeile 2.";
    assert_eq!(
        agentkit::body_after_frontmatter(t).trim(),
        "Der Body.\nZeile 2."
    );
    assert_eq!(agentkit::body_after_frontmatter("kein fm"), "kein fm");
}

#[test]
fn load_roles_from_dir_parses_markdown() {
    let dir = std::env::temp_dir().join(format!("agentkit_roles_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("security.md"),
        "---\nname: security\ndescription: Sec review\ntools: read_only\nstrategy: plain\n---\nDu bist Security-Reviewer.",
    )
    .unwrap();
    let roles = agentkit::load_roles_from_dir(dir.to_str().unwrap());
    assert_eq!(roles.len(), 1);
    let r = &roles[0];
    assert_eq!(r.name, "security");
    assert_eq!(r.description, "Sec review");
    assert!(r.system.contains("Security-Reviewer"));
    // `tools: read_only` -> die READ_ONLY_TOOLS-Teilmenge (robust gegen deren Umfang).
    assert_eq!(
        r.tools.as_ref().unwrap().len(),
        agentkit::READ_ONLY_TOOLS.len()
    );
    assert!(r.tools.as_ref().unwrap().contains(&"read_file".to_string()));
    assert_eq!(r.strategy, Strategy::Plain);
    std::fs::remove_dir_all(&dir).ok();
}

// Die Team-Rollen des Coding-Swarm-Beispiels (examples/coding_swarm) müssen als
// Custom-Rollen laden und die Builtins korrekt überschreiben — das hält die
// eingecheckten .md-Dateien und den Rollen-Parser zusammen ehrlich. (CWD im
// Test = Crate-Wurzel, wie beim read_pdf-Fixture-Test.)
#[test]
fn coding_swarm_example_roles_load() {
    let roles = agentkit::load_roles_from_dir("examples/coding_swarm/roles");
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["architect", "developer", "reviewer", "tester"]);

    let get = |n: &str| roles.iter().find(|r| r.name == n).unwrap();
    // architect/reviewer sind read-only, developer hat vollen Zugriff (tools fehlt).
    assert_eq!(
        get("architect").tools.as_ref().unwrap().len(),
        agentkit::READ_ONLY_TOOLS.len()
    );
    assert_eq!(
        get("reviewer").tools.as_ref().unwrap().len(),
        agentkit::READ_ONLY_TOOLS.len()
    );
    assert!(get("developer").tools.is_none());
    // tester darf ausführen und den Diff sehen, aber nicht schreiben.
    let tester = get("tester").tools.as_ref().unwrap();
    assert!(tester.contains(&"run_shell".to_string()));
    assert!(tester.contains(&"git_diff".to_string()));
    assert!(
        !tester.contains(&"write_file".to_string()) && !tester.contains(&"edit_file".to_string())
    );
    assert_eq!(get("architect").strategy, Strategy::Plan);

    // Gemergt über die Builtins: tester/reviewer ÜBERSCHREIBEN, architect/developer
    // kommen dazu — es bleibt bei genau einer Rolle pro Name.
    let merged = agentkit::merge_roles(agentkit::builtin_roles(), roles);
    assert_eq!(merged.iter().filter(|r| r.name == "tester").count(), 1);
    assert!(merged
        .iter()
        .find(|r| r.name == "tester")
        .unwrap()
        .system
        .contains("Tester eines Software-Teams"));
    assert!(merged.iter().any(|r| r.name == "architect"));
    assert!(merged.iter().any(|r| r.name == "explorer"));
}

#[test]
fn task_tool_registers_and_runs_subagent() {
    let dir = std::env::temp_dir().join(format!("agentkit_task_{}", std::process::id()));
    let run = agentkit::RunHandle::new();
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("SUBERGEBNIS")]]));
    let mut reg = ToolRegistry::new();
    agentkit::add_task_tool(
        &mut reg,
        agentkit::TaskToolConfig {
            run,
            llm,
            coding: agentkit::coding::CodingTools::new(dir.to_str().unwrap(), false),
            roles: agentkit::builtin_roles(),
            mcp: std::sync::Arc::new(agentkit::McpHub::empty()),
            dry_run: false,
            helper_ctx_budget: None,
        },
    );
    assert!(reg.has("task"));

    // Schema: subagent_type-Enum enthält die Rollen, 'general' steht zuletzt.
    let schemas = reg.schemas().unwrap();
    let task = schemas
        .iter()
        .find(|s| s["function"]["name"] == "task")
        .unwrap();
    let en = task["function"]["parameters"]["properties"]["subagent_type"]["enum"]
        .as_array()
        .unwrap();
    let kinds: Vec<&str> = en.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        kinds.contains(&"explorer") && kinds.contains(&"reviewer") && kinds.contains(&"tester")
    );
    assert_eq!(kinds.last(), Some(&"general"));

    // Ohne Bus -> Sub-Agent läuft und liefert seine finale Antwort zurück.
    let out = reg
        .call(
            "task",
            json!({"prompt":"erkunde", "subagent_type":"explorer"}),
        )
        .unwrap();
    assert_eq!(out, "SUBERGEBNIS");

    // Fehlender prompt -> klarer Fehlertext.
    assert!(reg.call("task", json!({})).unwrap().contains("'prompt'"));
    std::fs::remove_dir_all(&dir).ok();
}

/// `--dry-run` muss über die Delegationsgrenze halten: früher bekam der Sub-Agent
/// eine ungefilterte Registry und schrieb, was der Orchestrator selbst nicht durfte.
#[test]
fn task_tool_propagates_dry_run_to_subagent() {
    let dir = std::env::temp_dir().join(format!("agentkit_task_dry_{}", std::process::id()));
    // Sub-Agent: erst write_file, dann finale Antwort.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "w1",
            "write_file",
            r#"{"path":"neu.txt","content":"x"}"#,
        )],
        vec![Chunk::text("fertig")],
    ]));
    let mut reg = ToolRegistry::new();
    agentkit::add_task_tool(
        &mut reg,
        agentkit::TaskToolConfig {
            run: agentkit::RunHandle::new(),
            llm,
            coding: agentkit::coding::CodingTools::new(dir.to_str().unwrap(), false),
            roles: agentkit::builtin_roles(),
            mcp: std::sync::Arc::new(agentkit::McpHub::empty()),
            dry_run: true,
            helper_ctx_budget: None,
        },
    );
    reg.call(
        "task",
        json!({"prompt":"schreib was","subagent_type":"general"}),
    )
    .unwrap();
    assert!(
        !dir.join("neu.txt").exists(),
        "Sub-Agent hat unter --dry-run geschrieben"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// PDF-Textextraktion (`read_pdf`-Tool) — nur mit Feature `pdf`. Nutzt die für den
// Accounts-Payable-Demo committete Beispielrechnung als Fixture (kein Netz, keine Tokens).
#[cfg(feature = "pdf")]
#[test]
fn read_pdf_extracts_invoice_text() {
    use agentkit::coding::CodingTools;
    // CWD im Test = Crate-Wurzel; Sandbox auf den Demo-Ordner setzen.
    let tools = CodingTools::new("examples/accounts_payable/inbox", false);
    let text = tools.read_pdf("rechnung_sauber.pdf").expect("read_pdf");
    // Umlaute/€ korrekt dekodiert (WinAnsi) und Kernfelder vorhanden.
    assert!(text.contains("München"), "Umlaut fehlt: {text}");
    assert!(text.contains("2025-0042"), "Rechnungsnummer fehlt: {text}");
    assert!(text.contains("1.892,10"), "Bruttobetrag fehlt: {text}");
    assert!(text.contains('€'), "Euro-Zeichen fehlt: {text}");
}

// Human-in-the-Loop OHNE Sonderwerkzeug: Der Agent stellt eine Rückfrage, indem er seinen Zug
// beendet; die Antwort des Menschen kommt als nächste Nachricht, und er macht mit vollem
// Gesprächsverlauf weiter. (Ersetzt das frühere `ask_user`-Werkzeug — die agentische Schleife
// kann das nativ, weil die Kurzzeit-Memory über die Züge erhalten bleibt.)
#[test]
fn interactive_followup_question_continues_with_history() {
    use agentkit::{build_coding_agent, ApproveFn, CodingAgentConfig, McpHub};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("agentkit_followup_{}", std::process::id()));
    // Zug 1: Rückfrage (kein Tool-Call -> der Zug endet). Zug 2: finale Antwort.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::text("Welche Kostenstelle für Lieferant X?")],
        vec![Chunk::text("Erledigt: gebucht auf KST-4200.")],
    ]));
    let cfg = CodingAgentConfig {
        workspace: dir.to_str().unwrap(),
        strategy: Strategy::Plain,
        max_steps: 5,
        skills: None,
        agents: None,
        memory: None,
        subagents: false,
        system: None,
        verify: false,
        shell_timeout: 120,
        dry_run: false,
        extra_tools: None,
        helper_ctx_budget: None,
    };
    let approve: ApproveFn = Arc::new(|_| true);
    let (mut agent, _p, _s, _r, _b, _c) =
        build_coding_agent(llm, &cfg, approve, Arc::new(McpHub::empty()));

    // Kein Sonderwerkzeug mehr für Rückfragen.
    assert!(!agent.tools.has("ask_user"));

    // Zug 1: Der Agent fragt zurück und beendet den Zug.
    let question = agent.run("Verbuche die Rechnung von Lieferant X.");
    assert!(
        question.contains("Kostenstelle"),
        "erwartete Rückfrage, bekam: {question}"
    );

    // Zug 2: Die Antwort des Menschen als nächste Nachricht — der Agent macht weiter.
    let done = agent.run("Kostenstelle KST-4200 (Marketing).");
    assert!(
        done.contains("KST-4200"),
        "erwartete Fortsetzung, bekam: {done}"
    );

    // Der Verlauf trägt beide Züge: die erste Aufgabe UND die spätere Antwort — Kontext bleibt.
    let convo = serde_json::to_string(&agent.memory.messages).unwrap();
    assert!(
        convo.contains("Lieferant X") && convo.contains("KST-4200"),
        "Gesprächsverlauf unvollständig: {convo}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------- Benutzer-Config (~/.agentkit)

/// Die frische Vorlage, die das Setup-Skript schreibt, darf keinen Anbieter
/// *aktivieren*: ihre Zugangsdaten sind Platzhalter (`<…>`) und müssen übersprungen
/// werden. Sonst bekäme der Anwender statt des Demo-Fallbacks ein 401 vom Endpunkt.
/// Unkritische Defaults (`api_version`, `model`) dürfen dagegen durchgereicht werden.
#[test]
fn config_template_activates_no_provider_until_filled_in() {
    let cfg: Value = serde_json::from_str(agentkit::CONFIG_TEMPLATE).unwrap();
    let pairs = agentkit::config_env_pairs(&cfg);
    for (k, v) in &pairs {
        assert!(
            !matches!(
                k.as_str(),
                "AZURE_OPENAI_API_KEY"
                    | "AZURE_OPENAI_ENDPOINT"
                    | "AZURE_OPENAI_DEPLOYMENT"
                    | "OPENAI_API_KEY"
                    | "OPENAI_BASE_URL"
            ),
            "Platzhalter aus der Vorlage wurde gesetzt: {k}={v}"
        );
    }
}

/// Lokaler OpenAI-kompatibler Server via Config: `openai.base_url` wird auf
/// `OPENAI_BASE_URL` abgebildet — ohne API-Key (leer bleibt ungesetzt), denn
/// Ollama & Co. verlangen keinen.
#[test]
fn config_maps_local_base_url_to_env() {
    let cfg = json!({
        "openai": { "api_key": "", "model": "llama3.1", "base_url": "http://localhost:11434/v1" }
    });
    let pairs = agentkit::config_env_pairs(&cfg);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("OPENAI_BASE_URL"), Some("http://localhost:11434/v1"));
    assert_eq!(get("OPENAI_MODEL"), Some("llama3.1"));
    assert_eq!(get("OPENAI_API_KEY"), None);
}

/// Ausgefüllte Config -> `AZURE_OPENAI_*`; `provider` -> `AGENTKIT_PROVIDER`; der freie
/// `env`-Block wird durchgereicht. Leere Felder (hier `openai.api_key`) bleiben ungesetzt.
#[test]
fn config_maps_azure_values_to_env() {
    let cfg = json!({
        "provider": "azure",
        "azure": {
            "endpoint": "https://demo.openai.azure.com",
            "api_key": "geheim",
            "deployment": "gpt-4o",
            "api_version": "2024-10-21"
        },
        "openai": { "api_key": "", "model": "gpt-4o-mini" },
        "env": { "HTTPS_PROXY": "http://proxy:8080" }
    });
    let pairs = agentkit::config_env_pairs(&cfg);
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(
        get("AZURE_OPENAI_ENDPOINT"),
        Some("https://demo.openai.azure.com")
    );
    assert_eq!(get("AZURE_OPENAI_API_KEY"), Some("geheim"));
    assert_eq!(get("AZURE_OPENAI_DEPLOYMENT"), Some("gpt-4o"));
    assert_eq!(get("AZURE_OPENAI_API_VERSION"), Some("2024-10-21"));
    assert_eq!(get("AGENTKIT_PROVIDER"), Some("azure"));
    assert_eq!(get("HTTPS_PROXY"), Some("http://proxy:8080"));
    // Leeres Feld -> gar nicht erst gesetzt (sonst bräche der Azure-Pfad).
    assert_eq!(get("OPENAI_API_KEY"), None);
    // `model` steht aber drin — der OpenAI-Pfad braucht nur den Key zusätzlich.
    assert_eq!(get("OPENAI_MODEL"), Some("gpt-4o-mini"));
}

// ------------------------------------------- Robustheit: Stream-Retry mit Backoff

/// Schlägt die ersten `fails` Stream-Aufrufe fehl (transienter Fehler), danach
/// delegiert es an ein FakeLlm — testet den Retry-Pfad des Harness.
struct FlakyLlm {
    fails: std::sync::atomic::AtomicUsize,
    inner: FakeLlm,
}

impl agentkit::Llm for FlakyLlm {
    fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
    ) -> Result<agentkit::Message, String> {
        self.inner.complete(messages, tools)
    }

    fn stream(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
    ) -> Result<agentkit::llm::ChunkStream, String> {
        use std::sync::atomic::Ordering;
        if self.fails.load(Ordering::SeqCst) > 0 {
            self.fails.fetch_sub(1, Ordering::SeqCst);
            return Err("HTTP 429 (Rate-Limit): zu viele Anfragen".to_string());
        }
        self.inner.stream(messages, tools)
    }
}

/// Ein LLM, das immer denselben Fehler liefert und seine Aufrufe zählt.
struct RateLimitedLlm {
    error: String,
    calls: std::sync::atomic::AtomicUsize,
}

impl agentkit::Llm for RateLimitedLlm {
    fn complete(&self, _m: &[Value], _t: Option<&[Value]>) -> Result<agentkit::Message, String> {
        Err(self.error.clone())
    }

    fn stream(
        &self,
        _m: &[Value],
        _t: Option<&[Value]>,
    ) -> Result<agentkit::llm::ChunkStream, String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(self.error.clone())
    }
}

/// Ein LLM, das die ersten `fails` Versuche mit `error` abweist.
struct RetryAfterLlm {
    fails: std::sync::atomic::AtomicUsize,
    error: String,
    inner: FakeLlm,
}

impl agentkit::Llm for RetryAfterLlm {
    fn complete(&self, m: &[Value], t: Option<&[Value]>) -> Result<agentkit::Message, String> {
        self.inner.complete(m, t)
    }

    fn stream(
        &self,
        m: &[Value],
        t: Option<&[Value]>,
    ) -> Result<agentkit::llm::ChunkStream, String> {
        use std::sync::atomic::Ordering;
        if self.fails.load(Ordering::SeqCst) > 0 {
            self.fails.fetch_sub(1, Ordering::SeqCst);
            return Err(self.error.clone());
        }
        self.inner.stream(m, t)
    }
}

/// Das genannte Fenster bestimmt die Wartezeit, nicht der (viel kürzere)
/// exponentielle Backoff — sonst landet der zweite Versuch im selben Limit.
/// Absichtlich nur 300 ms: die Sperre ist prozessweit, ein längeres Fenster
/// hier würde die übrigen Tests ausbremsen.
#[test]
fn retry_after_bestimmt_die_wartezeit() {
    let llm = Arc::new(RetryAfterLlm {
        fails: std::sync::atomic::AtomicUsize::new(1),
        error: "HTTP 429 (Rate-Limit), Retry-After: 0.3s: rate_limit_exceeded".to_string(),
        inner: FakeLlm::new(vec![vec![Chunk::text("Antwort nach dem Warten.")]]),
    });
    let mut agent = Agent::builder(llm)
        .tools(ToolRegistry::new())
        .retry_backoff_ms(1) // eigener Backoff wäre 1 ms — das Fenster gewinnt
        .build();

    let start = std::time::Instant::now();
    assert_eq!(agent.run("hi"), "Antwort nach dem Warten.");
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(250),
        "hat das Retry-After nicht abgewartet: {:?}",
        start.elapsed()
    );
}

/// Nennt der Provider ein Fenster jenseits der Obergrenze, sind weitere Versuche
/// garantiert vergeblich — der Loop bricht nach EINEM Versuch ab, statt zwei
/// weitere Anfragen gegen dasselbe Rate-Limit zu schicken. Der Test belegt
/// zugleich, dass `Retry-After` überhaupt im Retry-Pfad ankommt und nicht bloß
/// in der Fehlermeldung steht.
#[test]
fn retry_after_jenseits_der_obergrenze_bricht_sofort_ab() {
    let llm = Arc::new(RateLimitedLlm {
        error: "HTTP 429 (Rate-Limit), Retry-After: 300s: rate_limit_exceeded".to_string(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut agent = Agent::builder(llm.clone())
        .tools(ToolRegistry::new())
        // Backoff aktiv — sonst greift der „gar nicht warten"-Kurzschluss.
        .retry_backoff_ms(500)
        .build();

    let start = std::time::Instant::now();
    assert_eq!(agent.run("hi"), "(keine Antwort)");
    assert_eq!(
        llm.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "es hätte nur EIN Versuch sein dürfen"
    );
    // Ohne die Auswertung wären es 3 Versuche mit 500 ms + 1000 ms Backoff.
    assert!(
        start.elapsed() < std::time::Duration::from_millis(400),
        "hat trotzdem gewartet: {:?}",
        start.elapsed()
    );
}

#[test]
fn stream_retry_recovers_after_transient_failures() {
    // 2 Fehlversuche, der 3. Versuch (innerhalb desselben Schritts) liefert.
    let llm = Arc::new(FlakyLlm {
        fails: std::sync::atomic::AtomicUsize::new(2),
        inner: FakeLlm::new(vec![vec![Chunk::text("Antwort trotz Rate-Limit.")]]),
    });
    let mut agent = Agent::builder(llm)
        .tools(ToolRegistry::new())
        .retry_backoff_ms(0) // Tests warten nicht
        .build();
    assert_eq!(agent.run("hi"), "Antwort trotz Rate-Limit.");
}

#[test]
fn stream_retry_gives_up_after_three_failures() {
    let llm = Arc::new(FlakyLlm {
        fails: std::sync::atomic::AtomicUsize::new(99),
        inner: FakeLlm::new(vec![]),
    });
    let mut agent = Agent::builder(llm)
        .tools(ToolRegistry::new())
        .retry_backoff_ms(0)
        .build();
    let mut saw_error = false;
    let out = agent.run_with_events("hi", None, |ev| {
        if let EventData::Error { error, .. } = &ev.data {
            saw_error = true;
            assert!(error.contains("429"), "war: {error}");
        }
    });
    assert_eq!(out, "(keine Antwort)");
    assert!(saw_error, "ERROR-Event mit HTTP-Status erwartet");
}

// ------------------------------------------------ verify_before_final (Selbstcheck)

fn verify_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.add(
        "write_file",
        "Schreibt eine Datei.",
        json!({"type":"object","properties":{"path":{"type":"string"}}}),
        |_| Ok("geschrieben".to_string()),
    );
    reg.add(
        "run_shell",
        "Führt einen Befehl aus.",
        json!({"type":"object","properties":{"command":{"type":"string"}}}),
        |_| Ok("Tests grün".to_string()),
    );
    reg
}

#[test]
fn verify_nudge_blocks_unverified_finish() {
    // Zug 1: write_file. Zug 2: will fertig sein (kein Check) -> Nudge statt Ende.
    // Zug 3: run_shell (Check). Zug 4: finale Antwort geht durch.
    let turns = vec![
        vec![Chunk::tool(0, "c1", "write_file", "{\"path\":\"a.txt\"}")],
        vec![Chunk::text("Fertig, alles erledigt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "run_shell",
            "{\"command\":\"pytest\"}",
        )],
        vec![Chunk::text("Fertig — Check ausgeführt.")],
    ];
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(verify_registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .build();
    assert_eq!(agent.run("ändere a.txt"), "Fertig — Check ausgeführt.");
    // Der Nudge steht genau einmal als User-Nachricht in der Historie.
    let nudges = agent
        .memory
        .messages
        .iter()
        .filter(|m| {
            m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.contains("Halt:"))
        })
        .count();
    assert_eq!(nudges, 1);
}

#[test]
fn verify_nudge_not_repeated_and_skipped_after_check() {
    // Nach write_file + run_shell im selben Lauf ist der Abschluss sofort erlaubt.
    let turns = vec![
        vec![Chunk::tool(0, "c1", "write_file", "{\"path\":\"a.txt\"}")],
        vec![Chunk::tool(
            0,
            "c2",
            "run_shell",
            "{\"command\":\"pytest\"}",
        )],
        vec![Chunk::text("Fertig nach Check.")],
    ];
    let llm = Arc::new(FakeLlm::new(turns));
    let mut agent = Agent::builder(llm.clone())
        .tools(verify_registry())
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .build();
    assert_eq!(agent.run("ändere a.txt"), "Fertig nach Check.");
    assert_eq!(llm.calls(), 3, "kein Nudge-Extra-Turn erwartet");
}

#[test]
fn verify_disabled_keeps_old_behaviour() {
    // Default (aus): Abschluss direkt nach write_file ohne Check.
    let turns = vec![
        vec![Chunk::tool(0, "c1", "write_file", "{\"path\":\"a.txt\"}")],
        vec![Chunk::text("Fertig ohne Check.")],
    ];
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(verify_registry())
        .strategy(Strategy::Plain)
        .build();
    assert_eq!(agent.run("ändere a.txt"), "Fertig ohne Check.");
}

// -------------------------------------- Memory: Token-Zählung + Session-Persistenz

#[test]
fn memory_tokens_count_tool_call_arguments() {
    let mut mem = ShortTermMemory::new(None);
    mem.add(json!({"role": "assistant", "content": "",
        "tool_calls": [{"id": "c1", "type": "function",
            "function": {"name": "write_file", "arguments": "x".repeat(4000)}}]}));
    // Ohne tool_calls-Zählung wäre das 0 — der Blindfleck aus großen write_file-Argumenten.
    assert!(mem.tokens() > 900, "war: {}", mem.tokens());
}

#[test]
fn memory_save_load_roundtrip() {
    let path = std::env::temp_dir().join(format!("agentkit_sess_{}.json", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let mut mem = ShortTermMemory::new(Some("System-Prompt"));
    mem.add_user("Erste Frage");
    mem.add(json!({"role": "assistant", "content": "Erste Antwort"}));
    mem.save(&path).unwrap();

    let loaded = ShortTermMemory::load(&path).unwrap();
    assert_eq!(loaded.messages, mem.messages);

    // Fehlende Datei ist KEIN Fehler, sondern eine frische Session.
    let fresh = ShortTermMemory::load("/nirgendwo/gibt/es/das.json").unwrap();
    assert!(fresh.messages.is_empty());
    std::fs::remove_file(&path).ok();
}

// ------------------------------------------------------- git-Tools (read-only)

#[test]
fn git_tools_read_repo_and_reject_option_injection() {
    let dir = std::env::temp_dir().join(format!("agentkit_git_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    ct.write_file("f.txt", "hallo\n").unwrap();

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .expect("git ausführbar")
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);

    assert!(ct.git_status("").unwrap().contains("main"));
    assert!(ct.git_log("", 10, "").unwrap().contains("init"));
    assert!(ct.git_show("", "HEAD").unwrap().contains("hallo"));

    // Unkommittierte Änderung erscheint im Diff.
    ct.write_file("f.txt", "hallo\nneu\n").unwrap();
    let diff = ct.git_diff("", "", "", false).unwrap();
    assert!(diff.contains("+neu"), "war: {diff}");

    // Options-Injection über Ref/Pfad wird als weicher Fehler abgelehnt.
    assert!(ct
        .git_diff("", "--output=/tmp/x", "", false)
        .unwrap()
        .starts_with("ERROR:"));
    assert!(ct.git_show("", "--help").unwrap().starts_with("ERROR:"));

    // Read-only-Rollen bekommen die git-Tools über READ_ONLY_TOOLS.
    let mut reg = ToolRegistry::new();
    ct.register(&mut reg, Some(agentkit::READ_ONLY_TOOLS));
    for t in ["git_status", "git_diff", "git_log", "git_show"] {
        assert!(reg.has(t), "git-Tool fehlt in READ_ONLY_TOOLS: {t}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn git_tools_outside_repo_are_soft_errors() {
    let dir = std::env::temp_dir().join(format!("agentkit_nogit_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    // Kein Repo -> ERROR-Ergebnis (Modell korrigiert sich), kein harter Fehler.
    assert!(ct.git_status("").unwrap().starts_with("ERROR:"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn git_tools_work_in_subdirectory_repo() {
    // Repo liegt in einem Unterordner des Workspace (wie /app/repo in Containern):
    // der 'dir'-Parameter macht die git-Tools dort nutzbar; Sandbox bleibt bindend.
    let dir = std::env::temp_dir().join(format!("agentkit_gitsub_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    ct.write_file("repo/f.txt", "hallo\n").unwrap();
    let repo = dir.join("repo");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .expect("git ausführbar")
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);

    // Ohne dir: Workspace-Root ist kein Repo -> weicher Fehler. Mit dir: klappt.
    assert!(ct.git_status("").unwrap().starts_with("ERROR:"));
    assert!(ct.git_status("repo").unwrap().contains("main"));
    assert!(ct.git_log("repo", 10, "").unwrap().contains("init"));
    // Sandbox-Ausbruch über dir bleibt ein weicher Fehler.
    assert!(ct.git_status("../..").unwrap().starts_with("ERROR:"));
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------- Kontext-Report

/// `/context`-Datenbasis ohne ctxman: der Report gruppiert die
/// `ShortTermMemory`-Spiegelung nach Abschnitten, zählt die Tool-Schemas mit
/// und trägt das Builder-Budget.
#[test]
fn context_report_zaehlt_abschnitte_und_summe() {
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "c1", "echo", "{\"text\":\"hi\"}")],
        vec![Chunk::text("Fertig.")],
    ]));
    let mut tools = ToolRegistry::new();
    tools.add(
        "echo",
        "Gibt den Text zurück.",
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        |args: Value| Ok(args["text"].as_str().unwrap_or("").to_string()),
    );
    let mut agent = Agent::builder(llm)
        .tools(tools)
        .system("Du bist ein Test-Agent.")
        .retry_backoff_ms(0)
        .build();
    agent.run("Sag hi per echo.");

    let report = agentkit::context_report(&agent);
    assert!(!report.managed);
    assert_eq!(report.budget, 8000); // Builder-Default
    let get = |label: &str| report.segments.iter().find(|s| s.label == label);
    let sys = get("System-Prompt").expect("System-Prompt fehlt");
    assert_eq!(sys.count, 1);
    assert!(sys.tokens > 0);
    let schemas = get("Tool-Schemas").expect("Tool-Schemas fehlen");
    assert_eq!(schemas.count, 1);
    assert!(schemas.tokens > 0);
    assert_eq!(get("User-Nachrichten").unwrap().count, 1);
    assert_eq!(get("Tool-Aufrufe").unwrap().count, 1);
    assert_eq!(get("Tool-Ergebnisse").unwrap().count, 1);
    assert_eq!(get("Assistant-Antworten").unwrap().count, 1);
    // Leere Abschnitte tauchen nicht auf, die Summe passt zur Abschnittsliste.
    assert!(get("Verlaufs-Zusammenfassung").is_none());
    assert_eq!(
        report.total,
        report.segments.iter().map(|s| s.tokens).sum::<usize>()
    );
}

// Der Erweiterungspunkt für Frontends (`CodingAgentConfig::extra_tools`): das
// injizierte Tool muss im fertigen Agenten UND in der MCP-freien Basis-Registry
// liegen — sonst verschwindet es beim Neuverdrahten (REPL `/mcp`, TUI F2).
// Zusätzlich muss der Callback den Lauf-Kontext des fertigen Agenten sehen
// (derselbe `RunHandle`), sonst erreicht ein Tool den aktiven Bus nie.
#[test]
fn extra_tools_landen_in_agent_und_mcp_base() {
    use agentkit::{build_coding_agent, ApproveFn, CodingAgentConfig, McpHub, RunHandle};
    use std::sync::{Arc, Mutex};

    let dir = std::env::temp_dir().join(format!("agentkit_extra_{}", std::process::id()));
    // Der Callback merkt sich den übergebenen Lauf-Kontext zum Vergleich.
    let seen_run: Arc<Mutex<Option<RunHandle>>> = Arc::new(Mutex::new(None));
    let seen_roles = Arc::new(Mutex::new(Vec::<String>::new()));
    let extra = {
        let seen_run = seen_run.clone();
        let seen_roles = seen_roles.clone();
        Arc::new(
            move |reg: &mut agentkit::ToolRegistry, ctx: &agentkit::ExtraToolCtx| {
                *seen_run.lock().unwrap() = Some(ctx.run.clone());
                *seen_roles.lock().unwrap() = ctx.roles.iter().map(|r| r.name.clone()).collect();
                reg.add(
                    "mein_frontend_tool",
                    "Testwerkzeug des Frontends.",
                    serde_json::json!({"type": "object", "properties": {}}),
                    |_| Ok("ok".to_string()),
                );
            },
        )
    };

    let cfg = CodingAgentConfig {
        workspace: dir.to_str().unwrap(),
        strategy: Strategy::Plain,
        max_steps: 3,
        skills: None,
        agents: None,
        memory: None,
        subagents: true,
        system: None,
        verify: false,
        shell_timeout: 120,
        dry_run: false,
        extra_tools: Some(extra),
        helper_ctx_budget: None,
    };
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("fertig")]]));
    let approve: ApproveFn = Arc::new(|_| true);
    let (agent, _p, _s, _r, mcp_base, _c) =
        build_coding_agent(llm, &cfg, approve, Arc::new(McpHub::empty()));

    assert!(agent.tools.has("mein_frontend_tool"));
    assert!(
        mcp_base.has("mein_frontend_tool"),
        "Tool fehlt in der MCP-freien Basis — ein /mcp-Toggle würde es verlieren"
    );
    // Der Callback sieht die aktiven Rollen (für rollenbasierte Frontend-Tools).
    assert!(seen_roles.lock().unwrap().iter().any(|r| r == "explorer"));
    // Und denselben Lauf-Kontext wie der fertige Agent: derselbe Bus zur Laufzeit.
    let ctx_run = seen_run.lock().unwrap().clone().expect("kein RunHandle");
    assert!(ctx_run.bus().is_none() && agent.run_handle().bus().is_none());
    let bus = agentkit::EventBus::new();
    let rx = bus.subscribe();
    let mut agent = agent;
    agent.run_on_bus("los", &bus, 7, None, "");
    // Während des Laufs war der Bus gesetzt; danach bleibt er stehen — der
    // Callback-Handle zeigt dieselbe Zelle, also auch dasselbe Ergebnis.
    assert!(ctx_run.bus().is_some());
    // `try_iter`, nicht `iter`: der Bus lebt im RunHandle weiter, ein blockierendes
    // `iter()` würde nie enden.
    assert!(rx.try_iter().count() > 0);

    std::fs::remove_dir_all(&dir).ok();
}

// --------------------------------------------- ctxman (nur mit Feature `ctxman`)

#[cfg(feature = "ctxman")]
mod ctxman_integration {
    use super::*;
    use agentkit::{ManagedContext, ManagedContextConfig};

    /// Ende-zu-Ende: eine VERWAISTE Unit wird geheilt, und die gerenderten
    /// Nachrichten erfüllen danach die Ordnungszusicherung von OpenAI.
    ///
    /// Das ist der Fehler, der 10 von 64 Polyglot-Tasks getötet hat. Der
    /// Agent bricht mitten im Lauf ab (oder ein Ergebnis wird evicted), die
    /// Unit ist unvollständig, `messages()` hängt ein Platzhalter-Ergebnis an
    /// — mit der HÖCHSTEN seq. Die Antwort stand danach hinter allem anderen,
    /// und der Provider lehnte den ganzen Request ab:
    ///
    ///   HTTP 400: An assistant message with 'tool_calls' must be followed by
    ///             tool messages responding to each 'tool_call_id'
    #[test]
    fn geheilte_unit_erfuellt_die_openai_ordnung() {
        let dir = std::env::temp_dir().join(format!("agentkit_heal_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let llm = Arc::new(FakeLlm::new(vec![]));
        let ctx = ManagedContext::new(ManagedContextConfig::new(dir.clone()), llm).unwrap();

        ctx.add_user("mach was");
        // Ein Aufruf OHNE Ergebnis — die verwaiste Unit.
        ctx.add_assistant(
            None,
            &[json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "run_shell", "arguments": "{}"}
            })],
        );
        // Danach geht der Verkehr weiter, das Ergebnis fehlt weiterhin.
        ctx.add_user("und weiter");

        let messages = ctx.messages().expect("Render muss gelingen");
        for (i, m) in messages.iter().enumerate() {
            let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) else {
                continue;
            };
            let folgend: Vec<&str> = messages[i + 1..]
                .iter()
                .take_while(|n| n["role"] == "tool")
                .filter_map(|n| n["tool_call_id"].as_str())
                .collect();
            for c in calls {
                let id = c["id"].as_str().unwrap_or("");
                assert!(
                    folgend.contains(&id),
                    "auf die tool_calls-Nachricht {i} folgt keine Antwort für '{id}';                      Rollen danach: {:?}",
                    messages[i + 1..]
                        .iter()
                        .map(|n| n["role"].as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Prüft die Lücke, auf die sich der `/rewind`-Hinweis stützt: startet man
    /// mit einer `--session`-Datei UND einem frischen `--ctx`-Verzeichnis,
    /// muss der geladene Verlauf auch im ctxman-Kontext landen — sonst begänne
    /// das Modell bei null, obwohl der Spiegel den Verlauf trägt.
    #[test]
    fn frischer_ctx_uebernimmt_geladenen_verlauf() {
        let dir = std::env::temp_dir().join(format!("agentkit_replay_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("ok")]]));
        let ctx = ManagedContext::new(ManagedContextConfig::new(dir.clone()), llm.clone()).unwrap();

        // Ein "geladener" Verlauf, wie ihn load_session in memory legt.
        let mut geladen = ShortTermMemory::new(Some("Testsystem"));
        geladen.add_user("frueher gefragt");
        geladen.add(agentkit::to_assistant_dict(
            Some("frueher geantwortet"),
            &[],
        ));

        let mut agent = Agent::builder(llm.clone())
            .system("Testsystem")
            .managed_context(ctx)
            .retry_backoff_ms(0)
            .build();
        agent.adopt_history(geladen);
        agent.run("und jetzt?");

        // Der erste Modell-Call muss den geladenen Verlauf enthalten.
        let seen = llm.seen_messages.lock().unwrap();
        let erster = &seen[0];
        let texte: String = erster
            .iter()
            .map(|m| m["content"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            texte.contains("frueher gefragt") && texte.contains("frueher geantwortet"),
            "geladener Verlauf fehlt im ctxman-Kontext: {texte}"
        );
        drop(seen);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Der volle Weg: kleiner Budget-Rahmen, ein riesiges Tool-Ergebnis wird
    /// externalisiert (Summary + Ref-Hinweis im Kontext), das Modell kann es per
    /// `expand_context_ref` zurückholen, und der Snapshot macht die Session
    /// prozessübergreifend fortsetzbar.
    #[test]
    fn managed_context_externalizes_expands_and_resumes() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctx_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![
            vec![Chunk::tool(0, "c1", "big", "{}")],
            vec![Chunk::text("fertig")],
        ]));
        let mut tools = ToolRegistry::new();
        tools.add(
            "big",
            "Liefert ein riesiges Ergebnis.",
            json!({"type":"object","properties":{},"required":[]}),
            |_args: Value| Ok("x".repeat(15_000)),
        );

        // Budget bewusst winzig: das 15k-Zeichen-Ergebnis reißt die Emergency-Schwelle,
        // der Minor GC externalisiert es in den Blob Store. Tokenizer auf die
        // Zeichen-Heuristik gepinnt, damit die Schwellen-Rechnung unabhängig vom
        // Feature `tiktoken` gilt (o200k zählt "xxxx…" drastisch kleiner).
        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.budget_tokens = 2_000;
        cfg.policy_overlay = Some(json!({"tokenizer": "heuristic"}));
        let ctx = ManagedContext::new(cfg, llm.clone()).unwrap();

        let mut agent = Agent::builder(llm.clone())
            .tools(tools)
            .system("Testsystem")
            .managed_context(ctx)
            .retry_backoff_ms(0)
            .build();
        assert!(agent.tools.has("expand_context_ref"));
        assert_eq!(agent.run("los"), "fertig");

        // Zweiter Modell-Call sah ctxman-gerenderte Messages: System-Prompt, User,
        // und statt des Roh-Ergebnisses den Externalisierungs-Hinweis.
        let seen = llm.seen_messages.lock().unwrap();
        let second = &seen[1];
        assert!(second.iter().any(|m| m["role"] == "system"));
        let tool_msg = second
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool-Message fehlt");
        let hint = tool_msg["content"].as_str().unwrap();
        assert!(
            hint.contains("expand_context_ref"),
            "Externalisierungs-Hinweis fehlt: {hint}"
        );

        // Page Fault: segment_id aus dem Hinweis ziehen und den Inhalt zurückholen.
        let sid = hint
            .split("segment_id=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("segment_id im Hinweis");
        let expanded = agent
            .tools
            .call("expand_context_ref", json!({ "segment_id": sid }))
            .unwrap();
        assert_eq!(expanded.len(), 15_000, "voller Inhalt erwartet");

        // Resume: frische Instanz aus demselben Verzeichnis kennt die Historie.
        drop(agent);
        let mut cfg2 = ManagedContextConfig::new(dir.clone());
        cfg2.budget_tokens = 2_000;
        let ctx2 = ManagedContext::new(cfg2, Arc::new(FakeLlm::new(vec![]))).unwrap();
        let msgs = ctx2.messages().unwrap();
        let all = serde_json::to_string(&msgs).unwrap();
        assert!(all.contains("los") && all.contains("fertig"), "war: {all}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--ctx-policy`-Overlay: partielle Angaben werden über die Default-Policy
    /// gemergt (Objekte feldweise — `externalize` von `tool_result` bleibt), die
    /// Metadaten sind ehrlich (`compaction.model` = tatsächliches Modell), und
    /// Tippfehler bzw. inkonsistente Watermarks sind harte Fehler.
    #[test]
    fn ctx_policy_overlay_wird_gemergt_und_validiert() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxpol_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.compaction_model_label = Some("gpt-5.4-mini".to_string());
        cfg.policy_overlay = Some(json!({
            "watermarks": {"soft": 0.5},
            "kinds": {"tool_result": {"ttl_turns": 7}, "notiz": {"ttl_turns": 2}},
        }));
        let ctx = ManagedContext::new(cfg, Arc::new(FakeLlm::new(vec![]))).unwrap();
        ctx.save().unwrap();

        let snapshot: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("snapshot.json")).unwrap())
                .unwrap();
        let policy = &snapshot["session"]["policy"];
        assert_eq!(policy["watermarks"]["soft"], 0.5);
        assert_eq!(policy["watermarks"]["hard"], 0.8); // nicht überschrieben
        assert_eq!(policy["kinds"]["tool_result"]["ttl_turns"], 7);
        assert_eq!(policy["kinds"]["tool_result"]["externalize"], true); // Merge, kein Ersetzen
        assert_eq!(policy["kinds"]["notiz"]["ttl_turns"], 2); // offenes Vokabular
        assert_eq!(policy["compaction"]["model"], "gpt-5.4-mini"); // ehrliches Metadatum
        assert_ne!(policy["tokenizer"], "claude"); // die irreführende Spec-Vorlage ist weg

        // Tippfehler und inkonsistente Watermarks sind harte Fehler.
        let err = |overlay: Value| {
            let mut cfg = ManagedContextConfig::new(dir.join("neu"));
            cfg.policy_overlay = Some(overlay);
            match ManagedContext::new(cfg, Arc::new(FakeLlm::new(vec![]))) {
                Err(e) => e,
                Ok(_) => panic!("Fehler erwartet"),
            }
        };
        assert!(err(json!({"watermark": {}})).contains("unbekanntes Feld"));
        assert!(err(json!({"watermarks": {"soft": 0.9}})).contains("Watermarks"));
        #[cfg(not(feature = "tiktoken"))]
        assert!(err(json!({"tokenizer": "o200k"})).contains("tiktoken"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Kind-Mapping: große `read_skill`-/`task`-Ergebnisse werden in ein kleines
    /// gepaartes `tool_result` plus ein eigenständiges `skill_content`-/`task`-
    /// Segment zerlegt — Pairing bleibt intakt (kein Heal-Platzhalter), und der
    /// `/context`-Report weist die neuen Abschnitte aus.
    #[test]
    fn ctx_kind_mapping_fuer_skill_und_subagent_ergebnisse() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxkind_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![
            vec![
                Chunk::tool(0, "c1", "read_skill", "{\"name\":\"deploy\"}"),
                Chunk::tool(1, "c2", "task", "{\"prompt\":\"recherchiere\"}"),
            ],
            vec![Chunk::text("fertig")],
        ]));
        let mut tools = ToolRegistry::new();
        tools.add(
            "read_skill",
            "Lädt einen Skill.",
            json!({"type":"object","properties":{},"required":[]}),
            |_args: Value| Ok(format!("SKILL-ANLEITUNG {}", "x".repeat(600))),
        );
        tools.add(
            "task",
            "Sub-Agent.",
            json!({"type":"object","properties":{},"required":[]}),
            |_args: Value| Ok(format!("SUB-AGENT-ERGEBNIS {}", "y".repeat(600))),
        );

        let ctx = ManagedContext::new(ManagedContextConfig::new(dir.clone()), llm.clone()).unwrap();
        let mut agent = Agent::builder(llm.clone())
            .tools(tools)
            .system("Testsystem")
            .managed_context(ctx)
            .retry_backoff_ms(0)
            .build();
        assert_eq!(agent.run("los"), "fertig");

        // Zweiter Modell-Call: gepaarte tool-Zeiger + eigenständige Inhalts-Segmente,
        // KEIN Heal-Platzhalter ("abgebrochen") — das Pairing war nie offen.
        let seen = llm.seen_messages.lock().unwrap();
        let second = serde_json::to_string(&seen[1]).unwrap();
        assert!(second.contains("skill_content") && second.contains("SKILL-ANLEITUNG"));
        assert!(second.contains("[task —") && second.contains("SUB-AGENT-ERGEBNIS"));
        assert!(
            !second.contains("abgebrochen"),
            "Pairing war offen: {second}"
        );

        let report = agentkit::context_report(&agent);
        let get = |label: &str| report.segments.iter().find(|s| s.label == label);
        assert!(get("Skill-Inhalte").is_some(), "skill_content fehlt");
        assert!(get("Sub-Agent-Ergebnisse").is_some(), "task fehlt");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `/compact` verdichtet auch bei aktivem ctxman auf Kommando — und meldet
    /// bei einem zu kurzen Verlauf ehrlich „nichts getan". Letzteres ist die
    /// eigentliche Falle: `run_major_gc` liefert bei einem No-op-Plan ebenfalls
    /// `Ok`, ein `.is_ok()` als Erfolgssignal löge also.
    #[test]
    fn ctx_compact_now_meldet_nur_echte_verdichtung() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxman_now_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![]));
        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.budget_tokens = 1_000;
        cfg.policy_overlay = Some(json!({"tokenizer": "heuristic"}));
        let ctx = ManagedContext::new(cfg, llm.clone()).unwrap();
        let mut agent = Agent::builder(llm)
            .system("Testsystem")
            .managed_context(ctx)
            .build();

        // Frischer Kontext: nichts zu holen — darf KEINEN Erfolg melden.
        assert!(
            !agent.compact_now(None),
            "leerer Kontext meldete eine Verdichtung"
        );

        // Genug Material fürs Compaction-Fenster (≥ 2 Units), dann greift es.
        for i in 0..6 {
            agent.memory.add_user(&format!("Nachricht {i}"));
            if let Some(c) = &agent.context {
                c.add_user(&format!("Nachricht {i}: {}", "z".repeat(800)));
            }
        }
        assert!(agent.compact_now(None), "Verdichtung blieb aus");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ein Helfer-Kontext darf NICHTS auf die Platte schreiben: `drive()` ruft
    /// `save()` nach jedem Lauf, und ein Sub-Agent hat kein Zustandsverzeichnis.
    /// Ohne den No-op-Zweig landete pro Helfer-Turn eine `snapshot.json` im
    /// aktuellen Verzeichnis — und parallele Helfer überschrieben sie gegenseitig.
    #[test]
    fn ephemerer_kontext_schreibt_nichts_und_verdichtet_trotzdem() {
        use agentkit::ManagedContext;

        let vorher: Vec<_> = std::fs::read_dir(".")
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();

        let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("verdichtet")]]));
        let ctx = ManagedContext::ephemeral(20_000, llm.clone()).expect("ephemeral");
        ctx.set_system("Testsystem").unwrap();
        for i in 0..6 {
            ctx.add_user(&format!("Nachricht {i}: {}", "z".repeat(800)));
        }
        // Der Speicher-Aufruf muss folgenlos durchgehen.
        ctx.save()
            .expect("save eines ephemeren Kontexts schlug fehl");

        let nachher: Vec<_> = std::fs::read_dir(".")
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(
            vorher.len(),
            nachher.len(),
            "Helfer-Kontext hat Dateien angelegt"
        );

        // Und er tut trotzdem seine Arbeit: Rendern liefert die Static-Region.
        let msgs = ctx.messages().expect("messages");
        assert!(msgs.iter().any(|m| m["role"] == "system"), "{msgs:?}");
    }

    /// Separates Compaction-LLM: Major GC (Fact-Extraction + Summarization) läuft
    /// über das konfigurierte Zweit-LLM — nicht über das Agent-LLM.
    #[test]
    fn ctx_separates_compaction_llm_wird_genutzt() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxcomp_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let agent_llm = Arc::new(FakeLlm::new(vec![]));
        let compaction_llm = Arc::new(FakeLlm::new(vec![]));
        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.budget_tokens = 1_000; // winzig: Emergency-Schwelle wird sofort gerissen
                                   // Schwellen-Rechnung unabhängig vom Feature `tiktoken` halten.
        cfg.policy_overlay = Some(json!({"tokenizer": "heuristic"}));
        cfg.compaction_llm = Some(compaction_llm.clone());
        cfg.compaction_model_label = Some("mini-compactor".to_string());
        let ctx = ManagedContext::new(cfg, agent_llm.clone()).unwrap();

        // user_msg ist per Policy nicht externalisierbar — der Minor GC kann nichts
        // retten, also muss der Major GC (Compaction) ran. Die Nachrichten sind so
        // dimensioniert, dass ≥ 2 Units ins Compaction-Fenster passen
        // (max_share 0,5 × Budget 1000 = 500 Tokens; ~200 Tokens je Nachricht).
        for i in 0..6 {
            ctx.add_user(&format!("Nachricht {i}: {}", "z".repeat(800)));
        }
        let _ = ctx.messages();

        assert!(
            compaction_llm.complete_calls() >= 1,
            "Compaction lief nicht über das Zweit-LLM"
        );
        assert_eq!(
            agent_llm.complete_calls(),
            0,
            "Agent-LLM wurde fälschlich für Compaction benutzt"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Der gemeinsame Anklink-Pfad von CLI und TUI: `attach_managed_context`
    /// registriert das Page-Fault-Tool im Agenten UND in der MCP-freien
    /// Basis-Registry, übernimmt den System-Prompt als Static-Region und
    /// schaltet den `/context`-Report auf die ctxman-Statistik um.
    #[test]
    fn attach_managed_context_verdrahtet_agent_und_basis_registry() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxattach_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![]));
        let mut agent = Agent::builder(llm.clone()).system("Testsystem").build();
        let mut mcp_base = ToolRegistry::new();

        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.budget_tokens = 5_000;
        agentkit::attach_managed_context(&mut agent, &mut mcp_base, cfg, llm).unwrap();

        assert!(agent.tools.has("expand_context_ref"));
        assert!(mcp_base.has("expand_context_ref"));
        let report = agentkit::context_report(&agent);
        assert!(report.managed);
        assert_eq!(report.budget, 5_000);
        assert!(report
            .segments
            .iter()
            .any(|s| s.label == "System-Prompt" && s.tokens > 0));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `/context`-Datenbasis mit ctxman: der Report kommt aus den Segmenten
    /// (inklusive Ausgelagert-Hinweis) und trägt das Policy-Budget.
    #[test]
    fn context_report_mit_ctxman_liefert_segment_statistik() {
        let dir = std::env::temp_dir().join(format!("agentkit_ctxrep_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();

        let llm = Arc::new(FakeLlm::new(vec![
            vec![Chunk::tool(0, "c1", "big", "{}")],
            vec![Chunk::text("fertig")],
        ]));
        let mut tools = ToolRegistry::new();
        tools.add(
            "big",
            "Liefert ein riesiges Ergebnis.",
            json!({"type":"object","properties":{},"required":[]}),
            |_args: Value| Ok("x".repeat(15_000)),
        );

        let mut cfg = ManagedContextConfig::new(dir.clone());
        cfg.budget_tokens = 2_000;
        // Schwellen-Rechnung unabhängig vom Feature `tiktoken` halten.
        cfg.policy_overlay = Some(json!({"tokenizer": "heuristic"}));
        let ctx = ManagedContext::new(cfg, llm.clone()).unwrap();
        let mut agent = Agent::builder(llm)
            .tools(tools)
            .system("Testsystem")
            .managed_context(ctx)
            .retry_backoff_ms(0)
            .build();
        agent.run("los");

        let report = agentkit::context_report(&agent);
        assert!(report.managed);
        assert_eq!(report.budget, 2_000); // Policy-Budget, nicht token_budget
        let get = |label: &str| report.segments.iter().find(|s| s.label == label);
        assert!(get("System-Prompt").is_some());
        assert!(get("Tool-Schemas").is_some());
        assert!(get("User-Nachrichten").is_some());
        // Das riesige Tool-Ergebnis wurde externalisiert — der Report weist es aus.
        let results = get("Tool-Ergebnisse").expect("Tool-Ergebnisse fehlen");
        assert!(
            results
                .note
                .as_deref()
                .unwrap_or("")
                .contains("ausgelagert"),
            "Ausgelagert-Hinweis fehlt: {:?}",
            results.note
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Ein panickendes Tool darf einen Bus-Konsumenten nicht ewig blockieren.
///
/// Genau so warten CLI und TUI auf den Lauf: Worker-Thread + `EventBus`. Bleibt
/// der Bus im Aufrufer liegen, überlebt sein Subscriber-Sender den toten Worker
/// und `recv()` kehrt nie zurück — der Prozess hängt. Der Bus muss deshalb per
/// Move in den Worker; dann endet die Schleife mit `Disconnected`.
///
/// (Die "thread panicked"-Zeile in der Testausgabe ist erwartet.)
#[test]
fn panicking_tool_does_not_hang_bus_consumer() {
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::tool(
        0,
        "p1",
        "panic_tool",
        "{}",
    )]]));
    let mut reg = ToolRegistry::new();
    reg.add(
        "panic_tool",
        "Panickt absichtlich.",
        json!({"type": "object", "properties": {}}),
        |_args: Value| -> Result<String, String> { panic!("kaputt") },
    );
    let mut agent = Agent::builder(llm)
        .tools(reg)
        .strategy(Strategy::Plain)
        .build();

    let bus = EventBus::new();
    let q = bus.subscribe();
    let worker = std::thread::spawn(move || {
        agent.run_on_bus("los", &bus, -1, None, "");
    });

    let mut saw_done = false;
    while let Ok(ev) = q.recv() {
        if ev.etype == DONE && ev.source.is_empty() {
            saw_done = true;
            break;
        }
    }
    assert!(!saw_done, "der Lauf hätte gar nicht fertig werden dürfen");
    assert!(worker.join().is_err(), "der Worker hätte panicken müssen");
}

/// Sub-Agenten bekommen dieselbe Luft wie der Haupt-Agent, nicht die
/// Builder-Defaults (8000 Token / 12 Schritte): mit denen kompaktierte ein
/// Explorer schon nach zwei großen Tool-Ergebnissen naiv, und ein `general` war
/// nach 12 Schritten am Ende.
#[test]
fn subagents_get_the_coding_budget_not_the_builder_default() {
    let dir = std::env::temp_dir().join(format!("agentkit_budget_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 20 Turns mit je einem großen Tool-Ergebnis (list_files), dann die finale
    // Antwort. Mit den Builder-Defaults endet das bei "(max_steps erreicht)" und
    // hätte unterwegs die naive Compaction ausgelöst.
    let mut turns: Vec<Vec<Chunk>> = (0..20)
        .map(|i| vec![Chunk::tool(0, &format!("t{i}"), "list_files", "{}")])
        .collect();
    turns.push(vec![Chunk::text("fertig")]);
    let llm = Arc::new(FakeLlm::new(turns));

    let mut reg = ToolRegistry::new();
    agentkit::add_task_tool(
        &mut reg,
        agentkit::TaskToolConfig {
            run: agentkit::RunHandle::new(),
            llm: llm.clone(),
            coding: CodingTools::new(dir.to_str().unwrap(), false),
            roles: agentkit::builtin_roles(),
            mcp: std::sync::Arc::new(agentkit::McpHub::empty()),
            dry_run: false,
            helper_ctx_budget: None,
        },
    );
    let out = reg
        .call(
            "task",
            json!({"prompt":"erkunde","subagent_type":"explorer"}),
        )
        .unwrap();

    assert_eq!(out, "fertig", "Sub-Agent lief in max_steps");
    // Naive Compaction ruft `complete()` — bei ausreichendem Budget passiert das nicht.
    assert_eq!(llm.complete_calls(), 0, "Sub-Agent hat kompaktiert");
    std::fs::remove_dir_all(&dir).ok();
}

/// Ein mitten im Stream abgerissener Modell-Strom ist kein Ergebnis. Vorher
/// verschluckte der SSE-Parser den Lesefehler, der Loop sah nur die bis dahin
/// gesammelten Tokens, hielt sie für die finale Antwort — und die CLI meldete
/// Exit 0 auf eine halbe Antwort.
#[test]
fn truncated_stream_is_an_error_not_a_final_answer() {
    struct TornLlm;
    impl agentkit::llm::Llm for TornLlm {
        fn complete(
            &self,
            _m: &[Value],
            _t: Option<&[Value]>,
        ) -> Result<agentkit::llm::Message, String> {
            unreachable!()
        }
        fn stream(
            &self,
            _m: &[Value],
            _t: Option<&[Value]>,
        ) -> Result<agentkit::llm::ChunkStream, String> {
            Ok(Box::new(
                vec![
                    Ok(Chunk::text("Die halbe Ant")),
                    Err("Stream-Lesefehler: connection reset".to_string()),
                ]
                .into_iter(),
            ))
        }
    }

    let mut agent = Agent::builder(Arc::new(TornLlm))
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build();
    let mut errors = Vec::new();
    let out = agent.run_cb("los", None, |ev| {
        if let EventData::Error { name: None, error } = &ev.data {
            errors.push(error.clone());
        }
    });

    assert_eq!(out, "(keine Antwort)", "Teilantwort galt als Ergebnis");
    assert!(
        errors.iter().any(|e| e.contains("connection reset")),
        "kein ERROR-Event: {errors:?}"
    );
    // Und die CLI macht daraus einen API-Fehler statt Erfolg.
    assert_eq!(
        agentkit::cli::classify_outcome(&out, false),
        Some(agentkit::cli::ExitCode::ApiError)
    );
}

/// `grep` liest nicht mehr jede Datei komplett ein: Binärdateien und alles
/// jenseits der Größengrenze werden übersprungen — sonst zieht ein Workspace mit
/// Build-Artefakten hunderte MB durch den Speicher und spült Zeichensalat in die
/// Historie.
#[test]
fn grep_skips_binary_and_oversized_files() {
    let dir = std::env::temp_dir().join(format!("agentkit_grep_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("klein.txt"), "TREFFER hier\n").unwrap();
    // NUL im Kopf -> binär.
    std::fs::write(dir.join("binaer.bin"), b"\x00\x01TREFFER\n").unwrap();
    // Über 2 MiB -> zu groß.
    let mut riesig = vec![b'x'; 3 * 1024 * 1024];
    riesig.extend_from_slice(b"\nTREFFER\n");
    std::fs::write(dir.join("riesig.log"), riesig).unwrap();

    let tools = CodingTools::new(dir.to_str().unwrap(), false);
    let out = tools.grep("TREFFER", ".", "**/*", 100).unwrap();
    assert!(out.contains("klein.txt"), "{out}");
    assert!(!out.contains("binaer.bin"), "{out}");
    assert!(!out.contains("riesig.log"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Der Glob-Matcher darf bei mehreren `**` nicht kombinatorisch werden. Die
/// rekursive Variante probierte je Stern jede Restlänge durch — ein vom Modell
/// erfundenes `**/**/**/…` reichte, um glob_files festzufahren.
#[test]
fn glob_matcher_stays_fast_with_many_stars() {
    let dir = std::env::temp_dir().join(format!("agentkit_globperf_{}", std::process::id()));
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    // Ein tiefer Baum mit einer Datei je Ebene.
    let mut pfad = String::new();
    for i in 0..12 {
        pfad.push_str(&format!("d{i}/"));
        ct.write_file(&format!("{pfad}f{i}.rs"), "x\n").unwrap();
    }

    let start = std::time::Instant::now();
    let out = ct.glob_files("**/**/**/**/**/**/*.rs", ".", 200).unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "Matcher zu langsam: {:?}",
        start.elapsed()
    );
    assert!(out.contains("f0.rs") && out.contains("f11.rs"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Die Match-Semantik selbst: `**` über null oder mehr Segmente, `*`/`?` innerhalb
/// eines Segments. Regressionsschutz für den Umbau auf das iterative Verfahren.
#[test]
fn glob_matcher_semantics() {
    let dir = std::env::temp_dir().join(format!("agentkit_globsem_{}", std::process::id()));
    let ct = CodingTools::new(dir.to_str().unwrap(), false);
    ct.write_file("oben.txt", "y\n").unwrap();
    ct.write_file("a/b/tief.rs", "z\n").unwrap();

    let treffer = |muster: &str| ct.glob_files(muster, ".", 200).unwrap();
    // `**` matcht auch NULL Segmente …
    assert!(treffer("**/*.txt").contains("oben.txt"));
    assert!(treffer("**").contains("oben.txt"));
    // … und beliebig viele.
    assert!(treffer("**/*.rs").contains("a/b/tief.rs"));
    // `*` bleibt innerhalb eines Segments.
    assert!(!treffer("*.rs").contains("tief.rs"));
    assert!(treffer("*.txt").contains("oben.txt"));
    assert!(treffer("o*e*.txt").contains("oben.txt"));
    // `?` ist genau ein Zeichen.
    assert!(treffer("obe?.txt").contains("oben.txt"));
    assert_eq!(treffer("obe?.txt2"), "(keine Treffer)");
    assert_eq!(treffer("obe??.txt"), "(keine Treffer)");

    std::fs::remove_dir_all(&dir).ok();
}

/// Der Stop-Knopf beendet einen LAUFENDEN Shell-Befehl sofort (kill des
/// Kindprozesses), statt ihn bis zum Timeout weiterlaufen zu lassen — und der
/// Lauf endet mit dem Abbruch-Sentinel, nicht mit dem Tool-Ergebnis.
#[test]
#[cfg(unix)]
fn cancel_beendet_laufenden_shell_befehl_sofort() {
    let dir = std::env::temp_dir().join(format!("agentkit_cancel_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let run = agentkit::RunHandle::new();
    let tools = agentkit::CodingTools::with_approve(
        dir.to_str().unwrap(),
        false,
        Arc::new(|_: &str| true),
        120, // großzügiger Timeout — der Abbruch muss VOR ihm greifen
    )
    .with_run_handle(run.clone());
    let mut reg = ToolRegistry::new();
    tools.register(&mut reg, None);

    let turns = vec![
        vec![Chunk::tool(
            0,
            "c1",
            "run_shell",
            &json!({"command": "sleep 30"}).to_string(),
        )],
        vec![Chunk::text("sollte nie erreicht werden")],
    ];
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(reg)
        .strategy(Strategy::Plain)
        .run_handle(run)
        .build();

    let cancel = new_cancel();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        c.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let start = std::time::Instant::now();
    let bus = EventBus::new();
    let final_ = agent.run_on_bus("lauf", &bus, 0, Some(&cancel), "");
    assert_eq!(final_, "(abgebrochen)");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "Abbruch hat {}s gebraucht — der Kindprozess wurde nicht beendet",
        start.elapsed().as_secs()
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Ein Befehl, der in den Timeout läuft, wird abgeschossen — vorher lief er im
/// Hintergrund weiter und der Agent sammelte über eine Sitzung hängende Prozesse.
#[test]
#[cfg(unix)]
fn run_shell_kills_the_child_on_timeout() {
    let dir = std::env::temp_dir().join(format!("agentkit_kill_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("spaet.txt");
    let tools = agentkit::CodingTools::with_approve(
        dir.to_str().unwrap(),
        false,
        std::sync::Arc::new(|_: &str| true),
        1, // 1 s Timeout
    );

    // Schreibt den Marker erst nach 5 s — läuft der Prozess weiter, ist er da.
    let out = tools
        .run_shell("sleep 4; echo spaet > spaet.txt")
        .expect("run_shell");
    assert!(out.contains("Timeout"), "{out}");

    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(
        !marker.exists(),
        "der Kindprozess lief nach dem Timeout weiter"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `Plan::update` darf den on_update-Callback nicht unter dem eigenen Lock
/// aufrufen: der Callback ist fremder Code (im Frontend eine Bus-Publikation), und
/// greift er auf den Plan zurück, hinge er an einem Lock seines eigenen Aufrufers.
#[test]
fn plan_update_callback_runs_without_holding_the_lock() {
    use std::sync::{Mutex, OnceLock};
    let gesehen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Henne-Ei: der Callback wird vor dem Plan gebaut, braucht ihn aber. Über die
    // OnceLock erreicht er GENAU den Plan, dessen update() ihn gerade aufruft —
    // ein render() daraus lief vorher in den Deadlock.
    let selbst: Arc<OnceLock<Plan>> = Arc::new(OnceLock::new());

    let handle = selbst.clone();
    let ziel = gesehen.clone();
    let plan = Plan::with_on_update(move |_steps| {
        if let Some(p) = handle.get() {
            ziel.lock().unwrap().push(p.render());
        }
    });
    selbst.set(plan.clone()).ok();

    plan.update(vec![Step {
        step: "erster Schritt".to_string(),
        status: "pending".to_string(),
    }]);
    assert_eq!(plan.len(), 1);
    assert_eq!(
        gesehen.lock().unwrap().as_slice(),
        ["[ ] 1. erster Schritt".to_string()]
    );
}

/// Der Delegations-Block darf NUR im Prompt stehen, wenn es das `task`-Tool auch
/// gibt: mit `--no-subagents` wäre er eine Anleitung für ein fehlendes Werkzeug.
/// Umgekehrt muss er die Orientierungsregel aus `CODING_SYSTEM` ausdrücklich
/// überschreiben — sonst lesen sich die beiden Absätze widersprüchlich und der
/// Agent liest weiter selbst halbe Repos in seinen Kontext.
#[test]
fn delegations_hinweis_haengt_am_task_tool() {
    use agentkit::{build_coding_agent, ApproveFn, CodingAgentConfig, McpHub};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("agentkit_deleg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let bauen = |subagents: bool| {
        let cfg = CodingAgentConfig {
            workspace: dir.to_str().unwrap(),
            strategy: Strategy::Plain,
            max_steps: 5,
            skills: None,
            agents: None,
            memory: None,
            subagents,
            system: None,
            verify: false,
            shell_timeout: 120,
            dry_run: false,
            extra_tools: None,
            helper_ctx_budget: None,
        };
        let approve: ApproveFn = Arc::new(|_| true);
        let llm = Arc::new(FakeLlm::new(vec![]));
        let (agent, ..) = build_coding_agent(llm, &cfg, approve, Arc::new(McpHub::empty()));
        let system = agent.memory.messages[0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        (agent.tools.has("task"), system)
    };

    let (hat_task, mit) = bauen(true);
    assert!(hat_task, "task-Tool fehlt trotz subagents = true");
    assert!(mit.contains("explorer"), "{mit}");
    // Die Begründung (Kontext bleibt klein) und der Vorrang vor CODING_SYSTEM.
    assert!(mit.contains("NICHT in deinem"), "Begründung fehlt");
    // Und die Orientierungsregel selbst ist die delegierende Variante — sonst
    // stünde ganz vorn im Prompt weiter "lies erst mal selbst".
    assert!(mit.contains("NICHT selbst viele Dateien"), "{mit}");
    assert!(
        !mit.contains("Verschaffe dir zuerst mit list_files"),
        "{mit}"
    );

    let (hat_task, ohne) = bauen(false);
    assert!(!hat_task, "task-Tool trotz subagents = false");
    assert!(
        !ohne.contains("subagent_type"),
        "wirbt für fehlendes Tool:
{ohne}"
    );
    // Ohne Sub-Agenten bleibt die ursprüngliche Orientierungsregel stehen.
    assert!(
        ohne.contains("Verschaffe dir zuerst mit list_files"),
        "{ohne}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Der Einwurf ist die Rückfalllinie zum System-Prompt: er greift, sobald der
/// Orchestrator zu viele Dateien SELBST liest — unabhängig davon, wie gut ein
/// Modell mehrschichtige Instruktionen befolgt. Und er greift nur, wenn es das
/// `task`-Tool auch gibt (Sub-Agenten und Schwarm-Mitglieder haben es nie).
#[test]
fn delegations_einwurf_bei_zu_vielen_eigenen_read_file() {
    use agentkit::DELEGATE_NUDGE;

    fn registry(mit_task: bool) -> ToolRegistry {
        let leer = json!({"type": "object", "properties": {}});
        let mut reg = ToolRegistry::new();
        reg.add("read_file", "liest eine Datei", leer.clone(), |_| {
            Ok("Dateiinhalt".to_string())
        });
        if mit_task {
            reg.add("task", "delegiert", leer, |_| Ok("Bericht".to_string()));
        }
        reg
    }

    // Ein Zug mit vier read_file-Aufrufen, danach die finale Antwort.
    fn turns(n: usize) -> Vec<Vec<Chunk>> {
        vec![
            (0..n)
                .map(|i| Chunk::tool(i, &format!("r{i}"), "read_file", "{}"))
                .collect(),
            vec![Chunk::text("fertig")],
        ]
    }

    let eingeworfen = |mit_task: bool, gelesen: usize| {
        let llm = Arc::new(FakeLlm::new(turns(gelesen)));
        let mut agent = Agent::builder(llm)
            .tools(registry(mit_task))
            .strategy(Strategy::Plain)
            .retry_backoff_ms(0)
            .build();
        agent.run("Erklär mir dieses Crate.");
        agent
            .memory
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .any(|m| m["content"].as_str() == Some(DELEGATE_NUDGE))
    };

    assert!(eingeworfen(true, 4), "Einwurf fehlt trotz vier read_file");
    assert!(!eingeworfen(true, 2), "Einwurf schon bei zwei Dateien");
    assert!(!eingeworfen(false, 4), "Einwurf ohne task-Tool");
}

// ------------------------------------------------------------------ Trace

/// Der Trace ist ein NDJSON-Sink auf dem vorhandenen Ereignisstrom: was
/// hineingeht, muss zeilenweise und wieder parsbar herauskommen — inklusive
/// `seq`, `at_ms`, `source` und `etype`.
#[test]
fn trace_schreibt_wieder_parsbare_ndjson_zeilen() {
    use agentkit::TraceWriter;

    let dir = std::env::temp_dir().join(format!("agentkit_trace_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = TraceWriter::create(&dir).unwrap();

    writer.write_event(&AgentEvent::new(
        TOOL_CALL,
        EventData::ToolCall {
            name: "read_file".to_string(),
            args: json!({"path": "a.txt"}),
        },
    ));
    writer.write_event(&AgentEvent::with_meta(
        TOOL_RESULT,
        EventData::ToolResult {
            name: "read_file".to_string(),
            result: "Inhalt".to_string(),
        },
        7,
        "delegate:Wien".to_string(),
    ));
    writer.write_event(&AgentEvent::new(DONE, EventData::Done));

    let text = std::fs::read_to_string(writer.path()).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("jede Zeile ist gültiges JSON"))
        .collect();
    assert_eq!(lines.len(), 3);
    // Fortlaufende Sequenz, ab 1.
    assert_eq!(lines[0]["seq"], 1);
    assert_eq!(lines[2]["seq"], 3);
    assert!(lines[0]["at_ms"].as_u64().unwrap() > 0);
    assert_eq!(lines[0]["schema_version"], "1");
    assert_eq!(lines[0]["etype"], TOOL_CALL);
    assert_eq!(lines[0]["data"]["tool_call"]["name"], "read_file");
    assert_eq!(lines[0]["data"]["tool_call"]["args"]["path"], "a.txt");
    // `source`/`task_id` tragen die Zuordnung zum (Sub-)Agenten.
    assert_eq!(lines[1]["source"], "delegate:Wien");
    assert_eq!(lines[1]["task_id"], 7);
    assert_eq!(lines[2]["etype"], DONE);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Trace hängt am BUS, nicht an einem einzelnen Consumer — sonst hinge er
/// daran, dass gerade ein bestimmtes Frontend zuhört. Und `text_delta` (ein
/// Ereignis PRO TOKEN) bleibt draußen: derselbe Text steht als `final` drin.
#[test]
fn bus_mit_trace_schreibt_mit_und_laesst_token_deltas_aus() {
    use agentkit::TraceWriter;

    let dir = std::env::temp_dir().join(format!("agentkit_trace_bus_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = Arc::new(TraceWriter::create(&dir).unwrap());
    let bus = EventBus::with_trace(writer.clone());

    // Kein Subscriber — der Mitschnitt darf davon nicht abhängen.
    bus.publish(AgentEvent::new(
        TEXT_DELTA,
        EventData::TextDelta("Erg".into()),
    ));
    bus.publish(AgentEvent::new(
        TEXT_DELTA,
        EventData::TextDelta("ebnis".into()),
    ));
    bus.publish(AgentEvent::new(FINAL, EventData::Final("Ergebnis".into())));

    let text = std::fs::read_to_string(writer.path()).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "nur das final-Ereignis: {text}");
    assert_eq!(lines[0]["etype"], FINAL);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein Trace darf nicht größer werden als das Repo, das er beobachtet: ein
/// riesiges Tool-Ergebnis wird gekürzt — MIT Vermerk, nie stillschweigend.
#[test]
fn trace_kuerzt_grosse_nutzlasten_mit_vermerk() {
    use agentkit::trace::MAX_TEXT_CHARS;
    use agentkit::TraceWriter;

    let dir = std::env::temp_dir().join(format!("agentkit_trace_kurz_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = TraceWriter::create(&dir).unwrap();

    let riesig = "x".repeat(MAX_TEXT_CHARS * 3);
    writer.write_event(&AgentEvent::new(
        TOOL_RESULT,
        EventData::ToolResult {
            name: "read_file".to_string(),
            result: riesig.clone(),
        },
    ));

    let text = std::fs::read_to_string(writer.path()).unwrap();
    let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let geschrieben = line["data"]["tool_result"]["result"].as_str().unwrap();
    assert!(geschrieben.chars().count() < riesig.chars().count());
    assert!(
        geschrieben.contains("Zeichen gekürzt"),
        "die Originalgröße muss vermerkt sein: {geschrieben}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Strukturierte Fremd-Nutzlast (`EventData::Structured`) überlebt den Weg
/// über den Bus und in den Trace verlustfrei — das ist die Naht, über die
/// agentkit-swarm & Co. ihre Ereignisse schicken, ohne dass der Kern sie kennt.
#[test]
fn trace_haelt_strukturierte_nutzlast_verlustfrei() {
    use agentkit::TraceWriter;

    let dir = std::env::temp_dir().join(format!("agentkit_trace_struct_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = TraceWriter::create(&dir).unwrap();

    let bus = EventBus::new();
    let q = bus.subscribe();
    bus.publish(AgentEvent::structured(
        "swarm_event",
        json!({"message_queued": {"from": "a", "to": "b", "kind": "request"}}),
        3,
        "a",
    ));
    let ev = q.recv().unwrap();
    writer.write_event(&ev);

    let text = std::fs::read_to_string(writer.path()).unwrap();
    let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(line["etype"], "structured");
    assert_eq!(line["data"]["structured"]["kind"], "swarm_event");
    assert_eq!(
        line["data"]["structured"]["payload"]["message_queued"]["kind"],
        "request"
    );
    assert_eq!(line["source"], "a");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein Trace enthält alles, was der Agent gelesen und geschrieben hat — er
/// gehört nie in ein Repository. Deshalb legt `create` eine `.gitignore` mit
/// `*` daneben (dieselbe Idee wie beim Work-Journal).
#[test]
fn trace_verzeichnis_bekommt_eine_gitignore() {
    use agentkit::TraceWriter;

    let dir = std::env::temp_dir().join(format!("agentkit_trace_gi_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let writer = TraceWriter::create(&dir).unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "*\n"
    );
    assert!(writer.path().starts_with(&dir));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein FEHLGESCHLAGENER Check löst die Prüfpflicht nicht ein.
///
/// Beobachtet an `polyglot_python_react`: `pytest -q react_test.py` mit
/// `exit=1`, im nächsten Schritt die Abschlussmeldung — nach 28 von 100
/// Schritten. Der Agent hörte beim ersten roten Test auf, obwohl
/// `verify_before_final` aktiv war: bis dahin galt JEDER ausgeführte
/// Shell-Befehl als Verifikation, egal wie er ausging.
#[test]
fn ein_roter_check_erfuellt_die_pruefpflicht_nicht() {
    let mut reg = ToolRegistry::new();
    reg.add(
        "write_file",
        "Schreibt eine Datei.",
        json!({"type":"object","properties":{"path":{"type":"string"}}}),
        |_| Ok("geschrieben".to_string()),
    );
    // Erst rot, dann grün — im ECHTEN Format von `coding.rs`.
    let laeufe = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let l = laeufe.clone();
    reg.add(
        "run_shell",
        "Führt einen Befehl aus.",
        json!({"type":"object","properties":{"command":{"type":"string"}}}),
        move |_| {
            let n = l.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if n == 0 {
                "exit=1\n--- STDOUT ---\n.....F\n--- STDERR ---\n".to_string()
            } else {
                "exit=0\n--- STDOUT ---\n......\n--- STDERR ---\n".to_string()
            })
        },
    );

    let turns = vec![
        vec![Chunk::tool(
            0,
            "c1",
            "write_file",
            "{\"path\":\"react.py\"}",
        )],
        vec![Chunk::tool(
            0,
            "c2",
            "run_shell",
            "{\"command\":\"pytest\"}",
        )],
        // Hier hörte der Agent bisher auf — der rote Test galt als Verifikation.
        vec![Chunk::text("Implemented the reactive cells.")],
        vec![Chunk::tool(
            0,
            "c3",
            "run_shell",
            "{\"command\":\"pytest\"}",
        )],
        vec![Chunk::text("Fertig, Tests grün.")],
    ];
    let mut agent = Agent::builder(Arc::new(FakeLlm::new(turns)))
        .tools(reg)
        .strategy(Strategy::Plain)
        .verify_before_final(true)
        .build();

    assert_eq!(
        agent.run("repariere react.py"),
        "Fertig, Tests grün.",
        "der Lauf darf nicht mit rotem Check enden"
    );
    let nudges = agent
        .memory
        .messages
        .iter()
        .filter(|m| {
            m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.contains("Halt:"))
        })
        .count();
    assert_eq!(nudges, 1, "genau ein Einwurf nach dem roten Check");
}
