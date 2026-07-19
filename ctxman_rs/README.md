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

Vollständiges Beispiel: `cargo run --example basic`.

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
- **Events**: Outbox-Tabelle → internes Log mit `drain_events()` plus optionalem
  synchronem `EventSink`. Event-Vokabular und Payloads folgen Spec §6.
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
- **Summary-Kürzung** (200 Zeichen + „…") zählt Unicode-Zeichen statt UTF-16-Code-Units;
  die Token-Heuristik zählt wie C# UTF-16-Code-Units (`encode_utf16`).
- **Zeit** als Unix-Millis (`i64`) mit injizierbarer Clock statt `DateTimeOffset`.

## Tests

```
cargo test                  # Kern (offline), inkl. Golden-Byte-Vergleich
cargo test --all-features   # zusätzlich http-/tiktoken-Kompilate
cargo clippy --all-targets --all-features
```
