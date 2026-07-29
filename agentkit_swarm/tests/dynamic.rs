//! Tests für das `swarm`-Tool (`src/dynamic.rs`) — ein Agent baut sich seinen
//! Schwarm zur Laufzeit selbst. Offline, ohne Netz.
//!
//! **Warum ein eigenes LLM statt `FakeLlm`:** alle Mitglieder teilen sich EIN
//! `Arc<dyn Llm>` (das ist der Sinn der Sache — sie sind Rollen desselben
//! Modells). `FakeLlm` zählt seine Turns aber global in Aufrufreihenfolge, und
//! die ist bei nebenläufigen Actors nicht deterministisch. [`PerAgentLlm`]
//! skriptet deshalb PRO Agent und erkennt den Fragesteller an dessen
//! System-Prompt ("Deine Agent-ID ist '…'") — dieselbe Zeile, die `dynamic.rs`
//! jedem Mitglied mitgibt. Das braucht keine Test-Naht im Produktivcode.
//!
//! Scripting-Konvention wie in `integration.rs`: ein Turn je Loop-Schritt, ein
//! Turn mit Tool-Aufruf braucht danach einen weiteren Turn für die finale
//! Antwort. Läuft ein Skript leer, endet der Agent mit leerem Text.

use agentkit::coding::CodingTools;
use agentkit::llm::{chunk_stream, Chunk, ChunkStream, Llm, Message};
use agentkit::testing::FakeLlm;
use agentkit::{
    Agent, AgentRole, EventBus, EventData, McpHub, RunHandle, Skills, Strategy, ToolRegistry,
};
use agentkit_swarm::{add_swarm_tool, SwarmLimits, SwarmToolConfig};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ Helfer

/// Ein LLM, das je Agent-ID ein eigenes Skript abspielt.
struct PerAgentLlm {
    scripts: Mutex<HashMap<String, VecDeque<Vec<Chunk>>>>,
    /// System-Prompt, den jeder Agent gesehen hat (für Prompt-Assertions).
    systems: Mutex<HashMap<String, String>>,
}

impl PerAgentLlm {
    fn new(scripts: Vec<(&str, Vec<Vec<Chunk>>)>) -> Arc<Self> {
        Arc::new(PerAgentLlm {
            scripts: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(id, turns)| (id.to_string(), turns.into()))
                    .collect(),
            ),
            systems: Mutex::new(HashMap::new()),
        })
    }

    /// Der System-Prompt eines Mitglieds, wie es ihn gesehen hat.
    fn system_of(&self, id: &str) -> String {
        self.systems
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
}

/// Zieht die Agent-ID aus dem System-Prompt (`dynamic.rs` schreibt sie dort hin).
fn agent_id_from_system(system: &str) -> Option<String> {
    let rest = system.split_once("Deine Agent-ID ist '")?.1;
    Some(rest.split_once('\'')?.0.to_string())
}

impl Llm for PerAgentLlm {
    fn complete(&self, _messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("zusammengefasst".into()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        let system = messages
            .iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("");
        let Some(id) = agent_id_from_system(system) else {
            return Err("Prompt ohne Agent-ID — kein Schwarm-Mitglied?".into());
        };
        self.systems
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_insert_with(|| system.to_string());
        let turn = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_default();
        Ok(chunk_stream(turn))
    }
}

/// Ein Workspace je Test (die Coding-Tools legen ihn an).
fn workspace(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("agentkit_swarm_{name}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

fn config(llm: Arc<dyn Llm>, ws: &str) -> SwarmToolConfig {
    SwarmToolConfig {
        run: RunHandle::new(),
        llm,
        coding: CodingTools::with_approve(ws, true, Arc::new(|_: &str| true), 30),
        skills: None,
        roles: Vec::new(),
        mcp: Arc::new(McpHub::empty()),
        dry_run: false,
        limits: SwarmLimits {
            // Ohne Frist entscheiden — sonst wartete JEDER Konsens-Test 20 s.
            // Die Frist selbst prüft `abstimmungsfrist_sammelt_weitere_stimmen`.
            vote_window_s: 0,
            ..SwarmLimits::default()
        },
        extra_member_tools: None,
        helper_ctx_budget: None,
    }
}

/// Registry mit dem `swarm`-Tool — für den direkten Aufruf ohne Orchestrator.
fn registry(cfg: SwarmToolConfig) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    add_swarm_tool(&mut reg, cfg);
    reg
}

fn call_swarm(reg: &ToolRegistry, spec: Value) -> String {
    reg.call("swarm", spec).expect("swarm-Tool lieferte Err")
}

/// Ein Orchestrator, der GENAU einen `swarm`-Aufruf macht und danach antwortet.
/// Liefert den Agenten und den geteilten Lauf-Kontext (den das Tool braucht).
fn orchestrator(spec: Value, cfg_llm: Arc<dyn Llm>, ws: &str) -> (Agent, RunHandle) {
    let run = RunHandle::new();
    let mut cfg = config(cfg_llm, ws);
    cfg.run = run.clone();
    let mut tools = ToolRegistry::new();
    add_swarm_tool(&mut tools, cfg);

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(0, "s1", "swarm", &spec.to_string())],
        vec![Chunk::text("Der Schwarm ist fertig.")],
    ]));
    let agent = Agent::builder(llm)
        .tools(tools)
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .run_handle(run.clone())
        .build();
    (agent, run)
}

/// Führt einen `swarm`-Aufruf über einen echten Orchestrator-Lauf aus und gibt
/// das Tool-Ergebnis samt allen Events zurück — nur so ist auch sichtbar, was die
/// MITGLIEDER getan haben (deren Events tragen ihre Agent-ID als `source`).
fn run_with_events(
    spec: Value,
    llm: Arc<dyn Llm>,
    ws: &str,
) -> (String, Vec<agentkit::AgentEvent>) {
    let (mut agent, _run) = orchestrator(spec, llm, ws);
    let bus = EventBus::new();
    let rx = bus.subscribe();
    agent.run_on_bus("Los.", &bus, 0, None, "");
    let events: Vec<_> = rx.try_iter().collect();
    (swarm_result(&events), events)
}

