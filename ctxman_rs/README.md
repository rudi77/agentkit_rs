# ctxman (Rust)

Rust-Port des **ctxman**-Cores — Context-Management für LLM-Agents als eigenständige,
synchrone Bibliothek. Portiert aus dem C#/.NET-9-Service (`ctxman`-Repo,
Spec `docs/ctxman-spec.md` v0.2); das Web-Interface (ASP.NET, EF Core/Postgres,
Auth/Tenancy) wurde bewusst nicht übernommen.

Mentales Modell: Speicherverwaltung. Die **Static-Region** (System-Prompt, Tool-Defs) ist
epoch-versioniert und innerhalb einer Epoche immutable; das **Working Set** (Messages,
Tool-Ergebnisse, Skills) wird von einem **Garbage Collector** bewirtschaftet —
Clean-Page-Eviction, Externalisierung in einen content-addressed Blob Store, TTL-Eviction
auf Unit-Ebene (Minor GC) sowie LLM-gestützte Compaction mit vorgelagerter Fact-Promotion
(Major GC). Die Render-Pipeline erzeugt deterministische, byte-stabile Provider-Requests
(Anthropic/OpenAI) — identischer Segment-Stand ⇒ identische Bytes ⇒ stabile Prompt-Caches.

ctxman ruft **nie** selbst das LLM des Agents auf (Non-Goal N1): Compaction/Promotion laufen
über die vom Host implementierten Traits `CompactionModel` und `PromotionSink`.

## Schnellstart

```rust
use ctxman::domain::{PolicyConfig, RenderScope, Role};
use ctxman::{AppendRequest, ContextSession, CtxmanServices, RenderOptions, StaticSegmentSpec};

let mut session = ContextSession::new(PolicyConfig::default_policy(), CtxmanServices::default());

// Static-Region nur über den Epoch-Bump (Spec §4.2, I1):
session.bump_static_epoch(vec![StaticSegmentSpec {
    kind: "system_prompt".into(),
    role: Some(Role::System),
    content: "Du bist ein hilfreicher Agent.".into(),
    source: Some("core".into()),
}])?;

session.append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Hallo"))?;

let out = session.render(RenderOptions {
    provider: "anthropic".into(),   // oder "openai"
    scope: RenderScope::Path,
    turn_advance: true,
})?;
// out.request_fragment  → { system, tools[], messages[] } für den Provider-Call
// out.recommended_gc    → Some(Minor|Major), wenn Watermarks überschritten sind
// out.cache_prefix_hash → Determinismus-Messpunkt (Spec §6)

session.run_minor_gc()?;            // Externalisierung + TTL-Eviction (Spec §3.2)
# Ok::<(), ctxman::CtxmanError>(())
```

## Dokumentation und Beispiele

- **Entwicklerhandbuch:** [`docs/USER_MANUAL.md`](docs/USER_MANUAL.md) — mentales Modell,
  alle Operationen, Policies, GC, Traits, Fehlerbehandlung, Agent-Loop-Integration.
- **API-Referenz:** `cargo doc --open` (deutsche rustdoc-Kommentare auf der gesamten
  Public API, mit `// Spec §x.y`-Verweisen an den Invarianten).
- **Beispiele** (`cargo run --example <name>`):

| Beispiel | Zeigt |
|---|---|
| `basic` | Session, Static-Setup, Append, Render, Minor GC, Page Fault, Events |
| `frames` | Subagent-Stack: push/pop, isolierte Frame-Sicht, LIFO-Disziplin, Return-Segment |
| `major_gc` | eigenes `CompactionModel` + `PromotionSink`, Watermark → Major GC, Promotion vor Compaction |
| `persistence` | FileSystem-Blob-Store + Snapshot: „Neustart" mit identischem Cache-Prefix-Hash |

- **Tests als Verhaltens-Spezifikation:** `tests/orchestration.rs` liest sich als
  Regelkatalog des API-Verhaltens; `tests/golden/` sind die byte-genauen
  Konformanz-Fixtures aus dem C#-Original.

## Features

| Feature | Inhalt |
|---|---|
| *(default)* | Kompletter Kern, offline, ohne HTTP/TLS |
| `http` | `AnthropicCompactionModel` + `WebhookPromotionSink` über ureq |
| `tiktoken` | `TiktokenCounter` (o200k/cl100k) als präziser `TokenCounter`; Default bleibt die chars/4-Heuristik |

## Konformität zum C#-Original

Die deterministische Kern-Logik (Domain-Invarianten I1–I5, `RenderPlanner`,
`MinorCollector`, `MajorCollector`, `UnitGrouping`, kanonisches JSON, Provider-Adapter,
Watermarks) ist ein verhaltenstreuer Port. Die Golden-Fixtures
`tests/golden/render-{anthropic,openai}.json` sind byte-identische Kopien aus dem C#-Repo
und werden byte-genau reproduziert (Konformanz-Orakel, Spec §4.6/I4).

## Bewusste Unterschiede zum C#-Original

- **Kein Service, kein HTTP-API**: Die Endpunkt-Logik (Render-Hot-Path, Append, Frames,
  Epoch-Bump, GC-Ausführung) lebt als Methoden auf `ContextSession`; HTTP-Statuscodes
  (409/413/422/…) werden zu typisierten `CtxmanError`-Varianten.
- **Single-Tenant, in-process**: `tenant_id`, Auth-Modi, Idempotency-Keys, Prometheus-
  Metriken, Worker/Queues, Advisory-Locks, Azure-Blob/Cold-Storage und Blob-Sweep entfallen.
  `&mut`-Exklusivität ersetzt die optimistic concurrency der DB; die `context_version`
  bleibt als beobachtbare Monotonie-Garantie erhalten (Spec §4.4).
