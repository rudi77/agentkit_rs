//! ManagedContext — ctxman als Context-Manager des Agent-Loops (Feature `ctxman`).
//!
//! Ersetzt für lange Läufe die naive `compact()`-Strategie der [`ShortTermMemory`]
//! durch das volle ctxman-Modell (`../ctxman_rs`):
//!
//! - **Watermarks** statt harter Budget-Grenze: soft ⇒ Minor GC (große Tool-Ergebnisse
//!   werden verlustfrei in den Blob Store externalisiert), hard ⇒ Major GC
//!   (LLM-Compaction mit vorgelagerter Fact-Promotion), emergency ⇒ synchrone Eviction.
//! - **Page Fault**: der Agent bekommt das Tool `expand_context_ref` und kann
//!   ausgelagerte Inhalte gezielt zurückholen.
//! - **Promotion**: dauerhafte Fakten wandern VOR jeder lossy Compaction in eine
//!   JSONL-Datei im `LongTermMemory`-Format — eine spätere Session kann sie über
//!   `--memory` per `recall` wiederfinden.
//! - **Snapshot-Persistenz**: der komplette Kontext überlebt Prozess-Neustarts
//!   (`state_dir/snapshot.json` + content-adressierter Blob Store).
//!
//! ctxman ruft nie selbst ein LLM auf (Spec Non-Goal N1) — [`LlmCompactionModel`]
//! implementiert dessen `CompactionModel`-Trait über das [`Llm`]-Trait des Agenten.
//!
//! Bewusst KEIN eigener Ausführungspfad: der Agent-Loop bleibt die eine Schleife.
//! Ist ein `ManagedContext` gesetzt, liefert [`ManagedContext::messages`] die
//! Provider-Messages (deterministisch gerendert von ctxman) statt
//! `ShortTermMemory::messages`; alle Appends laufen zusätzlich hierher.

use crate::llm::Llm;
use crate::memory::LongTermMemory;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ctxman::compaction::{
    CompactionModel, CompactionRequest, CompactionResult, FACT_EXTRACTION_TEMPLATE_ID,
};
use ctxman::domain::{PolicyConfig, RenderScope, Role, SegmentState};
use ctxman::gc::GcLevel;
use ctxman::promotion::{PromotedFact, PromotionSink};
use ctxman::storage::FileSystemBlobStore;
use ctxman::tokenization::{HeuristicTokenCounter, TokenCounter};
use ctxman::{
    AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions, StaticSegmentSpec,
};

#[cfg(feature = "tiktoken")]
use ctxman::tokenization::TiktokenCounter;

/// Default-Tokenizer der Policy: mit Feature `tiktoken` die provider-genaue
/// o200k_base-Kodierung (GPT-4o/5-Familie), sonst die Zeichen-Heuristik. Der Name
/// steht als ehrliches Metadatum in der Policy UND wählt den echten Counter aus.
#[cfg(feature = "tiktoken")]
const DEFAULT_TOKENIZER: &str = "o200k";
#[cfg(not(feature = "tiktoken"))]
const DEFAULT_TOKENIZER: &str = "heuristic";

/// Ab dieser Inhaltslänge (Zeichen) werden `read_skill`-/`task`-Ergebnisse als
/// eigenes Kind-Segment angehängt (siehe [`ManagedContext::add_tool_result`]) —
/// kürzere Ergebnisse bleiben ein gewöhnliches `tool_result`.
const SPLIT_KIND_MIN_CHARS: usize = 400;

