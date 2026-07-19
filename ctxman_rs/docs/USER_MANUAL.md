# ctxman (Rust) — Entwicklerhandbuch

Ein praktisches Handbuch für Entwickler, die `ctxman` als Bibliothek in einen Agent-Host
(z. B. `agentkit`) einbauen oder eigenständig nutzen wollen. Es erklärt das mentale Modell,
alle Operationen der `ContextSession`, die Policy-Konfiguration und die Erweiterungspunkte
(Traits). Die API-Referenz liefert `cargo doc --open`; die verbindliche Semantik definiert
die Spezifikation des C#-Originals (`ctxman`-Repo, `docs/ctxman-spec.md` v0.2 — Verweise
hier als „Spec §x.y").

> Kurzformel: **ctxman verwaltet den LLM-Context wie ein Betriebssystem den Speicher.**
> Segmente statt Bytes, Watermarks statt OOM-Killer, Externalisierung statt Swap —
> und ein deterministischer Renderer, damit Prompt-Caches nie unnötig invalidieren.

---

## Inhalt

1. [Das mentale Modell](#1-das-mentale-modell)
2. [Schnellstart](#2-schnellstart)
3. [Die Bausteine: Session, Segment, Unit, Frame](#3-die-bausteine)
4. [Die Static-Region und der Epoch-Bump](#4-die-static-region-und-der-epoch-bump)
5. [Appenden: Working-Segmente](#5-appenden-working-segmente)
6. [Rendern: deterministischer Provider-Request](#6-rendern)
7. [Policies: das deklarative Regelwerk](#7-policies)
8. [Garbage Collection: Minor, Major, Emergency](#8-garbage-collection)
9. [Page Fault: expand_context_ref](#9-page-fault-expand_context_ref)
10. [Frames: Subagent-Aufrufe als Stack](#10-frames)
11. [Events: der Audit-Trail](#11-events)
12. [Persistenz: Snapshots und Blob Store](#12-persistenz)
13. [Erweiterungspunkte: die Traits](#13-erweiterungspunkte)
14. [Fehlerbehandlung](#14-fehlerbehandlung)
15. [Integration in einen Agent-Loop](#15-integration-in-einen-agent-loop)
16. [Beispiele und Tests](#16-beispiele-und-tests)

---

## 1. Das mentale Modell

Ein LLM-Context ist eine knappe Ressource. ctxman behandelt ihn als **first-class Ressource**
mit dem Vokabular der Speicherverwaltung (Spec §1):

| Speicherverwaltung | ctxman |
|---|---|
| Code-Segment (read-only) | **Static-Region**: System-Prompt, Tool-Defs, Skill-Index — epoch-versioniert, innerhalb einer Epoche immutable (I1) |
| Heap / Working Set | **Working-Region**: Messages, Tool-Ergebnisse, Skills, Ressourcen |
| Page (file-backed) | **refetchable Segment** (Skill-Content, MCP-Resource): kann verlustfrei neu geladen werden ⇒ Eviction ohne Backup |
| Page (dirty) → Swap | **Externalisierung**: großer Inhalt wandert in den Blob Store, im Context bleibt eine Summary + Referenz |
| Page Fault | **`expand_ref`**: das Modell fordert ausgelagerten Inhalt per Tool-Call zurück |
| GC-Generationen | **Minor GC** (billig, deterministisch) / **Major GC** (LLM-gestützte Compaction) |
| Speicherdruck-Schwellen | **Watermarks**: soft 0,60·B, hard 0,80·B, emergency 0,95·B (Spec §3.1) |

Zwei Grundsätze ziehen sich durch alles:

- **Determinismus (I4):** identischer Segment-Stand ⇒ byte-identischer Render-Output ⇒
  stabile Provider-Prompt-Caches. Deshalb kanonische Sortierung der Static-Region,
  strikte `seq`-Ordnung im Working Set und kanonisches JSON.
- **Nichts geht still verloren:** Eviction und Compaction sind Soft-Deletes (I3, Audit-Trail
  bleibt), Externalisierung ist verlustfrei (Page Fault holt alles zurück), und vor jeder
  lossy Compaction läuft die **Promotion** dauerhafter Fakten (Spec §3.3).

ctxman ruft **nie selbst das LLM des Agents auf** (Non-Goal N1). Auch die Compaction nutzt
ein separates, vom Host bereitgestelltes Backend (`CompactionModel`-Trait).

## 2. Schnellstart

```toml
# Cargo.toml des Hosts
[dependencies]
ctxman = { path = "../ctxman_rs" }            # Features: "http", "tiktoken" optional
```

```rust
use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::{AppendRequest, ContextSession, CtxmanServices, RenderOptions, StaticSegmentSpec};

fn main() -> Result<(), ctxman::CtxmanError> {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), CtxmanServices::default());

    // 1) Static-Region setzen — IMMER über den Epoch-Bump, nie per Append (I1).
    session.bump_static_epoch(vec![
        StaticSegmentSpec {
            kind: "system_prompt".into(),
            role: Some(Role::System),
            content: "Du bist ein hilfreicher Agent.".into(),
            source: Some("core".into()),
        },
        StaticSegmentSpec {
            kind: "tool_def".into(),
            role: None,
            content: r#"{"name":"suche","description":"Websuche"}"#.into(),
            source: Some("core".into()),
        },
    ])?;

    // 2) Konversation appenden.
    session.append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Hallo"))?;

    // 3) Rendern und den Request an den Provider schicken.
    let out = session.render(RenderOptions {
        provider: "anthropic".into(),
        scope: RenderScope::Path,
        turn_advance: true,
    })?;
    // out.request_fragment == { "system": …, "tools": […], "messages": […] }

    // 4) GC-Empfehlung befolgen.
    if out.recommended_gc.is_some() {
        session.run_minor_gc()?;
    }
    Ok(())
}
```

## 3. Die Bausteine

**`ContextSession`** besitzt den gesamten Zustand eines Agent-Laufs exklusiv: die `Session`
(Policy-Snapshot, `context_version`, `static_epoch`, `current_turn`), alle `Segment`e, die
`Frame`s und das Event-Log. Alle Operationen sind synchrone `&mut`-Methoden — die
Exklusivität des Borrow-Checkers ersetzt die optimistic concurrency der Datenbank des
C#-Services; die `context_version` bleibt als beobachtbare Monotonie-Garantie erhalten.

**`Segment`** (Spec §2.2) ist das atomare Element: `kind` (offenes Vokabular, §2.3), `region`
(Static|Working), `role`, `content` XOR `blob_ref`, `seq` (stabile Render-Ordnung),
`tokens` (beim Append gezählt), `state` (live → externalized | compacted | evicted),
`pinned`, `refetchable`/`origin`, `tool_call_id`, `frame_id`. Zustandsübergänge laufen
ausschließlich über Guard-Methoden, die die Invarianten I1–I3 erzwingen.

**Units** (Spec §2.4): ein `tool_call` und sein `tool_result` (gekoppelt über
`tool_call_id`) bilden eine untrennbare Einheit. GC und Compaction operieren auf Units —
so entstehen nie verwaiste `tool_use`-Blöcke, die die Provider-API ablehnen würde (I5).

**Frames** (Spec §2.5): Subagent-Aufrufe als Stack — siehe [Abschnitt 10](#10-frames).

**Multi-Session:** `CtxmanStore` verwaltet mehrere `ContextSession`s per ULID
(`create_session` / `session_mut` / `remove`).

## 4. Die Static-Region und der Epoch-Bump

Die Static-Region (System-Prompt, `tool_def`s, `skill_index`) ist innerhalb einer Epoche
**immutable** (I1): kein Append, kein Evict, kein Pin. Jede Änderung — z. B. ein MCP-Server
kommt hinzu oder fällt weg — läuft über:

```rust
let outcome = session.bump_static_epoch(new_static)?;
// outcome.static_epoch   → neue Epoche (bewusste, auditierbare Cache-Invalidierung)
// outcome.diff           → added/removed tools und sources
```

Der Bump ersetzt die komplette Static-Region, berechnet den Diff und wendet auf die
Working-Units entfernter Tools die Policy `on_tool_removed` an (Spec §4.2):

| Wert | Wirkung |
|---|---|
| `keep` | Units bleiben unverändert |
| `externalize` *(Default)* | `tool_result`s der betroffenen Units wandern in den Blob Store |
| `evict` | ganze Units werden evicted |

Warum so streng? Der `cache_prefix_hash` (Spec §6) über den kanonisch sortierten
Static-Prefix macht Cache-Stabilität messbar: ändert er sich bei gleicher `static_epoch`,
ist der Determinismus verletzt. Ein Toggle-Off/On derselben Tool-Quelle erzeugt wieder den
byte-identischen Prefix — der Provider-Cache greift wieder.

## 5. Appenden: Working-Segmente

```rust
// Kurzform für Inline-Content:
session.append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Hallo"))?;

// Volle Form (z. B. Tool-Roundtrip, Batch — EIN Versions-Increment pro Aufruf):
session.append_segments(vec![
    AppendRequest {
        tool_call_id: Some("call_1".into()),
        source: Some("suche".into()),                  // Tool-Name (für Render + Epoch-Diff)
        ..AppendRequest::inline("tool_call", Some(Role::Assistant), r#"{"q":"rust"}"#)
    },
    AppendRequest {
        tool_call_id: Some("call_1".into()),
        ..AppendRequest::inline("tool_result", Some(Role::Tool), &ergebnis)
    },
])?;

// Refetchable Content (Clean Page — Eviction ohne Blob-Backup, Spec §3.2.1):
session.append_segment(AppendRequest {
    refetchable: true,
    origin: Some("skill://review-checklist".into()),
    ..AppendRequest::inline("skill_content", Some(Role::User), &skill_text)
})?;

// Sehr großer Inhalt (> 1 MiB inline verboten): direkt externalisiert appenden.
session.append_segment(AppendRequest {
    content: ctxman::AppendContent::Blob {
        content: riesige_bytes,
        content_type: "text/plain".into(),
        summary: Some("Logfile vom 19.07., 2 MB, 3 Fehler".into()),
    },
    ..AppendRequest::inline("tool_result", Some(Role::Tool), "")
})?;
```

Regeln (Spec §4.3): `seq` vergibt die Bibliothek strikt monoton; Static-Kinds
(`system_prompt`, `tool_def`, `skill_index`) sind hier verboten (nur Epoch-Bump);
Segmente erben automatisch den obersten offenen Frame; `pinned: true` macht ein Segment
für jede GC unantastbar (Tasks, Entscheidungen).

## 6. Rendern

```rust
let out = session.render(RenderOptions {
    provider: "anthropic".into(),   // oder "openai"
    scope: RenderScope::Path,       // Default; RenderScope::Frame = isolierte Subagent-Sicht
    turn_advance: true,             // genau dann true, wenn danach WIRKLICH ein Model-Call folgt
})?;
```

`RenderOutput`:

| Feld | Bedeutung |
|---|---|
| `request_fragment` | Provider-Request: Anthropic `{system, tools[], messages[]}`, OpenAI `{tools[], messages[]}` |
| `canonical_json` | dieselben Bytes kanonisch serialisiert (I4 — byte-stabil) |
| `builtin_tools` | das `expand_context_ref`-Tool im Provider-Schema (in die Tool-Liste aufnehmen!) |
| `cache_breakpoints` | empfohlene Cache-Marken (Anthropic: Ende der Static-Region) |
| `tokens_total`, `watermark` | Budget-Stand und Stufe ok/soft/hard/emergency |
| `recommended_gc` | `Some(Minor)` bei soft, `Some(Major)` bei hard — der Host führt aus |
| `cache_prefix_hash` | Determinismus-Messpunkt (Spec §6) |

Wichtig: **ein Turn = ein Model-Call** (Spec §3). Ein Tool-Loop mit fünf Model-Calls altert
TTLs um fünf Turns — beabsichtigt: Tool-Results veralten relativ zur Aufmerksamkeit des
Modells. Renders ohne folgenden Model-Call (Debugging, Preview) mit `turn_advance: false`.

Fehlerfälle: offene Units ⇒ `CtxmanError::IncompleteUnits` (erst jedes `tool_call` mit
einem `tool_result` schließen — notfalls mit synthetischem Error-Result, Spec I5);
Budget auch nach Emergency-GC ≥ B ⇒ `CtxmanError::BudgetExceeded` (retrybar: erst
`run_minor_gc`/`run_major_gc` laufen lassen, dann erneut rendern).

## 7. Policies

Die Policy ist **Konfiguration, kein Code** (Spec §5) und wird bei Session-Erstellung als
Snapshot eingefroren. `PolicyConfig::default_policy()` liefert die Spec-Defaults
(Budget 180 000, Watermarks 0,60/0,80/0,95, Externalisierungs-Schwelle 2 000 Tokens, das
Kind-Vokabular aus §2.3). Anpassung per Feldzugriff vor Session-Erstellung:

```rust
let mut policy = PolicyConfig::default_policy();
policy.budget_tokens = 32_000;
policy.externalize_threshold_tokens = 500;
policy.kinds.insert("mein_kind".into(), KindPolicy {
    ttl_turns: Some(5),      // None = ∞ (nie TTL-evicten)
    externalize: true,       // Kandidat für Blob-Auslagerung
    refetchable: false,
    promote: false,
});
```

`PolicyConfig` ist serde-serialisierbar (snake_case) — Policies lassen sich als
JSON/YAML-Dateien pflegen und einlesen. Unbekannte Kinds erhalten Working-Defaults
(keine TTL, keine Externalisierung).

## 8. Garbage Collection

Der Host steuert, wann GC läuft — `render` liefert die Empfehlung (`recommended_gc`):

**Minor GC** (`run_minor_gc()`, Spec §3.2) — deterministisch, billig, normative Reihenfolge
„billig vor teuer, lossless vor lossy":

1. **Clean-Page-Eviction:** refetchable Segmente jenseits ihrer Kind-TTL → evicted, ohne
   Blob-Write (die Quelle liegt außerhalb; `origin` bleibt im Audit-Trail).
2. **Externalisierung:** nicht-refetchable Live-Segmente mit `tokens >
   externalize_threshold_tokens` und Kind-Eignung → Inhalt in den Blob Store, im Context
   bleiben Summary + `expand_context_ref`-Hinweis. Bei gekoppelten Units nur das
   `tool_result` — der `tool_call` bleibt live.
3. **TTL-Eviction auf Unit-Ebene:** Units, deren definierte TTLs alle überschritten sind →
   evicted (beide Segmente). Pinned/Static blockiert die ganze Unit.

**Major GC** (`run_major_gc()`, Spec §3.3) — LLM-gestützt, braucht ein konfiguriertes
`CompactionModel` (+ `PromotionSink`, wenn Fakten anfallen):

1. **Promotion (zwingend zuerst):** Fact-Extraction über das Compaction-Fenster; extrahierte
   Fakten gehen an den `PromotionSink` (Webhook, Long-Term-Memory, …). Schlägt sie fehl,
   wird **nichts** kompaktiert — Faktenverlust ist strukturell ausgeschlossen.
2. **Compaction:** das Fenster (älteste nicht-gepinnte Working-Units bis `max_share` des
   Budgets) wird zu einem `compaction_summary`-Segment zusammengefasst, das die
   `seq`-Position der ältesten Quelle übernimmt (Chronologie bleibt). Quellen → compacted.

**Emergency** (automatisch in `render`, Spec §3.1): ab 0,95·B läuft synchron eine I/O-freie
Notfall-Collection (nur Phasen 1+3 — kein Blob-Write, kein LLM-Call im Hot Path). Reicht
das nicht, kommt `BudgetExceeded` zurück, ohne den Turn zu zählen.

## 9. Page Fault: `expand_context_ref`

Jeder Render liefert in `builtin_tools` die Definition des Tools
`expand_context_ref(segment_id)`. Nimm es in die Tool-Liste des Modells auf; ruft das
Modell es auf, beantwortet der Host den Call so:

```rust
let expanded = session.expand_ref(segment_id)?;   // ULID aus dem Tool-Call-Argument
// expanded.content               → der vollständige Inhalt aus dem Blob Store
// expanded.expansion_segment_id  → neues `ref_expansion`-Segment (sehr kurze TTL)
```

Der Zugriff setzt `last_referenced_turn` des Ursprungssegments — daraus entsteht
approximierte Liveness statt reiner Heuristik (Spec §3.4). Nicht mehr expandierbare
Segmente (evicted/compacted/Blob gesweept) liefern `CtxmanError::RefGone` mit der
bestmöglichen Restinformation (`summary`, `origin`).

## 10. Frames

Subagent-Aufrufe werden als Stack-Frames abgebildet (Spec §2.5):

```rust
let frame = session.push_frame("research_subtask");
// … Subagent arbeitet: alle Appends landen automatisch in diesem Frame …

// Isolierte Subagent-Sicht: Static + gepinnte Root-Segmente + Segmente des Tip-Frames.
let sub_view = session.render(RenderOptions {
    provider: "anthropic".into(),
    scope: RenderScope::Frame,
    turn_advance: true,
})?;

// Pop: Promotion der Frame-Fakten → Frame-Segmente evicted → Return-Segment im Parent.
let pop = session.pop_frame(frame, "Ergebnis: Die Bibliothek ist kompatibel.", None)?;
// pop.return_segment_id → kind="subagent_return" im Parent-Frame
```

LIFO ist strikt: ein Frame mit offenen Kind-Frames kann nicht gepoppt werden
(`FrameDiscipline`-Fehler) — Kinder zuerst. Der Zwischenstand des Subagents verschwindet
aus dem Context des Parents; nur das Return-Segment bleibt.

## 11. Events

Jede Mutation und jede GC-Operation emittiert ein Event (Spec §6): `segment_appended`,
`segment_externalized`, `segment_evicted`, `unit_evicted`, `compaction_started/completed`,
`fact_promoted`, `frame_pushed/popped`, `ref_expanded`, `static_epoch_bumped`,
`watermark_crossed`, `render_served` (mit `cache_prefix_hash`).

```rust
for event in session.drain_events() {          // interner Puffer, seq pro Session monoton
    println!("{} {}: {}", event.seq, event.event_type, event.payload);
}
```

Alternativ (oder zusätzlich) hört ein `EventSink` synchron mit — z. B. für Logging oder
Metriken (`CtxmanServices.event_sink`).

## 12. Persistenz

```rust
session.save_to_file(Path::new("session.json"))?;                 // JSON-Snapshot
let restored = ContextSession::load_from_file(path, services)?;   // Invarianten re-validiert
```

Der Snapshot enthält Session, Segmente, Frames und die Zähler — **nicht** die Blob-Inhalte:
die liegen content-addressed (sha256) im `BlobStore`. Mit dem `FileSystemBlobStore`
überleben beide zusammen einen Prozess-Neustart; der Cache-Prefix-Hash ist nach dem Laden
byte-identisch (getestet).

## 13. Erweiterungspunkte

Alle Dienste werden über `CtxmanServices` injiziert:

| Trait | Zweck | Mitgeliefert |
|---|---|---|
| `BlobStore` | content-addressed Ablage externalisierter Inhalte | `InMemoryBlobStore` (Default), `FileSystemBlobStore` |
| `TokenCounter` | Token-Zählung beim Append | `HeuristicTokenCounter` (chars/4, Default); `TiktokenCounter` (Feature `tiktoken`) |
| `CompactionModel` | Summarization + Fact-Extraction (Major GC, Frame-Pop) | `AnthropicCompactionModel` (Feature `http`); eigener Impl z. B. über agentkits `Llm` |
| `PromotionSink` | write-only Senke für promotete Fakten | `VecPromotionSink` (Tests), `WebhookPromotionSink` (Feature `http`) |
| `EventSink` | synchroner Event-Mithörer | — |

Eigenes CompactionModel in fünf Zeilen (Beispiel `examples/major_gc.rs` zeigt es komplett):

```rust
struct MeinModell;
impl ctxman::compaction::CompactionModel for MeinModell {
    fn summarize(&self, req: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        let text = mein_llm_call(&req.prompt_template_id, &req.window)?; // Host-Sache
        Ok(CompactionResult { summary: text })
    }
}
```

## 14. Fehlerbehandlung

Alle Operationen liefern `Result<_, CtxmanError>` — ein typisiertes Enum statt der
HTTP-Statuscodes des C#-Services:

| Variante | C#-Pendant | Typische Reaktion |
|---|---|---|
| `StaticRegionImmutable` | 409 | Epoch-Bump verwenden |
| `IncompleteUnits { open_tool_call_ids }` | 422 | offene Calls mit (synthetischem) `tool_result` schließen |
| `BudgetExceeded` | 413 | GC laufen lassen, dann erneut rendern (retrybar) |
| `FrameDiscipline` | 409 | Kind-Frames zuerst poppen |
| `RefGone { summary, origin }` | 410 | Restinfo nutzen; refetchable Inhalt neu laden |
| `ContentTooLarge` | 413 | `AppendContent::Blob` verwenden |
| `Compaction` / `Promotion` | 5xx | retrybar; bei Promotion-Fehler wurde nichts kompaktiert |

## 15. Integration in einen Agent-Loop

Das Muster für einen Host wie `agentkit` (heute: `ShortTermMemory` + `Llm`-Trait):

```text
pro Iteration des Agent-Loops:
  1. out = session.render(provider, Path, turn_advance = true)
  2. tools = eigene Tools + out.builtin_tools (expand_context_ref!)
  3. antwort = llm.complete(out.request_fragment.messages, tools)
  4. session.append_segments(assistant_msg / tool_call+tool_result aus der Antwort)
  5. bei Tool-Call expand_context_ref(id): session.expand_ref(id) und als Result zurückgeben
  6. match out.recommended_gc:
       Some(Minor) => session.run_minor_gc()
       Some(Major) => session.run_major_gc()   // CompactionModel über den Llm-Trait
       None => {}
```

Damit ersetzt ctxman `ShortTermMemory::compact` (lossy, in-place) durch budgetierte,
verlustarme GC — und `LongTermMemory` kann als `PromotionSink` andocken.

## 16. Beispiele und Tests

| Beispiel | Zeigt | Start |
|---|---|---|
| `examples/basic.rs` | Session, Static-Setup, Append, Render, Minor GC, Page Fault, Events | `cargo run --example basic` |
| `examples/frames.rs` | Subagent-Stack: push/pop, Frame-Scope-Render, LIFO, Return-Segment | `cargo run --example frames` |
| `examples/major_gc.rs` | eigenes CompactionModel + PromotionSink, Watermark → Major GC, Compaction-Ratio | `cargo run --example major_gc` |
| `examples/persistence.rs` | FileSystem-Blob-Store + Snapshot: Neustart mit identischem Cache-Prefix-Hash | `cargo run --example persistence` |

Die Integrationstests unter `tests/` sind bewusst als lesbare Verhaltens-Spezifikation
geschrieben (ein Testname = eine Regel) — `tests/orchestration.rs` ist der schnellste Weg,
das API-Verhalten im Detail zu verstehen. `tests/golden/` enthält die byte-genauen
Konformanz-Fixtures aus dem C#-Original.

```
cargo test                  # Kern, offline
cargo test --all-features   # + http/tiktoken
cargo doc --open            # API-Referenz
```
