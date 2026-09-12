# agentkit-graph

Graphbasiertes Wissen für [agentkit](../agent_framework_rs): ein **Working Graph** für den
laufenden Auftrag, ein **Canonical Graph** für dauerhaftes Wissen, Provenance auf jeder
Aussage — als optionales Crate, ohne eine Zeile Änderung am Agent-Loop.

```text
agentkit          führt den LLM-/Tool-Loop aus
ctxman            verwaltet den Kontext EINES Agenten
agentkit-graph    speichert Wissen — dauerhaft und über Agenten hinweg
agentkit-swarm    verwaltet Agent-Actors und Peer-Kommunikation
```

> Der Graph speichert Wissen. Peer-Nachrichten fordern Handlungen an. Was davon im
> Modellkontext landet, entscheidet der Recall — nie der Graph selbst.

## In 30 Sekunden

```rust
use std::sync::Arc;
use agentkit::ToolRegistry;
use agentkit_graph::{register_graph_tools, GraphAccess, GraphStore};

let store = Arc::new(GraphStore::open("./data/graph")?);
// Autor, Lauf und Ziel-Scope setzt die Laufzeit — nie das Modell.
let access = GraphAccess::session("agentkit", "/pfad/zum/workspace", "session-4711");

let mut tools = ToolRegistry::new();
register_graph_tools(&mut tools, store.clone(), access);
// … tools an Agent::builder(llm).tools(tools) — fertig.
# Ok::<(), agentkit_graph::GraphError>(())
```

In der Executable reicht ein Flag:

```bash
agentkit --graph ./data/graph -w .            # lesen, merken, promotieren
agentkit --graph ./data/graph --graph-readonly  # nur lesen
```

## Das Modell