/// Konfiguration des [`ManagedContext`] (CLI/TUI: `--ctx DIR` / `--ctx-budget N` /
/// `--ctx-policy FILE` / `--ctx-compaction-model NAME`).
pub struct ManagedContextConfig {
    /// Zustandsverzeichnis: `snapshot.json`, `blobs/` und `facts.jsonl` liegen hier.
    pub state_dir: PathBuf,
    /// Modell-Kontext-Budget B in Tokens (Watermarks: 0,60·B / 0,80·B / 0,95·B).
    pub budget_tokens: u32,
    /// Ab dieser Größe wird ein Tool-Ergebnis externalisiert (Summary + Ref bleibt).
    pub externalize_threshold_tokens: u32,
    /// Ziel der Fact-Promotion (JSONL im `LongTermMemory`-Format). `None` ⇒
    /// `state_dir/facts.jsonl`. Zeigt sinnvollerweise auf die `--memory`-Datei,
    /// damit `recall` die Fakten in der nächsten Session findet.
    pub facts_path: Option<PathBuf>,
    /// Partielles JSON-Overlay über die Default-Policy (`--ctx-policy FILE`):
    /// nur die angegebenen Felder werden überschrieben, Objekte rekursiv gemergt
    /// (z. B. `{"kinds": {"tool_result": {"ttl_turns": 5}}}` lässt `externalize`
    /// unangetastet). Unbekannte Felder und inkonsistente Watermarks sind Fehler.
    pub policy_overlay: Option<Value>,
    /// Ehrliches Metadatum `compaction.model` in der Policy: welches Modell
    /// kompaktiert wirklich? `None` ⇒ "agent-llm" (das LLM des Agenten).
    pub compaction_model_label: Option<String>,
    /// Separates LLM nur für Compaction/Fact-Extraction
    /// (`--ctx-compaction-model`); `None` ⇒ das Agent-LLM übernimmt beides.
    pub compaction_llm: Option<Arc<dyn Llm>>,
}

/// Der volle GC-Durchgang: erst Major (Fakten-Promotion + verlustbehaftete
/// Compaction, braucht das LLM), danach Minor (verlustfreie Externalisierung) —
/// scheitert der Major, hilft der Minor immer noch. `true`, wenn einer von
/// beiden etwas bewegt hat.
///
/// Eine Funktion für beide Auslöser (Watermark in [`ManagedContext::messages`]
/// und `/compact` über [`ManagedContext::compact_now`]), damit die Reihenfolge
/// nicht an zwei Stellen gepflegt werden muss.
fn major_then_minor(s: &mut ContextSession) -> bool {
    // Nicht `.is_ok()`: ein No-op-Plan (< 2 Units im Fenster) liefert ebenfalls
    // `Ok`, nur mit leerem Report. Für „hat sich etwas bewegt?" zählen die
    // Felder — sonst meldete `/compact` auch dann Erfolg, wenn nichts geschah.
    let major = s
        .run_major_gc()
        .map(|r| r.summary_segment_id.is_some() || r.fact_promoted)
        .unwrap_or(false);
    let minor = s.run_minor_gc().map(|r| !r.is_empty()).unwrap_or(false);
    major || minor
}

impl ManagedContextConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        ManagedContextConfig {
            state_dir: state_dir.into(),
            budget_tokens: 100_000,
            externalize_threshold_tokens: 2_000,
            facts_path: None,
            policy_overlay: None,
            compaction_model_label: None,
            compaction_llm: None,
        }
    }
}

/// Implementiert ctxmans `CompactionModel` über das [`Llm`]-Trait des Agenten:
/// Summarization und Fact-Extraction sind gewöhnliche `complete()`-Aufrufe.
struct LlmCompactionModel {
    llm: Arc<dyn Llm>,
}

impl CompactionModel for LlmCompactionModel {
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        let digest: String = request
            .window
            .iter()
            .map(|w| {
                let kind = w.kind.as_deref().unwrap_or("segment");
                format!("[{kind}] {}\n", w.content)
            })
            .collect();
        let prompt = if request.prompt_template_id == FACT_EXTRACTION_TEMPLATE_ID {
            format!(
                "Extrahiere aus dem folgenden Agenten-Verlauf DAUERHAFT wichtige Fakten \
                 (Entscheidungen, Ergebnisse, gelernte Eigenheiten des Projekts) als kurze \
                 Stichpunkte. Gibt es keine, antworte mit einem leeren Text.\n\n{digest}"
            )
        } else {
            format!(
                "Fasse den folgenden Agenten-Verlauf kompakt zusammen (wichtige Fakten, \
                 Zwischenergebnisse, offene Punkte) — als Ersatz für den Originaltext im \
                 Kontext eines laufenden Agenten:\n\n{digest}"
            )
        };
        let reply = self
            .llm
            .complete(&[json!({"role": "user", "content": prompt})], None)
            .map_err(CtxmanError::Compaction)?;
        Ok(CompactionResult {
            summary: reply.content.unwrap_or_default().trim().to_string(),
        })
    }
}