/// Das Tool-Ergebnis des ORCHESTRATORS (`source` leer) — ein Mitglied, das selbst
/// `swarm` aufruft, erzeugt ein gleichnamiges Ergebnis und würde sonst gewinnen.
fn swarm_result(events: &[agentkit::AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolResult { name, result } if name == "swarm" && e.source.is_empty() => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("kein tool_result für 'swarm'")
}

/// Das Ergebnis eines Mitglied-Tool-Aufrufs aus dem Event-Strom.
fn tool_result_of(events: &[agentkit::AgentEvent], agent: &str, tool: &str) -> String {
    events
        .iter()
        .find_map(|e| match &e.data {
            EventData::ToolResult { name, result } if name == tool && e.source == agent => {
                Some(result.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("kein tool_result '{tool}' von '{agent}'"))
}

/// Zwei Mitglieder in Kette: `a` schlägt vor, `b` stimmt zu. Der Vorschlag ist
/// `msg-2` (msg-1 = Initialaufgabe) — in einer Kette ohne Races deterministisch.
fn propose_and_vote(ergebnis: &str) -> Arc<PerAgentLlm> {
    PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    &json!({ "proposal": ergebnis }).to_string(),
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true,"comment":"passt"}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ])
}

fn kette_spec(auftrag: &str) -> Value {
    json!({
        "auftrag": auftrag,
        "topologie": "kette",
        "agenten": [
            {"id": "a", "system": "Du entwirfst."},
            {"id": "b", "system": "Du prüfst."}
        ]
    })
}

// ------------------------------------------------------------- Der Happy Path

#[test]
fn swarm_tool_erreicht_konsens_und_liefert_das_ergebnis() {
    let ws = workspace("konsens");
    let llm = propose_and_vote("Retry-Backoff mit Obergrenze umgesetzt.");
    let reg = registry(config(llm.clone(), &ws));

    let out = call_swarm(&reg, kette_spec("Klärt das Backoff und schließt ab."));
    let v: Value = serde_json::from_str(&out).expect("kein JSON: {out}");

    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["ergebnis"], "Retry-Backoff mit Obergrenze umgesetzt.");
    assert_eq!(v["zustimmungen"], 1);
    // Turn-Statistik: beide Mitglieder haben je eine Nachricht bearbeitet.
    assert_eq!(v["turns"]["a"], 1, "{out}");
    assert_eq!(v["turns"]["b"], 1, "{out}");
    // Budgetiert ist hier nur der Kickoff: die Peer-Kopien eines `swarm_propose`
    // gehen wie der Weg zum CompletionActor am Budget vorbei — sonst wäre der
    // Abschluss ausgerechnet am erschöpften Limit unmöglich.
    assert_eq!(v["nachrichten"], 1, "{out}");
    assert_eq!(v["unzustellbar"], 0, "{out}");

    // Der Auftrag kam beim Startagenten an, im [SWARM MESSAGE]-Format.
    let system_a = llm.system_of("a");
    assert!(
        system_a.contains("swarm_propose"),
        "Protokoll fehlt im Prompt"
    );
    assert!(system_a.contains("Du entwirfst."), "Rollen-Prompt fehlt");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn orchestrator_bekommt_das_ergebnis_als_tool_result() {
    let ws = workspace("orchestrator");
    let llm = propose_and_vote("Fertig: Variante B.");
    let (mut agent, _run) = orchestrator(kette_spec("Entscheidet euch."), llm, &ws);

    let bus = EventBus::new();
    let rx = bus.subscribe();
    let answer = agent.run_on_bus("Findet die beste Variante.", &bus, 0, None, "");
    assert_eq!(answer, "Der Schwarm ist fertig.");

    let events: Vec<_> = rx.try_iter().collect();
    // Das Tool-Ergebnis des `swarm`-Aufrufs enthält den Vorschlagstext.
    let ergebnis = swarm_result(&events);
    assert!(ergebnis.contains("Fertig: Variante B."), "{ergebnis}");
    assert!(ergebnis.contains("konsens"), "{ergebnis}");

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------- Weiche Fehler

#[test]
fn ungueltige_spezifikationen_sind_weiche_fehler() {
    let ws = workspace("ungueltig");
    let llm = PerAgentLlm::new(vec![]);
    let reg = registry(config(llm, &ws));

    let faelle: Vec<(&str, Value, &str)> = vec![
        (
            "leerer Auftrag",
            json!({"auftrag": "  ", "agenten": [{"id": "a"}]}),
            "'auftrag' fehlt",
        ),
        (
            "keine Agenten",
            json!({"auftrag": "x", "agenten": []}),
            "mindestens ein Mitglied",
        ),
        (
            "zu viele Agenten",
            json!({"auftrag": "x", "agenten": (0..9).map(|i| json!({"id": format!("a{i}")})).collect::<Vec<_>>()}),
            "Limit von 6",
        ),
        (
            "doppelte ID",
            json!({"auftrag": "x", "agenten": [{"id": "a"}, {"id": "a"}]}),
            "doppelte Agent-ID",
        ),
        (
            "reservierte ID",
            json!({"auftrag": "x", "agenten": [{"id": "runtime"}]}),
            "reserviert",
        ),
        (
            "unbekannter start_agent",
            json!({"auftrag": "x", "agenten": [{"id": "a"}], "start_agent": "z"}),
            "unbekannter 'start_agent'",
        ),
        (
            "unbekannte Topologie",
            json!({"auftrag": "x", "agenten": [{"id": "a"}, {"id": "b"}], "topologie": "ring"}),
            "unbekannte 'topologie'",
        ),
        (
            "Kante ins Leere",
            json!({"auftrag": "x", "agenten": [{"id": "a"}, {"id": "b"}], "verbindungen": [["a", "z"]]}),
            "unbekannte Agent-ID 'z'",
        ),
        (
            "Kante auf sich selbst",
            json!({"auftrag": "x", "agenten": [{"id": "a"}, {"id": "b"}], "verbindungen": [["a", "a"]]}),
            "auf sich selbst",
        ),
        (
            "Verbindung kein Paar",
            json!({"auftrag": "x", "agenten": [{"id": "a"}, {"id": "b"}], "verbindungen": [["a"]]}),
            "erwartet Paare",
        ),
        (
            "Feld falschen Typs",
            json!({"auftrag": "x", "agenten": [{"id": 42}]}),
            "nicht lesbar",
        ),
    ];

    for (name, spec, erwartet) in faelle {
        let out = call_swarm(&reg, spec);
        assert!(
            out.starts_with("ERROR: "),
            "{name}: erwartete weichen Fehler, bekam: {out}"
        );
        assert!(
            out.contains(erwartet),
            "{name}: erwartete '{erwartet}' in: {out}"
        );
    }

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------ Grenzen der Mitglieder

// Keine Rekursion: ein Mitglied kann weder einen eigenen Schwarm bauen noch
// Sub-Agenten starten — dieselbe Invariante wie beim `task`-Tool in agentkit.
#[test]
fn mitglieder_haben_weder_swarm_noch_task_tool() {
    let ws = workspace("rekursion");
    let llm = PerAgentLlm::new(vec![(
        "a",
        vec![
            vec![
                Chunk::tool(
                    0,
                    "r1",
                    "swarm",
                    r#"{"auftrag":"x","agenten":[{"id":"q"}]}"#,
                ),
                Chunk::tool(1, "r2", "task", r#"{"prompt":"x"}"#),
            ],
            vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                r#"{"proposal":"nichts davon ging"}"#,
            )],
            vec![Chunk::text("fertig")],
        ],
    )]);
    // Solo-Schwarm: Quorum 0, der Vorschlag schließt sofort ab.
    let (out, events) = run_with_events(
        json!({"auftrag": "Versuche zu eskalieren.", "agenten": [{"id": "a"}]}),
        llm,
        &ws,
    );

    // Beide Versuche laufen ins Leere — als WEICHE Fehler, der Loop läuft weiter.
    assert!(
        tool_result_of(&events, "a", "swarm").contains("unbekanntes Tool"),
        "ein Mitglied konnte einen Sub-Schwarm bauen"
    );
    assert!(
        tool_result_of(&events, "a", "task").contains("unbekanntes Tool"),
        "ein Mitglied konnte einen Sub-Agenten starten"
    );

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["ergebnis"], "nichts davon ging");
    assert_eq!(v["zustimmungen"], 0, "Solo-Schwarm braucht keine Stimmen");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn default_toolset_ist_read_only_und_alle_oeffnet_es() {
    let ws = workspace("readonly");
    // Beide Mitglieder versuchen dasselbe: eine Datei schreiben, dann abschließen.
    let schreiben = |datei: &str| {
        vec![
            vec![Chunk::tool(
                0,
                "w1",
                "write_file",
                &json!({"path": datei, "content": "hallo"}).to_string(),
            )],
            vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                r#"{"proposal":"versucht"}"#,
            )],
            vec![Chunk::text("fertig")],
        ]
    };
    let llm = PerAgentLlm::new(vec![
        ("leser", schreiben("verboten.txt")),
        ("schreiber", schreiben("erlaubt.txt")),
    ]);
    let reg = registry(config(llm, &ws));

    // 1. Ohne `tools`-Feld: read-only -> write_file ist gar nicht registriert.
    let out = call_swarm(
        &reg,
        json!({"auftrag": "Schreib was.", "agenten": [{"id": "leser"}]}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["status"],
        "konsens",
        "{out}"
    );
    assert!(
        !std::path::Path::new(&ws).join("verboten.txt").exists(),
        "read-only-Mitglied konnte schreiben"
    );

    // 2. Mit `tools: "alle"`: derselbe Aufruf schreibt wirklich.
    let out = call_swarm(
        &reg,
        json!({"auftrag": "Schreib was.", "agenten": [{"id": "schreiber", "tools": "alle"}]}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["status"],
        "konsens",
        "{out}"
    );
    assert!(
        std::path::Path::new(&ws).join("erlaubt.txt").exists(),
        "Mitglied mit 'alle' konnte nicht schreiben"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// `--dry-run` des Orchestrators muss auch für Mitglieder mit `tools: "alle"`
/// gelten — sonst wäre der Schwarm der Weg, das Sicherheitsnetz zu umgehen.
#[test]
fn dry_run_gilt_auch_fuer_schwarm_mitglieder() {
    let ws = workspace("dryrun");
    let llm = PerAgentLlm::new(vec![(
        "schreiber",
        vec![
            vec![Chunk::tool(
                0,
                "w1",
                "write_file",
                &json!({"path": "trotzdem.txt", "content": "hallo"}).to_string(),
            )],
            vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                r#"{"proposal":"versucht"}"#,
            )],
            vec![Chunk::text("fertig")],
        ],
    )]);
    let mut cfg = config(llm, &ws);
    cfg.dry_run = true;
    let reg = registry(cfg);

    let out = call_swarm(
        &reg,
        json!({"auftrag": "Schreib was.", "agenten": [{"id": "schreiber", "tools": "alle"}]}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["status"],
        "konsens",
        "{out}"
    );
    assert!(
        !std::path::Path::new(&ws).join("trotzdem.txt").exists(),
        "Schwarm-Mitglied hat unter --dry-run geschrieben"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn tools_als_namensliste_waehlt_genau_diese_werkzeuge() {
    let ws = workspace("namensliste");
    let llm = PerAgentLlm::new(vec![(
        "a",
        vec![
            // read_file ist erlaubt, grep nicht.
            vec![Chunk::tool(0, "g1", "grep", r#"{"pattern":"x"}"#)],
            vec![Chunk::tool(
                0,
                "p1",
                "swarm_propose",
                r#"{"proposal":"geprüft"}"#,
            )],
            vec![Chunk::text("fertig")],
        ],
    )]);
    let (out, events) = run_with_events(
        json!({
            "auftrag": "Sieh nach.",
            "agenten": [{"id": "a", "tools": "read_file, list_files"}]
        }),
        llm,
        &ws,
    );

    // `grep` stand nicht auf der Liste — weicher Fehler, der Loop läuft weiter.
    assert!(
        tool_result_of(&events, "a", "grep").contains("unbekanntes Tool"),
        "grep war trotz Namensliste registriert"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["status"],
        "konsens",
        "{out}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------------------------ Topologie

// Der Nachbarschafts-Satz jedes Mitglieds steht in seinem System-Prompt (und ist
// zugleich das, was `swarm_peers` liefert) — genau das prüfen wir hier. `a`
// broadcastet einmal, damit auch `b` einen Turn bekommt: erst dann hat es einen
// Prompt gesehen. Aus dem PAAR (a, b) sind alle drei Presets unterscheidbar.
#[test]
fn topologie_presets_bestimmen_die_nachbarn() {
    let ws = workspace("topologie");

    fn script() -> Vec<(&'static str, Vec<Vec<Chunk>>)> {
        vec![
            (
                "a",
                vec![
                    vec![Chunk::tool(
                        0,
                        "b1",
                        "swarm_broadcast",
                        r#"{"content":"Stellt euch vor.","kind":"request"}"#,
                    )],
                    vec![Chunk::text("gesendet")],
                ],
            ),
            ("b", vec![vec![Chunk::text("bin b")]]),
            ("c", vec![vec![Chunk::text("bin c")]]),
        ]
    }
    let spec = |topologie: Value| {
        json!({
            "auftrag": "Stellt euch vor.",
            "topologie": topologie,
            "max_laufzeit_s": 2,
            "agenten": [{"id": "a"}, {"id": "b"}, {"id": "c"}],
            "erforderliche_zustimmungen": 2
        })
    };
    let nachbarn = |llm: &PerAgentLlm, id: &str| -> String {
        let system = llm.system_of(id);
        let rest = system
            .split_once("reden mit: ")
            .unwrap_or_else(|| panic!("kein Prompt für '{id}': {system}"))
            .1;
        rest.split_once('.').unwrap().0.to_string()
    };

    // kette a-b-c: a nur mit b, b mit beiden.
    let llm = PerAgentLlm::new(script());
    let reg = registry(config(llm.clone(), &ws));
    call_swarm(&reg, spec(json!("kette")));
    assert_eq!(nachbarn(&llm, "a"), "b");
    assert_eq!(nachbarn(&llm, "b"), "a, c");

    // stern mit a im Zentrum: a mit beiden, b nur mit a.
    let llm = PerAgentLlm::new(script());
    let reg = registry(config(llm.clone(), &ws));
    call_swarm(&reg, spec(json!("stern")));
    assert_eq!(nachbarn(&llm, "a"), "b, c");
    assert_eq!(nachbarn(&llm, "b"), "a");

    // mesh ist der Default (kein `topologie`-Feld): jeder kennt jeden.
    let llm = PerAgentLlm::new(script());
    let reg = registry(config(llm.clone(), &ws));
    call_swarm(&reg, spec(Value::Null));
    assert_eq!(nachbarn(&llm, "a"), "b, c");
    assert_eq!(nachbarn(&llm, "b"), "a, c");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn explizite_verbindungen_schlagen_das_preset() {
    let ws = workspace("kanten");
    let llm = PerAgentLlm::new(vec![
        ("a", vec![vec![Chunk::text("ok")]]),
        ("b", vec![vec![Chunk::text("ok")]]),
        ("c", vec![vec![Chunk::text("ok")]]),
    ]);
    let reg = registry(config(llm.clone(), &ws));

    call_swarm(
        &reg,
        json!({
            "auftrag": "Hallo.",
            "topologie": "mesh",
            "verbindungen": [["a", "c"]],
            "max_laufzeit_s": 1,
            "agenten": [{"id": "a"}, {"id": "b"}, {"id": "c"}],
            "erforderliche_zustimmungen": 2
        }),
    );
    assert!(
        llm.system_of("a").contains("reden mit: c."),
        "{}",
        llm.system_of("a")
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Die Initialaufgabe kommt von der Laufzeit ("runtime"), die in keiner
/// PeerDirectory steht — ein `swarm_reply` darauf KANN nicht ankommen. Genau das
/// war aber die erste Reaktion, zu der das Mitgliedsprotokoll aufforderte; das
/// Ergebnis war ein `nicht_erlaubt`, nach dem der Schwarm verstummte und in die
/// Laufzeitgrenze lief. Der Kickoff muss das Mitglied deshalb auf `swarm_send`/
/// `swarm_broadcast`/`swarm_propose` verweisen, und ein Reply darauf muss ein
/// erklärender Status sein statt einer Abweisung.
#[test]
fn kickoff_verweist_auf_nachbarn_statt_auf_swarm_reply() {
    let ws = workspace("kickoff");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "r1",
                    "swarm_reply",
                    r#"{"content":"Verstanden."}"#,
                )],
                vec![Chunk::text("ok")],
            ],
        ),
        ("b", vec![vec![Chunk::text("warte")]]),
    ]);

    let mut spec = kette_spec("Analysiere das Repo.");
    spec["max_laufzeit_s"] = json!(2);
    let (_out, events) = run_with_events(spec, llm.clone(), &ws);

    let antwort = tool_result_of(&events, "a", "swarm_reply");
    let v: Value = serde_json::from_str(&antwort).unwrap();
    assert_eq!(v["status"], "initialaufgabe_ohne_absender", "{antwort}");
    assert!(
        v["hinweis"].as_str().unwrap().contains("swarm_propose"),
        "{antwort}"
    );
    // Der Nachbar bleibt sichtbar — das Mitglied soll ja dorthin ausweichen.
    assert_eq!(v["erreichbar"], json!(["b"]), "{antwort}");

    // Und das Protokoll im System-Prompt nennt die Ausnahme ausdrücklich.
    let system = llm.system_of("a");
    assert!(system.contains("runtime"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------- Quorum und Limits

// Ein zu großes Quorum wäre nie erreichbar (der Vorschlagende stimmt nicht mit)
// und würde den Schwarm bis zur Laufzeitgrenze hängen lassen.
#[test]
fn quorum_wird_auf_die_moeglichen_stimmen_gedeckelt() {
    let ws = workspace("quorum");
    let llm = propose_and_vote("trotzdem fertig");
    let reg = registry(config(llm, &ws));

    let mut spec = kette_spec("Schließt ab.");
    spec["erforderliche_zustimmungen"] = json!(99);
    let out = call_swarm(&reg, spec);

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["zustimmungen"], 1);

    std::fs::remove_dir_all(&ws).ok();
}

/// Das Default-Quorum muss zur Topologie passen: ein Vorschlag geht nur an die
/// direkten Nachbarn. In der Kette a–b–c sieht `c` einen Vorschlag von `a` nie,
/// ein Quorum von 2 (alle anderen) wäre also unerfüllbar — der Schwarm lief stumm
/// bis zur Laufzeitgrenze. Maßgeblich ist der kleinste Knotengrad, hier 1.
#[test]
fn default_quorum_bleibt_in_einer_kette_erreichbar() {
    let ws = workspace("kettenquorum");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Kette fertig"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
        // c hängt am anderen Ende der Kette und bekommt den Vorschlag nie zu sehen.
        ("c", vec![vec![Chunk::text("warte")]]),
    ]);
    let reg = registry(config(llm, &ws));

    let out = call_swarm(
        &reg,
        json!({
            "auftrag": "Schließt ab.",
            "topologie": "kette",
            // Kurz, damit ein Rückfall auf das alte Verhalten schnell auffällt
            // statt den Test 900 s hängen zu lassen.
            "max_laufzeit_s": 5,
            "agenten": [{"id": "a"}, {"id": "b"}, {"id": "c"}]
        }),
    );

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["zustimmungen"], 1);
    assert_eq!(v["ergebnis"], "Kette fertig");

    std::fs::remove_dir_all(&ws).ok();
}

/// Stern-Topologie: der Nabe erreicht alle, ein Blatt nur den Nabe. Das Quorum
/// richtet sich nach dem schwächsten Mitglied (Grad 1) — dokumentierte Näherung,
/// siehe README "Bewusste Design-Entscheidungen". Hier festgehalten, damit die
/// Abschwächung sichtbar ist, falls sie später verschärft wird.
#[test]
fn stern_quorum_folgt_dem_schwaechsten_mitglied() {
    let ws = workspace("sternquorum");
    let llm = PerAgentLlm::new(vec![
        (
            "nabe",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Stern fertig"}"#,
                )],
                vec![Chunk::text("eingereicht")],
            ],
        ),
        (
            "blatt1",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("zugestimmt")],
            ],
        ),
        ("blatt2", vec![vec![Chunk::text("enthalte mich")]]),
    ]);
    let reg = registry(config(llm, &ws));

    let out = call_swarm(
        &reg,
        json!({
            "auftrag": "Schließt ab.",
            "topologie": "stern",
            "max_laufzeit_s": 5,
            "agenten": [{"id": "nabe"}, {"id": "blatt1"}, {"id": "blatt2"}]
        }),
    );
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    // EINE Stimme genügt, obwohl der Nabe zwei Nachbarn erreicht hätte.
    assert_eq!(v["zustimmungen"], 1);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn nachrichtenlimit_beendet_den_schwarm_mit_hinweis() {
    let ws = workspace("limit");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "s1",
                    "swarm_send",
                    r#"{"to":"b","content":"hallo","kind":"task"}"#,
                )],
                vec![Chunk::text("gesendet")],
            ],
        ),
        ("b", vec![vec![Chunk::text("nie erreicht")]]),
    ]);
    let mut cfg = config(llm, &ws);
    // Die Initialaufgabe verbraucht das einzige Budget; der erste Send scheitert.
    cfg.limits.max_messages = 1;
    let reg = registry(cfg);

    let out = call_swarm(&reg, kette_spec("Redet miteinander."));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "nachrichtenlimit", "{out}");
    assert!(
        v["hinweis"].as_str().unwrap().contains("Nachrichtenlimit"),
        "{out}"
    );
    assert_eq!(v["unzustellbar"], 1, "{out}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Ein erschöpftes Nachrichtenbudget ist eine BREMSE, kein Not-Aus: die bereits
/// zugestellte Arbeit läuft zu Ende, und weil `swarm_propose`/`swarm_vote`
/// budgetfrei sind, kann der Schwarm auch am Limit noch regulär abschließen.
/// Vorher warf die erste Zustellung über Budget alle laufenden Turns weg.
#[test]
fn am_nachrichtenlimit_ist_der_abschluss_noch_moeglich() {
    let ws = workspace("limitabschluss");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                // Läuft ins Limit (msg-2) — der Schwarm darf davon nicht sterben.
                vec![Chunk::tool(
                    0,
                    "s1",
                    "swarm_send",
                    r#"{"to":"b","content":"hallo"}"#,
                )],
                // … und schließt trotzdem ab (msg-3).
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Ergebnis trotz Limit"}"#,
                )],
                vec![Chunk::text("eingereicht")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-3","approve":true}"#,
                )],
                vec![Chunk::text("zugestimmt")],
            ],
        ),
    ]);
    let mut cfg = config(llm, &ws);
    // Die Initialaufgabe verbraucht das einzige Budget.
    cfg.limits.max_messages = 1;
    let reg = registry(cfg);

    let mut spec = kette_spec("Redet miteinander und schließt ab.");
    spec["max_laufzeit_s"] = json!(5);
    let out = call_swarm(&reg, spec);

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["ergebnis"], "Ergebnis trotz Limit", "{out}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Das Default-Quorum im Mesh war der Knotengrad `n-1`, also Einstimmigkeit:
/// ein einziges Mitglied, das sich enthält, machte den Konsens unmöglich und der
/// Schwarm lief zwangsläufig in ein Limit. Jetzt genügt die Mehrheit.
#[test]
fn mesh_erreicht_konsens_ohne_einstimmigkeit() {
    let ws = workspace("meshquorum");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Mesh fertig"}"#,
                )],
                vec![Chunk::text("eingereicht")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("ja")],
            ],
        ),
        (
            "c",
            vec![
                vec![Chunk::tool(
                    0,
                    "v2",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("ja")],
            ],
        ),
        // d enthält sich — früher hätte das den Konsens verhindert.
        ("d", vec![vec![Chunk::text("keine Meinung")]]),
    ]);
    let reg = registry(config(llm, &ws));

    let out = call_swarm(
        &reg,
        json!({
            "auftrag": "Schließt ab.",
            // Kurz, damit ein Rückfall auf Einstimmigkeit sofort auffällt.
            "max_laufzeit_s": 5,
            "agenten": [{"id": "a"}, {"id": "b"}, {"id": "c"}, {"id": "d"}]
        }),
    );

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    assert_eq!(v["zustimmungen"], 2, "{out}");
    assert_eq!(v["ergebnis"], "Mesh fertig", "{out}");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn laufzeitlimit_beendet_den_schwarm_mit_hinweis() {
    let ws = workspace("laufzeit");
    // Niemand schlägt je einen Abschluss vor -> nur die Laufzeit beendet den Lauf.
    let llm = PerAgentLlm::new(vec![("a", vec![vec![Chunk::text("und nun?")]])]);
    let reg = registry(config(llm, &ws));

    let out = call_swarm(
        &reg,
        json!({"auftrag": "Warte.", "max_laufzeit_s": 1, "agenten": [{"id": "a"}]}),
    );
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "laufzeitlimit", "{out}");
    assert!(
        v["hinweis"].as_str().unwrap().contains("Laufzeitgrenze"),
        "{out}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// --------------------------------------------------------------- Rollen/Skills

#[test]
fn rolle_liefert_system_prompt_tools_und_strategie() {
    let ws = workspace("rollen");
    let llm = PerAgentLlm::new(vec![("kritiker", vec![vec![Chunk::text("ok")]])]);
    let mut cfg = config(llm.clone(), &ws);
    cfg.roles = vec![AgentRole {
        name: "kritiker".to_string(),
        description: "Zerlegt Vorschläge.".to_string(),
        system: "Du bist ein gnadenloser Kritiker.".to_string(),
        tools: Some(vec!["read_file".to_string()]),
        strategy: Strategy::Plain,
    }];
    let reg = registry(cfg);

    call_swarm(
        &reg,
        json!({
            "auftrag": "Kritisiere.",
            "max_laufzeit_s": 1,
            "agenten": [{"id": "kritiker", "rolle": "kritiker"}]
        }),
    );

    let system = llm.system_of("kritiker");
    assert!(system.contains("gnadenloser Kritiker"), "{system}");
    // Strategy::Plain -> KEINE ReAct-Präambel im Prompt.
    assert!(!system.contains("Denke Schritt für Schritt"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn eigener_system_prompt_schlaegt_die_rolle() {
    let ws = workspace("prompt_vorrang");
    let llm = PerAgentLlm::new(vec![("x", vec![vec![Chunk::text("ok")]])]);
    let mut cfg = config(llm.clone(), &ws);
    cfg.roles = vec![AgentRole {
        name: "kritiker".to_string(),
        description: String::new(),
        system: "Rollen-Prompt".to_string(),
        tools: None,
        strategy: Strategy::React,
    }];
    let reg = registry(cfg);

    call_swarm(
        &reg,
        json!({
            "auftrag": "Los.",
            "max_laufzeit_s": 1,
            "agenten": [{"id": "x", "rolle": "kritiker", "system": "Eigener Prompt"}]
        }),
    );

    let system = llm.system_of("x");
    assert!(system.contains("Eigener Prompt"), "{system}");
    assert!(!system.contains("Rollen-Prompt"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn skills_werden_nur_auf_anforderung_eingeklinkt() {
    let ws = workspace("skills");
    let skills_dir = std::path::Path::new(&ws).join("skills").join("recherche");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL.md"),
        "---\nname: recherche\ndescription: Wie man sauber recherchiert.\n---\n\nErst lesen, dann schreiben.\n",
    )
    .unwrap();

    fn mit_skill(id: &'static str) -> (&'static str, Vec<Vec<Chunk>>) {
        (
            id,
            vec![
                vec![Chunk::tool(0, "k1", "list_skills", "{}")],
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"geprüft"}"#,
                )],
                vec![Chunk::text("fertig")],
            ],
        )
    }
    let llm = PerAgentLlm::new(vec![mit_skill("mit"), mit_skill("ohne")]);
    let mut cfg = config(llm.clone(), &ws);
    cfg.skills = Some(Skills::new(
        std::path::Path::new(&ws).join("skills").to_str().unwrap(),
    ));
    let reg = registry(cfg);

    call_swarm(
        &reg,
        json!({"auftrag": "Recherchiere.", "agenten": [{"id": "mit", "skills": true}]}),
    );
    assert!(
        llm.system_of("mit").contains("Skills"),
        "Skill-Hinweis fehlt im Prompt: {}",
        llm.system_of("mit")
    );

    call_swarm(
        &reg,
        json!({"auftrag": "Recherchiere.", "agenten": [{"id": "ohne"}]}),
    );
    assert!(
        !llm.system_of("ohne").contains("SKILL"),
        "Skills ohne Anforderung eingeklinkt: {}",
        llm.system_of("ohne")
    );

    std::fs::remove_dir_all(&ws).ok();
}

// --------------------------------------------------- Live-Sicht und Abbruch

// Der Schwarm-Verkehr muss im Event-Strom des Orchestrators auftauchen — sonst
// sieht der Nutzer im TUI minutenlang nichts.
#[test]
fn schwarm_verkehr_landet_auf_dem_agent_bus() {
    let ws = workspace("bus");
    let llm = propose_and_vote("Ergebnis steht.");
    let (mut agent, _run) = orchestrator(kette_spec("Einigt euch."), llm, &ws);

    let bus = EventBus::new();
    let rx = bus.subscribe();
    agent.run_on_bus("Los.", &bus, 0, None, "");
    let events: Vec<_> = rx.try_iter().collect();

    // 1. Gespiegelte Schwarm-Ereignisse unter dem Namen "schwarm".
    let schwarm: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.data {
            EventData::ToolResult { name, result } if name == "schwarm" => {
                Some((e.source.clone(), result.clone()))
            }
            _ => None,
        })
        .collect();
    let text = schwarm
        .iter()
        .map(|(s, r)| format!("[{s}] {r}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Abschluss-Vorschlag"), "{text}");
    assert!(text.contains("stimmt über"), "{text}");
    assert!(text.contains("Schwarm beendet: konsens"), "{text}");
    // Die Zeilen sind mit der Agent-ID getaggt (im TUI der [a]-Präfix).
    assert!(schwarm.iter().any(|(s, _)| s == "a"), "{text}");

    // 2. Die eigenen Events der Mitglieder tragen ihre Agent-ID als `source`.
    assert!(
        events.iter().any(|e| e.source == "b"
            && matches!(&e.data, EventData::ToolCall { name, .. } if name == "swarm_vote")),
        "kein tool_call von 'b' im Strom"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// Esc im TUI setzt den Stop-Knopf des laufenden Auftrags; der blockierende
// `swarm`-Aufruf muss das sehen, statt bis zur Laufzeitgrenze zu warten.
#[test]
fn abbruch_des_orchestrators_stoppt_den_schwarm() {
    let ws = workspace("abbruch");
    // Niemand schlägt ab -> ohne Abbruch liefe der Schwarm 60 s.
    let llm = PerAgentLlm::new(vec![("a", vec![vec![Chunk::text("warte")]])]);
    let spec = json!({"auftrag": "Warte lange.", "max_laufzeit_s": 60, "agenten": [{"id": "a"}]});
    let (mut agent, _run) = orchestrator(spec, llm, &ws);

    let cancel = agentkit::new_cancel();
    let flipper = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::SeqCst);
        })
    };

    let bus = EventBus::new();
    let rx = bus.subscribe();
    let started = Instant::now();
    agent.run_on_bus("Los.", &bus, 0, Some(&cancel), "");
    flipper.join().unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "Abbruch hat zu lange gedauert: {:?}",
        started.elapsed()
    );
    let ergebnis = swarm_result(&rx.try_iter().collect::<Vec<_>>());
    assert!(ergebnis.contains("abgebrochen"), "{ergebnis}");

    std::fs::remove_dir_all(&ws).ok();
}