| Begriff | Was es ist |
|---|---|
| **Entity** | ein Knoten: eine Sache, über die etwas ausgesagt wird (mit Aliasen) |
| **Claim** | eine Kante: Subjekt – Prädikat – Objekt, mit Status, Konfidenz und Quellen |
| **Source** | woher die Aussage stammt: Agent, Lauf, Tool-Aufruf, Belegstelle |
| **Episode** | ein Ereignis („Agent X hat Y getan") — wird nie promotet, nie traversiert |

Zwei orthogonale Achsen:

- **Ebene** — wie belastbar: `working` (vorläufig) oder `canonical` (konsolidiert).
- **Scope** — wem es gehört: `workspace:…`, `session:…`, `swarm:…`, `agent:…`.

Sichtbarkeit läuft **ausschließlich** über den Scope. Ein privater Agent-Scope ist
technisch isoliert: was nicht in der Sicht steht, existiert für diesen Leser nicht —
auch nicht als Fehlermeldung.

Claim-Status: `observation` → `hypothesis` → `confirmed` (nur über Promotion) →
`superseded` (nur als deren Nebenwirkung). Ein Modell kann sich nicht selbst
kanonisieren: `graph_remember` akzeptiert ausschließlich `observation` und `hypothesis`.

## Lesen und Schreiben

```text
Leser A ─┐                       ┌─ Arc<GraphIndex> (Revision 41)
Leser B ─┼─ store.snapshot() ────┤   unveränderlich, sperrfrei traversierbar
Leser C ─┘                       └─ …

Schreiber ── Journal (JSONL) ── neuer Index ── Arc-Tausch (Revision 42)
```

- **Leser** halten einen Lock nur für die Dauer eines `Arc`-Klons. Danach läuft die
  Traversierung völlig sperrfrei auf einem unveränderlichen Snapshot — echte
  Snapshot-Isolation ohne Transaktionsklammer. Ein Schreiber bricht keinen laufenden
  Read ab, ein langer Read blockiert keinen Schreiber.
- **Schreiber** committen **synchron**: erst das Journal (dauerhaft), dann der Tausch
  (sichtbar). Kommt ein Receipt zurück, ist die Mutation für *jeden* Leser da.
  Read-your-writes braucht dadurch keine Wartemechanik, keinen Writer-Actor, keine
  Queue-Tiefe und kein Revisions-Timeout.

Die Revision ist eine store-weite, monoton steigende Zahl: Mutations-ID, Audit-Marke
und Aktualitätsmaß beim Ranking in einem — und im Gegensatz zu einer Uhrzeit
deterministisch.

## Retrieval

```text
Frage ──► Seeds (Alias exakt, sonst Token-Overlap inkl. Wortstamm)
       ──► 1–2 Hops ungerichtet
       ──► Ranking (Distanz, Konfidenz, Status, Sicht-Priorität, Aktualität, Overlap)
       ──► Token-Budget
       ──► <graph-context>
```

```text
<graph-context revision="41" claims="2">
[C-17] (MCP Client) --[nutzt]--> (stdio-Session)
  Status: confirmed | Vertrauen: 0.96 | Ebene: canonical
  Quellen: S-11, S-18
[C-21] (Parallele Tool-Aufrufe) --[verursachen]--> (Session-Konkurrenz)
  Status: observation | Vertrauen: 0.82 | Ebene: working
  Quellen: S-31
</graph-context>
```

Gleiche Revision + gleiche Sicht + gleiche Frage ⇒ **gleiche Reihenfolge**: sortiert
wird nach Score, dann numerischer ID — die Ordnung ist total, es gibt kein „zufällig
gleich".

## Tools

| Tool | Wirkung | Verfügbar wenn |
|---|---|---|
| `graph_search` | Frage → relevanter Ausschnitt mit Claim-IDs | immer |
| `graph_neighbors` | Umgebung einer Entity (1–2 Hops) | immer |
| `graph_evidence` | Quellen einer Aussage | immer |
| `graph_remember` | Beobachtung/Hypothese festhalten | Schreibziel gesetzt |
| `graph_promote` | belegte Aussage ins dauerhafte Wissen | Promotionsziel gesetzt |

**Kein Tool hat ein `scope`-, `agent`- oder `created_by`-Argument.** Autor, Lauf und
Ziel-Scope stecken im `GraphAccess`, den die Tool-Closure einschließt — ein Modell kann
sich nicht als jemand anderes ausgeben und nicht in fremde Scopes schreiben. Dasselbe
Prinzip wie `swarm_send` in agentkit-swarm, wo `from` immer aus dem Kontext kommt.

## Promotion

`graph_promote` verschiebt einen Claim samt seiner beiden Entities ins Promotionsziel
und setzt ihn auf `confirmed`. Geprüft wird: sichtbar, mindestens eine Quelle, noch
nicht dort, nicht bereits ersetzt. Widersprechende kanonische Aussagen (gleiches
Subjekt und Prädikat, anderes Objekt) werden **nicht überschrieben**, sondern als
`superseded` markiert und behalten einen Verweis auf die neue — die alte Aussage bleibt
als Evidenz lesbar.

Der Datensatz behält dabei seine ID — die Evidenz-Verknüpfung bleibt dieselbe.

Die Spur der Promotion steht **am Datensatz**: `promoted_from` hält den Scope fest,
aus dem er kam, `promoted_from_status`, was er vorher war (Beobachtung oder bloße
Vermutung). Die mitgewanderten Entities tragen dasselbe `promoted_from`, und zwar
vom ERSTEN Umzug — der Ursprung, nicht die letzte Zwischenstation.

Warum nicht einfach im Journal nachsehen? Weil das nur bis zur ersten Kompaktierung
trägt: die schreibt über `to_ops()` den aktuellen Index, und die Working-Zeile ist
danach fort. Ein Journal, das verdichtet wird, ist kein Audit-Trail — die Provenienz
muss die Verdichtung überleben, und das tut sie nur als Feld.

## Speicher

Ein `RwLock<Arc<GraphIndex>>` im Speicher, ein **OKF-Bundle** (Open Knowledge Format
v0.2) auf der Platte — ein Verzeichnis aus Markdown-Dokumenten mit YAML-Frontmatter,
kein Journal mehr. **Keine Speicher-Dependency** — kein SQLite, kein C-Compiler, reines
Rust (eigener Minimal-YAML-Adapter, siehe unten):

```text
<graph-dir>/
  index.md                     nur `okf_version: '0.2'` im Frontmatter
  working/
    index.md
    session-run-4711/
      index.md
      log.md
      episodes/
        index.md
        ep-1.md
      mcp-client-stdio.md
      session-mutex.md
  canonical/
    index.md
    workspace-agentkit-rs/
      index.md
      parallele-tool-aufrufe.md
      session-konkurrenz.md
```

Ein Dokument je Entity, `layer/scope` stecken als Verzeichnispfad (`<layer>/<scope-kind>-<scope-id>/`),
`log.md` und `episodes/` liegen je Scope daneben. Gekürztes Beispiel
(`canonical/workspace-agentkit-rs/parallele-tool-aufrufe.md`):

```markdown
---
type: thing
title: Parallele Tool-Aufrufe
status: stable
sources:
  - { author: agent:tester, id: S-1, resource: agentkit://test_run/run-4711/call_7, title: test_run }
agentkit:
  aliases: [parallele tool aufrufe]
  claims:
    - confidence: 0.82
      id: C-1
      object: E-2
      predicate: verursacht
      promoted_from: { id: run-4711, kind: session }
      promoted_from_status: observation
      sources: [S-1]
      status: confirmed
      verified:
        - { at: 2026-09-12T11:13:22Z, by: agent:tester }
  id: E-1
  layer: canonical
  scope: { id: agentkit-rs, kind: workspace }
---

# Aussagen

* **verursacht** → [Session-Konkurrenz](/canonical/workspace-agentkit-rs/session-konkurrenz.md) — confirmed · 0.82[^S-1]

# Quellen

[^S-1]: test_run — cargo test mcp:: — 2 Fehlschläge
```

Alles Graph-Eigene (Claims, IDs, Revisionen, Promotion-Herkunft) steckt unter dem einen
Extension-Key `agentkit:`; der Rest ist reguläres OKF-Frontmatter, das jeder fremde
OKF-Konsument versteht.

`GraphStore::open` erkennt drei Fälle: existiert `index.md` bereits, ist es ein
fertiges Bundle und wird direkt geladen. Sonst, wenn eine alte `graph.jsonl`
(Journal-Format aus der Zeit davor) existiert, wird sie **einmalig migriert** — abgespielt,
als Bundle neu geschrieben, dann nach `graph.jsonl.migriert` umbenannt (kein
Datenverlust, die alte Datei bleibt liegen). Sonst wird ein frisches, leeres Bundle
angelegt. `compact_journal()`/`journal_lines()` entfallen ersatzlos; an ihre Stelle
tritt `rebuild_bundle()` — schreibt den kompletten Bestand aus dem `GraphIndex` neu auf
die Platte, auf Zuruf statt automatisch bei Zeilen-Inflation (ein Bundle hat keine
Zeilen, die sich aufblähen könnten).

**Grenzen, ehrlich benannt:**

- Der Graph liegt vollständig im RAM, und jeder Commit baut den Index neu
  (Zeigerkopien der Datensätze, flache Kopie der Indizes — O(n)). Bis rund 10⁵ Claims
  ist das im einstelligen Millisekundenbereich und völlig unauffällig. Wer darüber
  hinaus muss, tauscht den Store gegen ein eingebettetes MVCC-Backend (redb wäre der
  reine-Rust-Kandidat) — dafür gibt es heute bewusst noch kein Trait.
- `open` liest jetzt **N Dateien statt einer** (ein Bundle mit vielen Entities ist viele
  kleine Dateisystemzugriffe statt eines sequentiellen Journal-Reads), und eine Mutation
  schreibt **bis zu drei Dokumente statt einer Zeile** (Objekt-Dokument, Subjekt-Dokument,
  betroffene `index.md`/`log.md`). Für die heutige Größenordnung (Benchmark-Läufe,
  einzelne Workspaces) ist das im einstelligen Millisekundenbereich; bei sehr vielen
  Entities pro Scope skaliert die Dateisystem-Last linear mit, nicht nur die RAM-Kopie.

## Bewusste Design-Entscheidungen

Dieses Crate ist **kein Port** — es gibt kein Python-/C#-Gegenstück. Das Design folgt
dem agentkit_graph-PRD, weicht aber an mehreren Stellen bewusst davon ab:

- **Kein SQLite/rusqlite** (PRD §29). `bundled` zieht die SQLite-Amalgamation und damit
  einen C-Compiler in ein Repo, das bewusst reines Rust ist (siehe die Begründung bei
  `pdf-extract` in agentkit) — und in den statischen musl-Build der Benchmark-Harness.
  Snapshot-Isolation, parallele Leser und Dauerhaftigkeit gibt es hier ohne diese
  Kosten; WAL-Mode wäre der umständlichere Weg zum selben Ziel.
- **Synchroner Commit statt Writer-Actor** (PRD §17.5, §19.2). Ein lokaler Commit
  kostet Mikrosekunden. Damit entfallen Actor, Queue, `queue_full`, `Accepted`-vs-
  `Committed` und `RevisionTimeout` ersatzlos — Read-your-writes ist eine Tatsache
  statt eines Features. Asynchron muss nur werden, was teuer ist (Konsolidierung), und
  die gibt es im MVP noch nicht.
- **Zwei Ebenen statt vier** (PRD §12). `SessionWorking`, `SwarmWorking` und
  `AgentPrivate` unterscheiden sich nur darin, *wem* sie gehören — das ist der Scope.
  Die Ebene trägt nur noch die Belastbarkeit. Das halbiert die Kombinatorik in Store,
  Sicht und Tests, ohne eine einzige Fähigkeit zu kosten.
- **Scope als `(kind, id)` statt acht Enum-Varianten** (PRD §13). Mandant und Workspace
  haben im Repo heute keinen Nutzer; jede Variante wäre eine Fallunterscheidung ohne
  Anwendungsfall. Neue Scope-Arten kosten so eine Konvention, keinen Code.
- **Vier Claim-Status statt neun** (PRD §15.4). `candidate`, `supported`, `contested`,
  `rejected` und `promoted` hätten im MVP keinen Erzeuger — ein Zustandsraum ohne
  Übergänge ist Dokumentation, kein Code.
- **Keine Traits ohne zweite Implementierung** (Coding-Guidelines §2). `GraphReader`,
  `GraphWriter`, `EntityResolver` und die Backend-Traits des PRD sind konkrete Typen
  bzw. Funktionen. Ein `Postgres`/`Neo4j`-Trait „für später" wird eingezogen, wenn das
  zweite Backend wirklich existiert.
- **Aliase im Entity-Datensatz** statt als eigene Datensatzart (PRD §15.2): im MVP hat
  kein Alias eigene Provenance oder Konfidenz.
- **Recall im Auftragstext statt als ctxman-Segment** (PRD §22/§23). `ManagedContext`
  hat heute keine öffentliche Naht für Fremdsegmente; sie einzuziehen wäre eine
  Änderung am Agent-Kern — genau das, was dieses Crate vermeiden soll. Der
  vorangestellte Ausschnitt funktioniert mit und ohne ctxman, im CLI wie im Schwarm.
  (ctxmans `Segment.kind` ist übrigens ein freier `String`, ein `SegmentKind::GraphContext`
  wäre also auch dort unnötig — die Integration bleibt aber Phase 2.)
- **Unscharfer Treffer über Wortstämme** (5 Zeichen). Rein exakter Token-Vergleich
  verfehlt auf Deutsch genau die Fragen, die ein Agent wirklich stellt („parallelen
  Tool-Aufrufen" vs. „Parallele Tool-Aufrufe"). Keine Embeddings, kein Volltextindex —
  dieselbe Linie wie agentkits `recall`.
- **FNV-1a statt SHA-256** für `content_hash`: Wiedererkennung von Quellen, keine
  Kryptografie — und keine `sha2`-Dependency.
- **Deutsche Beschriftungen, englische Wire-Werte.** Sprachkonvention des Repos für
  alles Nutzersichtbare; die Statuswerte bleiben englisch, weil das Modell exakt sie in
  `graph_remember` zurückgibt (dieselbe Linie wie `MessageKind` in agentkit-swarm).
- **`created_by` niemals aus Modellargumenten.** Die Tools haben schlicht kein Feld
  dafür (PRD §30.3/30.4 — hier strukturell statt per Prüfung).
- **OKF statt Eigenformat.** Ein Bundle ist lesbar, diffbar und von fremden Werkzeugen
  konsumierbar (z. B. dem Referenz-Validator aus okf-skills) — ein Eigenformat hätte
  dieselbe Funktion, aber keinen dieser Vorteile.
- **Eigener Minimal-YAML-Adapter statt einer YAML-Crate** (`src/okf/yaml.rs`). Hält die
  Null-Dependency-Eigenschaft und den musl-Build. Preis: bewusst eine enge Teilmenge —
  unbekannte Konstrukte sind ein harter Fehler statt eines stillen Verschluckens.
- **Ein Dokument je Entity**, Claims als echte Markdown-Links im Body plus
  maschinenlesbar unter dem EINEN Extension-Key `agentkit:`. Ein fremder OKF-Konsument
  sieht damit genau einen unbekannten Schlüssel, den Rest versteht er nativ.
- **Frontmatter ist die Wahrheit, der Body ist Beiwerk.** Der Body wird beim Laden
  ignoriert, damit ein Mensch dort redigieren darf, ohne den Wiederaufbau zu gefährden.
- **Keine Mehrdatei-Atomarität, aufgefangen über die Schreibreihenfolge:**
  Objekt-Dokumente werden vor Subjekt-Dokumenten geschrieben (ein Claim lebt im
  Subjekt-Dokument, also entsteht nie eine Referenz auf ein fehlendes Dokument); bei der
  Promotion wird erst am Ziel geschrieben, dann an der Quelle gelöscht; beim Laden
  gewinnt bei Konflikten die höhere `updated_revision`.
- **`layer`/`scope` am Claim-Eintrag, wenn sie von denen des Subjekts abweichen.** Kommt
  vor, wenn `write.rs::resolve_or_create` über die Sicht eine bereits kanonische Entity
  trifft — ohne diese Angabe käme eine vorläufige Beobachtung als kanonisch zurück: eine
  Selbst-Kanonisierung ohne `graph_promote`.
- **Dateinamen aus einer Whitelist** (`okf::bundle::slug`, nur `[a-z0-9-]`; reservierte
  und Windows-Gerätenamen bekommen ein `-doc`-Suffix). Entity-Namen kommen aus
  Modellargumenten, ein Pfad-Ausbruch muss strukturell unmöglich sein, nicht nur geprüft.

## Nicht im Umfang

Konsolidierungs-Worker, LLM-gestützte Entity-Resolution und -Summarization,
Promotion-Policies jenseits von „manuell", `support`/`contest`, Retention/TTL-Läufer,
Mandantenfähigkeit, ctxman-Segment-Integration, Gold-Set und Evaluations-Harness,
Postgres/Neo4j, Embeddings, verteilte Graphen.

Der Recall ist **lexikalisch**: er findet, was der Auftragstext benennt. Für alles
andere hat das Modell `graph_search` — und genau deshalb ist der automatische Recall
klein budgetiert (Default 12 Aussagen / 800 Tokens).

## Build & Test

```bash
cargo test  --manifest-path agentkit_graph/Cargo.toml                 # offline, kein Netz
cargo test  --manifest-path agentkit_graph/Cargo.toml --test promotion
cargo clippy --manifest-path agentkit_graph/Cargo.toml --all-targets
cargo run   --manifest-path agentkit_graph/Cargo.toml --example graph_agent
cargo run   --manifest-path agentkit_graph/Cargo.toml --example shared_swarm_graph
```

Kein Test berührt das Netz; die Agenten-Tests skripten `FakeLlm`. Die Testdateien lesen
sich als Spezifikation: `store.rs` (Schreibpfad), `parallel_reads.rs` (Nebenläufigkeit),
`retrieval.rs` (Suche und Ranking), `promotion.rs` (vorläufig → dauerhaft),
`tools.rs` (was das Modell darf), `agent.rs` (der Wrapper),
`okf_konformitaet.rs` (was der ECHTE Schreibpfad auf die Platte legt, geprüft gegen die
in Rust nachgebildeten Regeln des Referenz-Validators — inklusive der Warnungen, nicht
nur der drei harten Fehlerklassen).

Die Gegenprobe gegen die Referenz-Implementierung selbst braucht `uv` und Python und ist
deshalb bewusst KEIN `cargo test` — die Testsuite oben bleibt offline. Der erste Befehl
legt ein Demo-Bundle an (der Test ist `#[ignore]`, weil er außerhalb des
Temp-Verzeichnisses schreibt):

```bash
cargo test --manifest-path agentkit_graph/Cargo.toml --test okf_konformitaet -- --ignored
uv run --with pyyaml python /pfad/zu/okf-skills/skills/validate/scripts/okf_validate.py \
    agentkit_graph/target/okf-demo --strict
```

Erwartet: `✓ conformant — no issues`, Exit-Code 0.

## Einsatz im Schwarm

Alle Mitglieder eines Laufs bekommen denselben Schreib-Scope (`GraphAccess::swarm`) —
das *ist* das gemeinsame Arbeitsgedächtnis — aber jeweils ihre eigene Autor-ID. Ein
Tester legt einen Befund ab, der Developer liest ihn, ohne dass ihn jemand als
Nachricht durch dessen Kontext schicken muss. Die Nachricht signalisiert weiterhin den
Handlungsbedarf; der Graph trägt die Evidenz.

In der Executable passiert das automatisch: `agentkit --graph DIR` gibt die Graph-Tools
sowohl dem Orchestrator als auch jedem dynamisch erzeugten Schwarm-Mitglied
(`SwarmToolConfig::extra_member_tools`).