/// Schreibt promotete Fakten als JSONL im `LongTermMemory`-Format (`{"text", "tags"}`)
/// — dieselbe Datei kann in einer späteren Session als `--memory` dienen.
struct FilePromotionSink {
    path: PathBuf,
}

impl PromotionSink for FilePromotionSink {
    fn write(&self, fact: &PromotedFact, _sink_url: &str) -> Result<(), CtxmanError> {
        // Denselben Append-Pfad nutzen wie `remember`: LongTermMemory hängt an die
        // JSONL-Datei an, ohne Bestehendes zu lesen oder umzuschreiben.
        let ltm = LongTermMemory::new(&self.path.to_string_lossy());
        ltm.remember(&fact.fact, vec!["ctxman".to_string(), fact.kind.clone()]);
        Ok(())
    }
}

/// Belegungs-Statistik einer Segment-Art (`system_prompt`, `tool_result`, …) für
/// die `/context`-Anzeige der Frontends, geliefert von
/// [`ManagedContext::segment_stats`].
pub struct SegmentStat {
    /// Segment-Art (offenes Vokabular, ctxman-Spec §2.2).
    pub kind: String,
    /// Anzahl lebender (live) Segmente dieser Art.
    pub count: usize,
    /// Token-Summe der lebenden Segmente (gezählt beim Append).
    pub tokens: u64,
    /// Anzahl in den Blob Store ausgelagerter Segmente (Inhalt per
    /// `expand_context_ref` zurückholbar).
    pub externalized: usize,
    /// Anzahl lossy kompaktierter Segmente (nur Summary im Kontext).
    pub compacted: usize,
}

/// Der von ctxman verwaltete Kontext eines Agenten. `Clone` über einen geteilten
/// `Arc<Mutex<ContextSession>>`-Kern, damit das `expand_context_ref`-Tool dieselbe
/// Session sieht wie der Loop (analog zu [`crate::planning::Plan`]).
#[derive(Clone)]
pub struct ManagedContext {
    session: Arc<Mutex<ContextSession>>,
    snapshot_path: PathBuf,
    /// true ⇔ die Session wurde aus einem Snapshot fortgesetzt (dann gilt die
    /// damals eingefrorene Policy, nicht die beim Start übergebene).
    resumed: bool,
    /// Effektiver Tokenizer-Name (bei Resume der aus der Snapshot-Policy).
    tokenizer: String,
}

impl ManagedContext {
    /// Baut den verwalteten Kontext: lädt einen vorhandenen Snapshot aus
    /// `state_dir/snapshot.json` (Resume) oder legt eine frische Session mit der
    /// konfigurierten Policy an (Default + `--ctx-budget` + `--ctx-policy`-Overlay,
    /// validiert). Bei Resume bleibt die damals eingefrorene Policy aktiv
    /// (ctxman-Spec: Policy ist ein Session-Snapshot) — inklusive Tokenizer.
    pub fn new(cfg: ManagedContextConfig, llm: Arc<dyn Llm>) -> Result<Self, String> {
        std::fs::create_dir_all(&cfg.state_dir).map_err(|e| e.to_string())?;
        let facts = cfg
            .facts_path
            .clone()
            .unwrap_or_else(|| cfg.state_dir.join("facts.jsonl"));
        let snapshot_path = cfg.state_dir.join("snapshot.json");
        let resumed = snapshot_path.exists();

        // Frische Session: Policy aus Default + Overrides bauen und validieren.
        // Resume: die eingefrorene Snapshot-Policy gilt — nur der Tokenizer-Name
        // wird vorab gelesen, um den passenden Counter zu injizieren.
        let policy = if resumed {
            None
        } else {
            Some(build_policy(&cfg)?)
        };
        let tokenizer = match &policy {
            Some(p) => p.tokenizer.clone(),
            None => frozen_tokenizer(&snapshot_path),
        };
        // Bei Resume kann ein Legacy-/Fremd-Name im Snapshot stehen ("claude" aus
        // der alten Default-Policy) — konservativer Fallback auf die Heuristik.
        let token_counter =
            token_counter_for(&tokenizer).unwrap_or_else(|| Box::new(HeuristicTokenCounter));

        // Compaction/Fact-Extraction: separates LLM, falls konfiguriert
        // (`--ctx-compaction-model`), sonst das Agent-LLM.
        let compaction_llm = cfg.compaction_llm.clone().unwrap_or_else(|| llm.clone());

        let services = CtxmanServices {
            blob_store: Box::new(FileSystemBlobStore::new(cfg.state_dir.join("blobs"))),
            token_counter,
            compaction_model: Some(Box::new(LlmCompactionModel {
                llm: compaction_llm,
            })),
            promotion_sink: Some(Box::new(FilePromotionSink { path: facts })),
            ..Default::default()
        };

        let session = match policy {
            None => ContextSession::load_from_file(&snapshot_path, services)
                .map_err(|e| format!("ctx-Snapshot nicht ladbar: {e}"))?,
            Some(policy) => ContextSession::new(policy, services),
        };

        Ok(ManagedContext {
            session: Arc::new(Mutex::new(session)),
            snapshot_path,
            resumed,
            tokenizer,
        })
    }

