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
use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::gc::GcLevel;
use ctxman::promotion::{PromotedFact, PromotionSink};
use ctxman::storage::FileSystemBlobStore;
use ctxman::{
    AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions, StaticSegmentSpec,
};

/// Konfiguration des [`ManagedContext`] (CLI: `--ctx DIR` / `--ctx-budget N`).
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
}

impl ManagedContextConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        ManagedContextConfig {
            state_dir: state_dir.into(),
            budget_tokens: 100_000,
            externalize_threshold_tokens: 2_000,
            facts_path: None,
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

/// Der von ctxman verwaltete Kontext eines Agenten. `Clone` über einen geteilten
/// `Arc<Mutex<ContextSession>>`-Kern, damit das `expand_context_ref`-Tool dieselbe
/// Session sieht wie der Loop (analog zu [`crate::planning::Plan`]).
#[derive(Clone)]
pub struct ManagedContext {
    session: Arc<Mutex<ContextSession>>,
    snapshot_path: PathBuf,
}

impl ManagedContext {
    /// Baut den verwalteten Kontext: lädt einen vorhandenen Snapshot aus
    /// `state_dir/snapshot.json` (Resume) oder legt eine frische Session mit der
    /// konfigurierten Policy an. Bei Resume bleibt die damals eingefrorene Policy
    /// aktiv (ctxman-Spec: Policy ist ein Session-Snapshot).
    pub fn new(cfg: ManagedContextConfig, llm: Arc<dyn Llm>) -> Result<Self, String> {
        std::fs::create_dir_all(&cfg.state_dir).map_err(|e| e.to_string())?;
        let facts = cfg
            .facts_path
            .clone()
            .unwrap_or_else(|| cfg.state_dir.join("facts.jsonl"));

        let services = CtxmanServices {
            blob_store: Box::new(FileSystemBlobStore::new(cfg.state_dir.join("blobs"))),
            compaction_model: Some(Box::new(LlmCompactionModel { llm })),
            promotion_sink: Some(Box::new(FilePromotionSink { path: facts })),
            ..Default::default()
        };

        let mut policy = PolicyConfig::default_policy();
        policy.budget_tokens = cfg.budget_tokens.max(1_000);
        policy.externalize_threshold_tokens = cfg.externalize_threshold_tokens.max(100);
        // Die Senke ist eine lokale Datei — der Webhook-Platzhalter der Default-Policy
        // wäre irreführend im Snapshot.
        policy.promotion.sink.sink_type = "file".to_string();
        policy.promotion.sink.url = None;

        let snapshot_path = cfg.state_dir.join("snapshot.json");
        let session = if snapshot_path.exists() {
            ContextSession::load_from_file(&snapshot_path, services)
                .map_err(|e| format!("ctx-Snapshot nicht ladbar: {e}"))?
        } else {
            ContextSession::new(policy, services)
        };

        Ok(ManagedContext {
            session: Arc::new(Mutex::new(session)),
            snapshot_path,
        })
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

    /// Hängt ein Tool-Ergebnis an (Gegenstück zum `tool_call` mit derselben ID).
    pub fn add_tool_result(&self, tool_call_id: &str, content: &str) {
        let mut s = self.session.lock().unwrap();
        let _ = s.append_segments(vec![AppendRequest {
            tool_call_id: Some(tool_call_id.to_string()),
            ..AppendRequest::inline("tool_result", Some(Role::Tool), content)
        }]);
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
                // Major GC braucht das LLM (Compaction) — schlägt er fehl, hilft
                // zumindest die verlustfreie Externalisierung des Minor GC.
                let done = s.run_major_gc().is_ok();
                let minor = s.run_minor_gc().map(|r| !r.is_empty()).unwrap_or(false);
                if done || minor {
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
