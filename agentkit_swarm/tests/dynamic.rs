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

use agentkit::coding::ApproveFn;
use agentkit::llm::{Chunk, ChunkStream, Llm, Message};
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
        Ok(Box::new(turn.into_iter()))
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
        workspace: ws.to_string(),
        approve: Some(Arc::new(|_: &str| true) as ApproveFn),
        shell_timeout: 30,
        skills: None,
        roles: Vec::new(),
        mcp: Arc::new(McpHub::empty()),
        limits: SwarmLimits::default(),
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
    assert!(v["nachrichten"].as_u64().unwrap() >= 2, "{out}");
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