    /// true ⇔ aus einem Snapshot fortgesetzt — die Frontends weisen dann darauf
    /// hin, dass `--ctx-policy`/`--ctx-budget` erst eine NEUE Session betreffen.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Effektiver Tokenizer-Name ("heuristic", "o200k", "cl100k").
    pub fn tokenizer(&self) -> &str {
        &self.tokenizer
    }

    /// Setzt die Static-Region auf den System-Prompt. No-op, wenn er unverändert ist
    /// (jeder Epoch-Bump invalidiert Provider-Prompt-Caches — nur bewusst auslösen).
    pub fn set_system(&self, system: &str) -> Result<(), String> {
        let mut s = self.session.lock().unwrap();
        let current: Option<&str> = s
            .segments()
            .iter()
            .find(|seg| seg.kind() == "system_prompt")
            .and_then(|seg| seg.content());
        if current == Some(system) {
            return Ok(());
        }
        s.bump_static_epoch(vec![StaticSegmentSpec {
            kind: "system_prompt".to_string(),
            role: Some(Role::System),
            content: system.to_string(),
            source: Some("agentkit".to_string()),
        }])
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    /// Hängt die User-Nachricht (den Auftrag) an.
    pub fn add_user(&self, text: &str) {
        let mut s = self.session.lock().unwrap();
        let _ = s.append_segment(AppendRequest::inline("user_msg", Some(Role::User), text));
    }

    /// Hängt die Assistant-Antwort an: Text als `assistant_msg`, jeden Tool-Call als
    /// `tool_call`-Segment (Unit-Pairing über die `tool_call_id`).
    pub fn add_assistant(&self, content: Option<&str>, tool_calls: &[Value]) {
        let mut reqs: Vec<AppendRequest> = Vec::new();
        if let Some(text) = content {
            if !text.is_empty() {
                reqs.push(AppendRequest::inline(
                    "assistant_msg",
                    Some(Role::Assistant),
                    text,
                ));
            }
        }
        for tc in tool_calls {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            reqs.push(AppendRequest {
                tool_call_id: Some(id),
                source: Some(name),
                ..AppendRequest::inline("tool_call", Some(Role::Assistant), args)
            });
        }
        if reqs.is_empty() {
            return;
        }
        let mut s = self.session.lock().unwrap();
        let _ = s.append_segments(reqs);
    }

    /// Spielt einen bereits vorhandenen Verlauf (Provider-Messages, wie ihn
    /// [`crate::ShortTermMemory`] hält) in die Session ein.
    ///
    /// Gedacht für `--session` **zusammen mit** einem frischen `--ctx`: ohne
    /// das begänne der verwaltete Kontext bei null, obwohl der Spiegel den
    /// geladenen Verlauf trägt — das Modell hätte die Sitzung vergessen. Bei
    /// einer aus dem Snapshot fortgesetzten Session ist der Verlauf schon drin;
    /// dann wäre ein Replay eine Verdopplung, also passiert nichts.
    ///
    /// Der System-Prompt bleibt außen vor — den setzt `set_system` als
    /// Static-Region, nicht als Segment.
    pub fn replay(&self, messages: &[Value]) {
        if self.resumed {
            return;
        }
        // tool_call_id -> Tool-Name: die `tool`-Messages tragen den Namen nicht,
        // das Kind-Mapping in add_tool_result braucht ihn aber.
        let mut namen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for m in messages {
            match m.get("role").and_then(Value::as_str).unwrap_or("") {
                "user" => self.add_user(m["content"].as_str().unwrap_or("")),
                "assistant" => {
                    let calls: Vec<Value> = m
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for tc in &calls {
                        if let (Some(id), Some(name)) =
                            (tc["id"].as_str(), tc["function"]["name"].as_str())
                        {
                            namen.insert(id.to_string(), name.to_string());
                        }
                    }
                    self.add_assistant(m["content"].as_str().filter(|s| !s.is_empty()), &calls);
                }
                "tool" => {
                    let id = m["tool_call_id"].as_str().unwrap_or("");
                    let name = namen.get(id).map(String::as_str).unwrap_or("");
                    self.add_tool_result(id, name, m["content"].as_str().unwrap_or(""));
                }
                _ => {}
            }
        }
    }

    /// Hängt ein Tool-Ergebnis an (Gegenstück zum `tool_call` mit derselben ID).
    ///
    /// Kind-Mapping (Spec §2.3, offenes Vokabular): Ergebnisse mit eigener
    /// Lebenszyklus-Semantik — Skill-Anleitungen (`read_skill`) und
    /// Sub-Agent-Ergebnisse (`task`) — werden ab [`SPLIT_KIND_MIN_CHARS`] Zeichen
    /// in ZWEI Segmente zerlegt: ein kleines gepaartes `tool_result` (hält das
    /// tool_call/tool_result-Pairing I5 intakt — der Renderer schließt Units nur
    /// über Kind `tool_result`) plus ein eigenständiges `skill_content`- bzw.
    /// `task`-Segment, auf das die Kind-Policy wirkt: Skills sind refetchable
    /// (Clean-Page-Eviction nach TTL, das Modell lädt bei Bedarf per `read_skill`
    /// nach), Sub-Agent-Ergebnisse haben TTL ∞ (teuer zu reproduzieren).
    pub fn add_tool_result(&self, tool_call_id: &str, tool_name: &str, content: &str) {
        let split =
            if content.chars().count() >= SPLIT_KIND_MIN_CHARS && !content.starts_with("ERROR") {
                match tool_name {
                    "read_skill" => Some(("skill_content", true, "skill")),
                    "task" => Some(("task", false, "subagent")),
                    _ => None,
                }
            } else {
                None
            };

        let mut reqs: Vec<AppendRequest> = Vec::new();
        match split {
            None => reqs.push(AppendRequest {
                tool_call_id: Some(tool_call_id.to_string()),
                source: Some(tool_name.to_string()),
                ..AppendRequest::inline("tool_result", Some(Role::Tool), content)
            }),
            Some((kind, refetchable, origin)) => {
                reqs.push(AppendRequest {
                    tool_call_id: Some(tool_call_id.to_string()),
                    source: Some(tool_name.to_string()),
                    ..AppendRequest::inline(
                        "tool_result",
                        Some(Role::Tool),
                        &format!(
                            "[Inhalt folgt als eigenes Kontext-Segment (kind '{kind}') in der \
                             nächsten Nachricht]"
                        ),
                    )
                });
                reqs.push(AppendRequest {
                    source: Some(tool_name.to_string()),
                    refetchable,
                    origin: Some(origin.to_string()),
                    ..AppendRequest::inline(
                        kind,
                        Some(Role::User),
                        &format!("[{kind} — Ergebnis von '{tool_name}']\n{content}"),
                    )
                });
            }
        }
        let mut s = self.session.lock().unwrap();
        let _ = s.append_segments(reqs);
    }

    /// Rendert die Provider-Messages für den nächsten Modell-Call (OpenAI-Format)
    /// und führt dabei die Watermark-Aktionen aus: soft ⇒ Minor GC, hard ⇒ Major GC
    /// (Fallback Minor), Budget-Überschreitung ⇒ GC + zweiter Versuch. Verwaiste
    /// `tool_call`s (z. B. nach einem Abbruch mitten im Lauf) werden mit einem
    /// Platzhalter-Ergebnis geschlossen, statt den Render scheitern zu lassen.
    pub fn messages(&self) -> Result<Vec<Value>, String> {
        let mut s = self.session.lock().unwrap();
        let opts = |advance: bool| RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: advance,
        };

        let mut out = match s.render(opts(true)) {
            Ok(o) => o,
            Err(CtxmanError::IncompleteUnits { open_tool_call_ids }) => {
                let heal: Vec<AppendRequest> = open_tool_call_ids
                    .iter()
                    .map(|id| AppendRequest {
                        tool_call_id: Some(id.clone()),
                        ..AppendRequest::inline(
                            "tool_result",
                            Some(Role::Tool),
                            "(abgebrochen — kein Ergebnis geliefert)",
                        )
                    })
                    .collect();
                s.append_segments(heal).map_err(|e| e.to_string())?;
                s.render(opts(true)).map_err(|e| e.to_string())?
            }
            Err(CtxmanError::BudgetExceeded { .. }) => {
                // Notfall über der Emergency-Schwelle: gestaffelt — erst verlustfrei
                // externalisieren (Minor), nur wenn das nicht reicht lossy kompaktieren
                // (Major). Ein unbedingter Major GC würde hier sonst frisch Relevantes
                // wegfassen, obwohl die Externalisierung schon genügt hätte.
                let _ = s.run_minor_gc();
                match s.render(opts(true)) {
                    Ok(o) => o,
                    Err(CtxmanError::BudgetExceeded { .. }) => {
                        let _ = s.run_major_gc();
                        s.render(opts(true)).map_err(|e| e.to_string())?
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Err(e) => return Err(e.to_string()),
        };

        match out.recommended_gc {
            Some(GcLevel::Minor) => {
                if s.run_minor_gc().map(|r| !r.is_empty()).unwrap_or(false) {
                    out = s.render(opts(false)).map_err(|e| e.to_string())?;
                }
            }
            Some(GcLevel::Major) => {
                if major_then_minor(&mut s) {
                    out = s.render(opts(false)).map_err(|e| e.to_string())?;
                }
            }
            None => {}
        }

        // Event-Puffer leeren (Outbox-Semantik) — sonst wächst er unbegrenzt.
        let _ = s.drain_events();

        let messages = out.request_fragment["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(messages)
    }

    /// Kompaktiert auf Kommando (`/compact`), statt auf das Erreichen der
    /// Watermark zu warten. Führt exakt dieselbe GC-Folge aus wie der
    /// automatische Pfad in [`ManagedContext::messages`] — dieselbe Funktion,
    /// nicht bloß dieselbe Reihenfolge. `true`, wenn sich etwas bewegt hat.
    pub fn compact_now(&self) -> bool {
        let mut s = self.session.lock().unwrap();
        let bewegt = major_then_minor(&mut s);
        let _ = s.drain_events();
        bewegt
    }

    /// Aktueller Token-Stand laut ctxman (Summe der render-eligiblen Segmente).
    pub fn tokens_total(&self) -> i64 {
        let mut s = self.session.lock().unwrap();
        s.render(RenderOptions {
            provider: "openai".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        })
        .map(|o| o.tokens_total)
        .unwrap_or(0)
    }

    /// Belegung pro Segment-Art plus Token-Budget der Session-Policy — die
    /// Datenbasis der `/context`-Anzeige. In die Token-Summe zählen nur lebende
    /// Segmente (für ausgelagerte/kompaktierte steht im Kontext nur noch Summary
    /// bzw. Referenz-Hinweis); evictete Segmente fehlen ganz. Die Reihenfolge ist
    /// die des ersten Auftretens (Static zuerst — also der System-Prompt vorn).
    pub fn segment_stats(&self) -> (Vec<SegmentStat>, u32) {
        let s = self.session.lock().unwrap();
        let mut stats: Vec<SegmentStat> = Vec::new();
        for seg in s.segments() {
            let state = seg.state();
            if state == SegmentState::Evicted {
                continue;
            }
            let entry = match stats.iter_mut().find(|st| st.kind == seg.kind()) {
                Some(e) => e,
                None => {
                    stats.push(SegmentStat {
                        kind: seg.kind().to_string(),
                        count: 0,
                        tokens: 0,
                        externalized: 0,
                        compacted: 0,
                    });
                    stats.last_mut().unwrap()
                }
            };
            match state {
                SegmentState::Live => {
                    entry.count += 1;
                    entry.tokens += u64::from(seg.tokens());
                }
                SegmentState::Externalized => entry.externalized += 1,
                SegmentState::Compacted => entry.compacted += 1,
                SegmentState::Evicted => unreachable!(),
            }
        }
        let budget = s.session().policy().budget_tokens;
        (stats, budget)
    }

    /// Page Fault: holt ein externalisiertes Segment zurück. Weiche Fehler
    /// (`ERROR: …`), damit sich das Modell selbst korrigiert.
    pub fn expand(&self, segment_id: &str) -> String {
        let Ok(id) = segment_id.trim().parse::<ulid::Ulid>() else {
            return format!("ERROR: ungültige segment_id: {segment_id}");
        };
        let mut s = self.session.lock().unwrap();
        match s.expand_ref(id) {
            Ok(out) => out.content,
            Err(CtxmanError::RefGone { summary, .. }) => format!(
                "ERROR: Inhalt nicht mehr verfügbar (evicted/kompaktiert). \
                 Verbliebene Zusammenfassung: {}",
                summary.unwrap_or_else(|| "(keine)".to_string())
            ),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Bietet dem Agenten das `expand_context_ref`-Tool an (Page Fault, Spec §3.4).
    pub fn register_tool(&self, registry: &mut ToolRegistry) {
        let me = self.clone();
        registry.add(
            "expand_context_ref",
            "Holt einen ausgelagerten Kontext-Abschnitt vollständig zurück. Nutze die \
             segment_id aus dem Hinweis '[content externalized — expand_context_ref(...)]'.",
            json!({"type": "object",
                   "properties": {"segment_id": {"type": "string",
                       "description": "Die Segment-ID aus dem Externalisierungs-Hinweis."}},
                   "required": ["segment_id"]}),
            move |args: Value| {
                let sid = args.get("segment_id").and_then(Value::as_str).unwrap_or("");
                Ok(me.expand(sid))
            },
        );
    }

    /// Persistiert den kompletten Kontext als Snapshot (Resume beim nächsten Start).
    pub fn save(&self) -> Result<(), String> {
        let s = self.session.lock().unwrap();
        s.save_to_file(&self.snapshot_path)
            .map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------- Policy-Aufbau

/// Baut die Policy einer FRISCHEN Session: Spec-Default + agentkit-Overrides
/// (Budget, Threshold, ehrliche Metadaten) + optionales `--ctx-policy`-Overlay,
/// anschließend validiert. Die irreführenden Beispielwerte der Spec-Default-Policy
/// ("claude"-Tokenizer, "claude-haiku"-Compaction-Modell) werden hier durch die
/// tatsächliche Verdrahtung ersetzt.
fn build_policy(cfg: &ManagedContextConfig) -> Result<PolicyConfig, String> {
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = cfg.budget_tokens.max(1_000);
    policy.externalize_threshold_tokens = cfg.externalize_threshold_tokens.max(100);
    policy.tokenizer = DEFAULT_TOKENIZER.to_string();
    policy.compaction.model = cfg
        .compaction_model_label
        .clone()
        .unwrap_or_else(|| "agent-llm".to_string());
    // Die Senke ist eine lokale Datei — der Webhook-Platzhalter der Default-Policy
    // wäre irreführend im Snapshot.
    policy.promotion.sink.sink_type = "file".to_string();
    policy.promotion.sink.url = None;

    if let Some(overlay) = &cfg.policy_overlay {
        policy = merge_policy_overlay(policy, overlay)?;
    }
    validate_policy(&policy)?;
    Ok(policy)
}

/// Merged ein partielles JSON-Overlay über die Policy: Objekte rekursiv, alles
/// andere ersetzt. Unbekannte Feldnamen sind Fehler (Tippfehler dürfen nicht
/// still verpuffen); die `kinds`-Map ist offenes Vokabular — neue Kinds erlaubt.
fn merge_policy_overlay(policy: PolicyConfig, overlay: &Value) -> Result<PolicyConfig, String> {
    let Some(obj) = overlay.as_object() else {
        return Err("--ctx-policy: ein JSON-Objekt wird erwartet".to_string());
    };
    check_overlay_keys(obj)?;
    let mut base = serde_json::to_value(&policy).map_err(|e| e.to_string())?;
    deep_merge(&mut base, overlay);
    serde_json::from_value(base)
        .map_err(|e| format!("--ctx-policy passt nicht zum Policy-Schema: {e}"))
}

/// Rekursiver JSON-Merge: Objekt über Objekt feldweise, sonst ersetzt der
/// Overlay-Wert den Basis-Wert.
fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// Prüft die Overlay-Feldnamen gegen das Policy-Schema (Spec §5/§7.1). `kinds`
/// selbst ist offen (beliebige Kind-Namen), aber die Felder JE Kind sind fix.
fn check_overlay_keys(obj: &serde_json::Map<String, Value>) -> Result<(), String> {
    const TOP: [&str; 9] = [
        "budget_tokens",
        "watermarks",
        "externalize_threshold_tokens",
        "tokenizer",
        "kinds",
        "compaction",
        "promotion",
        "retention",
        "on_tool_removed",
    ];
    let check =
        |keys: &serde_json::Map<String, Value>, allowed: &[&str], wo: &str| -> Result<(), String> {
            for k in keys.keys() {
                if !allowed.contains(&k.as_str()) {
                    return Err(format!(
                        "--ctx-policy: unbekanntes Feld '{k}' in {wo} (erlaubt: {})",
                        allowed.join(", ")
                    ));
                }
            }
            Ok(())
        };
    check(obj, &TOP, "der Policy")?;
    if let Some(w) = obj.get("watermarks").and_then(Value::as_object) {
        check(w, &["soft", "hard", "emergency"], "watermarks")?;
    }
    if let Some(c) = obj.get("compaction").and_then(Value::as_object) {
        check(
            c,
            &["model", "prompt_template_id", "max_share"],
            "compaction",
        )?;
    }
    if let Some(kinds) = obj.get("kinds").and_then(Value::as_object) {
        for (kind, v) in kinds {
            if let Some(kp) = v.as_object() {
                check(
                    kp,
                    &["ttl_turns", "externalize", "refetchable", "promote"],
                    &format!("kinds.{kind}"),
                )?;
            }
        }
    }
    Ok(())
}

/// Validiert die fertige Policy: Watermark-Ordnung, `max_share`-Bereich und ein
/// auflösbarer Tokenizer-Name — Fehler hier verhindern das Anlegen der Session.
fn validate_policy(policy: &PolicyConfig) -> Result<(), String> {
    let w = &policy.watermarks;
    if !(w.soft > 0.0 && w.soft < w.hard && w.hard < w.emergency && w.emergency <= 1.0) {
        return Err(format!(
            "--ctx-policy: ungültige Watermarks (erwartet 0 < soft < hard < emergency ≤ 1): \
             soft={}, hard={}, emergency={}",
            w.soft, w.hard, w.emergency
        ));
    }
    let share = policy.compaction.max_share;
    if !(share > 0.0 && share <= 1.0) {
        return Err(format!(
            "--ctx-policy: compaction.max_share muss in (0, 1] liegen, ist {share}"
        ));
    }
    if token_counter_for(&policy.tokenizer).is_none() {
        let available = if cfg!(feature = "tiktoken") {
            "heuristic, o200k, cl100k"
        } else {
            "heuristic (o200k/cl100k brauchen das Cargo-Feature `tiktoken`)"
        };
        return Err(format!(
            "--ctx-policy: unbekannter tokenizer '{}' — verfügbar: {available}",
            policy.tokenizer
        ));
    }
    Ok(())
}

/// Wählt den Token-Counter zum Policy-Namen; `None` = nicht auflösbar (unbekannt
/// oder Feature `tiktoken` fehlt).
fn token_counter_for(name: &str) -> Option<Box<dyn TokenCounter>> {
    match name {
        "heuristic" => Some(Box::new(HeuristicTokenCounter)),
        #[cfg(feature = "tiktoken")]
        "o200k" => Some(Box::new(TiktokenCounter::o200k())),
        #[cfg(feature = "tiktoken")]
        "cl100k" => Some(Box::new(TiktokenCounter::cl100k())),
        _ => None,
    }
}

/// Liest den in einem Snapshot eingefrorenen Tokenizer-Namen vorab (die Dienste
/// müssen VOR dem Laden der Session gebaut werden — Henne-Ei). Fehler ⇒
/// "heuristic"; der eigentliche Load meldet Probleme dann sauber.
fn frozen_tokenizer(snapshot_path: &PathBuf) -> String {
    std::fs::read_to_string(snapshot_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v["session"]["policy"]["tokenizer"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "heuristic".to_string())
}