/// IDs werden überall gleich behandelt: `pruefe` trimmt sie, also müssen auch
/// System-Prompt, `verbindungen` und `start_agent` die getrimmte Fassung sehen.
/// Vorher nannte der Prompt `' architekt '`, während der Schwarm den Agenten als
/// `architekt` führte — `swarm_send` an die Prompt-ID wäre ins Leere gelaufen.
#[test]
fn agent_ids_werden_ueberall_getrimmt() {
    let ws = workspace("trim");
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"fertig"}"#,
                )],
                vec![Chunk::text("eingereicht")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("zugestimmt")],
            ],
        ),
    ]);
    let reg = registry(config(llm.clone(), &ws));

    let out = call_swarm(
        &reg,
        json!({
            "auftrag": "Schließt ab.",
            "max_laufzeit_s": 5,
            "agenten": [{"id": "  a  "}, {"id": " b "}],
            "verbindungen": [[" a", "b "]],
            "start_agent": " a "
        }),
    );
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "konsens", "{out}");
    // Der Prompt nennt die getrimmte ID — sonst hätte PerAgentLlm gar nicht
    // gewusst, wen es skripten soll, und der Schwarm wäre ins Limit gelaufen.
    assert!(
        llm.system_of("a").contains("Deine Agent-ID ist 'a'."),
        "{}",
        llm.system_of("a")
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------ Frontend-Tools der Mitglieder

/// Ein dynamisch erzeugtes Mitglied baut seine Registry von Grund auf neu und
/// erbt deshalb NICHTS vom Orchestrator. `extra_member_tools` ist die Naht, über
/// die das Frontend seine eigenen Fähigkeiten (heute: der Wissensgraph) in jedes
/// Mitglied bekommt — mit dessen echter Agent-ID, damit der Autor stimmt.
#[test]
fn extra_member_tools_landen_mit_richtiger_id_in_jedem_mitglied() {
    let ws = workspace("membertools");
    let notizen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(0, "n1", "notiz", r#"{"text":"von a"}"#)],
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"fertig"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "b",
            vec![
                vec![Chunk::tool(0, "n2", "notiz", r#"{"text":"von b"}"#)],
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ]);

    let mut cfg = config(llm, &ws);
    let gesammelt = notizen.clone();
    cfg.extra_member_tools = Some(Arc::new(move |reg: &mut ToolRegistry, id: &str| {
        let id = id.to_string();
        let gesammelt = gesammelt.clone();
        reg.add(
            "notiz",
            "Testtool: hält einen Text fest.",
            json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            move |args: Value| {
                let text = args["text"].as_str().unwrap_or_default().to_string();
                gesammelt.lock().unwrap().push((id.clone(), text));
                Ok("notiert".to_string())
            },
        );
    }));

    let reg = registry(cfg);
    let out = call_swarm(
        &reg,
        json!({
            "auftrag": "Notiert etwas und schließt ab.",
            "topologie": "kette",
            "max_laufzeit_s": 5,
            "agenten": [
                {"id": "a", "system": "Du entwirfst."},
                {"id": "b", "system": "Du prüfst."}
            ]
        }),
    );
    let v: Value = serde_json::from_str(&out).expect("kein JSON");
    assert_eq!(v["status"], "konsens", "{out}");

    // Beide Mitglieder hatten das Tool — und jedes schrieb unter eigener ID.
    let mut notiert = notizen.lock().unwrap().clone();
    notiert.sort();
    assert_eq!(
        notiert,
        vec![
            ("a".to_string(), "von a".to_string()),
            ("b".to_string(), "von b".to_string())
        ]
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Das Ergebnis-Schema steht im Prompt JEDES Mitglieds — nicht nur beim
/// Startagenten.
///
/// Beobachtet in einem echten Lauf: zwei Mitglieder reichten je ihre EIGENE
/// Perspektive als Abschluss ein, statt einer gemeinsamen Synthese. Nicht aus
/// Nachlässigkeit — keines wusste, wie ein vollständiges Ergebnis aussieht. Ein
/// Mitglied hat von sich aus nur seinen Teil.
#[test]
fn ergebnis_schema_steht_im_prompt_jedes_mitglieds() {
    let ws = workspace("schema");
    // `a` muss `b` anstoßen: `PerAgentLlm` zeichnet den System-Prompt erst auf,
    // wenn ein Mitglied tatsächlich das Modell fragt. Ohne Nachricht bleibt `b`
    // untätig und der Test prüfte einen Prompt, den es nie gab.
    let llm = PerAgentLlm::new(vec![
        (
            "a",
            vec![
                vec![Chunk::tool(
                    0,
                    "s1",
                    "swarm_send",
                    r#"{"to":"b","content":"deine Sicht bitte"}"#,
                )],
                vec![Chunk::text("ok")],
            ],
        ),
        ("b", vec![vec![Chunk::text("ok")]]),
    ]);
    let reg = registry(config(llm.clone(), &ws));

    call_swarm(
        &reg,
        json!({
            "auftrag": "Analysiert aus zwei Sichten.",
            "max_laufzeit_s": 2,
            "agenten": [{"id": "a"}, {"id": "b"}],
            "ergebnis_schema": {
                "architektur": "…",
                "qualitaet": "…"
            }
        }),
    );

    for id in ["a", "b"] {
        let system = llm.system_of(id);
        assert!(
            system.contains("VOLLSTÄNDIG ausfüllen"),
            "Mitglied '{id}' kennt das Schema nicht:
{system}"
        );
        assert!(system.contains("architektur"), "{system}");
        assert!(system.contains("qualitaet"), "{system}");
    }

    std::fs::remove_dir_all(&ws).ok();
}

/// Ohne `ergebnis_schema` bleibt der Prompt unverändert — kein leerer Block,
/// keine Anweisung zu einem Schema, das es nicht gibt.
#[test]
fn ohne_ergebnis_schema_kein_block_im_prompt() {
    let ws = workspace("kein_schema");
    let llm = PerAgentLlm::new(vec![("a", vec![vec![Chunk::text("ok")]])]);
    let reg = registry(config(llm.clone(), &ws));

    call_swarm(
        &reg,
        json!({
            "auftrag": "Mach was.",
            "max_laufzeit_s": 2,
            "agenten": [{"id": "a"}]
        }),
    );

    let system = llm.system_of("a");
    assert!(!system.contains("VOLLSTÄNDIG ausfüllen"), "{system}");
    // Das Protokoll selbst ist trotzdem da.
    assert!(system.contains("swarm_propose"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Der Charakter ist die zweite Achse neben der Rolle — und steht am ENDE des
/// Prompts.
///
/// Die Position ist kein Zufall: davor liegen rund 2000 Zeichen Protokoll, und
/// was hinten steht, prägt ein kleines Modell stärker. Stünde der Charakter vor
/// der Rolle, verwässerte er sie.
#[test]
fn charakter_steht_am_ende_des_mitglieds_prompts() {
    let ws = workspace("charakter");
    let llm = PerAgentLlm::new(vec![("skeptiker", vec![vec![Chunk::text("ok")]])]);
    let reg = registry(config(llm.clone(), &ws));

    call_swarm(
        &reg,
        json!({
            "auftrag": "Prüfe das Repo.",
            "max_laufzeit_s": 2,
            "agenten": [{
                "id": "skeptiker",
                "system": "Du prüfst die Code-Qualität.",
                "charakter": "vorsichtig und risikoscheu; du siehst überall Fallstricke"
            }]
        }),
    );

    let system = llm.system_of("skeptiker");
    let rolle = system
        .find("Du prüfst die Code-Qualität.")
        .expect("Rolle fehlt");
    let charakter = system.find("So urteilst du:").expect("Charakter fehlt");
    assert!(
        charakter > rolle,
        "Charakter steht VOR der Rolle — das verwässert sie:\n{system}"
    );
    assert!(system.contains("risikoscheu"), "{system}");
    // Der Belegmaßstab bleibt trotz Charakter unangetastet.
    assert!(system.contains("Datei und Zeile"), "{system}");
    // Und die Divergenz ist ausdrücklich gewollt.
    assert!(system.contains("ticken bewusst anders"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Ohne `charakter` bleibt der Prompt unverändert — kein leerer Block.
#[test]
fn ohne_charakter_kein_block_im_prompt() {
    let ws = workspace("kein_charakter");
    let llm = PerAgentLlm::new(vec![("a", vec![vec![Chunk::text("ok")]])]);
    let reg = registry(config(llm.clone(), &ws));

    call_swarm(
        &reg,
        json!({
            "auftrag": "Mach was.",
            "max_laufzeit_s": 2,
            "agenten": [{"id": "a"}]
        }),
    );

    let system = llm.system_of("a");
    assert!(!system.contains("So urteilst du:"), "{system}");
    assert!(system.contains("swarm_propose"), "{system}");

    std::fs::remove_dir_all(&ws).ok();
}