- **GC on demand**: Statt Hintergrund-Workern liefert `render` eine
  `recommended_gc`-Empfehlung (soft ⇒ Minor, hard ⇒ Major); der Host ruft
  `run_minor_gc()`/`run_major_gc()` explizit. Die synchrone Emergency-Eviction im
  Render-Hot-Path (Spec §3.1) ist unverändert.
- **Synchron statt async**: alle C#-`async`-Interfaces (`IBlobStore`, `ICompactionModel`,
  `IPromotionSink`) sind synchrone Traits; kein tokio (Konvention von `agent_framework_rs`).
- **Persistenz**: EF Core/Postgres → JSON-Snapshots (`snapshot()`/`save_to_file`/
  `load_from_file`); Blob-Inhalte liegen weiterhin content-addressed im `BlobStore`.
- **Events**: Outbox-Tabelle → interner Puffer plus optionalem synchronem `EventSink`.
  `drain_events()` entnimmt, `events()`/`events_after(seq)` lesen ohne zu entnehmen
  (Cursor wie `GET /events?after_seq`). Für die Auditierbarkeits-Zusage der Spec (G6)
  reicht der Puffer nicht — wer den Verlauf behalten will, hängt eine dauerhafte Senke
  ein; `JsonlEventSink` schreibt den Strom append-only in eine Datei. Zusätzlich zum
  C#-Vokabular emittiert der Port `session_archived` (dort ein reines
  Live-Discovery-Signal, das nie in der Outbox landet — hier gibt es keinen zweiten
  Kanal). `blob_swept` entfällt mit dem nicht portierten Mark-and-Sweep (§7.1).
- **Kanonisches JSON**: `System.Text.Json` escapet non-ASCII/HTML-Zeichen als `\uXXXX`,
  serde_json emittiert rohes UTF-8. Die Golden-Fixtures sind rein ASCII und bleiben
  byte-identisch; für beliebige Inhalte gilt Intra-Bibliotheks-Determinismus (I4), nicht
  Byte-Parität mit C#. JSON-Keys der Render-Ausgabe müssen ASCII sein (Sortier-Parität).
- **Page Fault**: `expand_ref()` übernimmt zusätzlich die Client-SDK-Rolle aus Spec §3.4
  und hängt das Ergebnis direkt als `ref_expansion`-Segment an.
- **Append-Erweiterung**: `AppendRequest` kennt `refetchable`/`origin` (Spec §2.2), die der
  C#-Endpunkt nicht entgegennimmt; der Blob-Pfad nimmt rohe Bytes an und schreibt selbst in
  den `BlobStore` (statt separatem Upload-Endpunkt).
- **Frame-Guard**: ein bereits gepoppter Frame kann nicht erneut gepoppt werden
  (`FrameDiscipline`-Fehler) — im Service verhindern das Idempotency-Keys. Ohne
  konfiguriertes `CompactionModel` wird die Promotion beim Frame-Pop übersprungen.
- **Archivierung**: `archive()` führt wie das C#-Original die **terminale Promotion**
  (Spec §4.3) über alle verbliebenen Working-Segmente aus, bevor der Status wechselt —
  gepinnte eingeschlossen, denn `task`/`decision` landen nie in einem Compaction-Fenster.
  Schlägt sie fehl, wird nicht archiviert und der Fehler propagiert (C#: retrybarer
  `503 promotion_failed`); ein Retry ist sicher, weil noch nichts mutiert wurde. Statt
  der Idempotency-Keys des Service schützt ein Status-Guard vor dem zweiten Lauf.
- **Summary-Kürzung** (200 Zeichen + „…") zählt Unicode-Zeichen statt UTF-16-Code-Units;
  die Token-Heuristik zählt wie C# UTF-16-Code-Units (`encode_utf16`).
- **Zeit** als Unix-Millis (`i64`) mit injizierbarer Clock statt `DateTimeOffset`.
- **Tool-Paarung im Provider-Adapter**: Beide Adapter stellen vor der Ausgabe her, dass die
  Antwort unmittelbar auf ihren Aufruf folgt (`tool_calls` → `role: tool` bei OpenAI,
  `tool_use` → `tool_result`-Blöcke in der nächsten Nachricht bei Anthropic). Der Planer
  sortiert nur nach `seq` (I4) — das genügt nicht, wenn der Host eine verwaiste Unit heilt
  (I5) und das Platzhalter-Ergebnis die höchste `seq` bekommt. Ohne die Paarung lehnt der
  Provider den ganzen Request ab (bei agentkit: 10 von 64 Polyglot-Tasks, jedes Mal ein
  Totalausfall). Verlustfrei — eine Antwort ohne Aufruf bleibt an ihrem Platz.

## Tests

```
cargo test                  # Kern (offline), inkl. Golden-Byte-Vergleich
cargo test --all-features   # zusätzlich http-/tiktoken-Kompilate
cargo clippy --all-targets --all-features
```

`tests/features.rs` prüft die **Nähte zwischen den Bausteinen** (Frame-Scope für Sub-Agents,
Pin gegen beide GC-Stufen, Units unter Eviction und Verdichtung, Watermark-Leiter,
Snapshot-Neustart mit Blob, Page-Fault-Lebensverlängerung, `on_tool_removed`,
Session-Isolation) — die anderen Suiten decken die Bausteine je für sich ab. Der Fehler, der
die Polyglot-Tasks kostete, saß in keiner Einzelsuite, sondern dazwischen.
