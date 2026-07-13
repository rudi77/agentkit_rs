//! Tests ohne Netz — Tools, Memory, Events, MCP-Konvertierung und der Agent-Loop
//! mit einem FakeLlm, das OpenAI-Streaming-Chunks nachstellt. Spiegelt
//! `agent_framework/tests/test_agentkit.py`.

use std::sync::Arc;

use agentkit::events::{DONE, FINAL, TOOL_CALL, TOOL_RESULT};
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
    ] {
        assert!(
            is_likely_destructive(d),
            "{d} sollte als zerstörerisch gelten"
        );
    }
    for safe in ["read_file", "list_files", "recall", "add", "wetter"] {
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
    agent.run_cb("rechne", None, |ev| {
        if let EventData::ToolResult { result, .. } = &ev.data {
            results.push(result.clone());
        }
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

#[test]
fn task_tool_registers_and_runs_subagent() {
    let dir = std::env::temp_dir().join(format!("agentkit_task_{}", std::process::id()));
    let run = agentkit::RunHandle::new();
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("SUBERGEBNIS")]]));
    let mut reg = ToolRegistry::new();
    agentkit::add_task_tool(
        &mut reg,
        run,
        llm,
        dir.to_str().unwrap(),
        false,
        None,
        agentkit::builtin_roles(),
        std::sync::Arc::new(agentkit::McpHub::empty()),
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
    };
    let approve: ApproveFn = Arc::new(|_| true);
    let (mut agent, _p, _s, _r, _b) =
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
            ),
            "Platzhalter aus der Vorlage wurde gesetzt: {k}={v}"
        );
    }
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
