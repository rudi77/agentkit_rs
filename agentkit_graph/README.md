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

In der Executable ist das Gedächtnis ohne Flag an:

```bash
agentkit -w .                       # Projekt-Bundle in ./.agentkit/graph, Benutzer-Wissen dazu
agentkit --graph ./data/graph -w .  # Projekt-Bundle an anderem Ort
agentkit --graph-readonly           # nur lesen
agentkit --no-graph                 # aus für diesen Lauf ("graph": false in config.json: dauerhaft)
agentkit graph status               # ansehen und pflegen: list, show, forget, merge, rename, doctor
```

Das Projekt-Bundle entsteht erst, wenn zum ersten Mal etwas gemerkt wird. Vor jedem
Auftrag des Nutzers steht ein kleiner, als Daten gerahmter Recall; ein leerer Graph
kostet keinen Text. `--tools none` oder eine `--tools`-Liste ohne Graph-Tools schaltet
Tools und Recall gemeinsam ab.

## Das Modell

| Begriff | Was es ist |
|---|---|
| **Entity** | ein Knoten: eine Sache, über die etwas ausgesagt wird (mit Aliasen) |
| **Claim** | eine Kante: Subjekt – Prädikat – Objekt, mit Status, Konfidenz und Quellen |
| **Source** | woher die Aussage stammt: Agent, Lauf, Tool-Aufruf, Belegstelle |
| **Episode** | ein Ereignis („Agent X hat Y getan") — wird nie promotet, nie traversiert |

Zwei orthogonale Achsen:

- **Ebene** — wie belastbar: `working` (vorläufig) oder `canonical` (konsolidiert).
- **Scope** — wem es gehört: `workspace:…`, `session:…`, `swarm:…`, `agent:…`, `user:…`.

Sichtbarkeit läuft **ausschließlich** über den Scope. Ein privater Agent-Scope ist
technisch isoliert: was nicht in der Sicht steht, existiert für diesen Leser nicht —
auch nicht als Fehlermeldung.

Claim-Status: `observation` → `hypothesis` → `confirmed` (nur über Promotion) →
`superseded` (nur als deren Nebenwirkung) oder `retracted` (ausdrücklich
zurückgezogen, mit Grund). Ein Modell kann sich nicht selbst kanonisieren:
`graph_remember` akzeptiert ausschließlich `observation` und `hypothesis`, und
befördert wird nur, was eine fremde Quelle hat (siehe „Promotion").

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
- **Mehrere Prozesse** dürfen dasselbe Bundle offen haben. Jeder Commit läuft unter
  einer Sperrdatei (`graph.lock`), die nur für seine Dauer existiert, und gleicht
  vorher seinen Stand mit der Platte ab (`graph.stamp`) — zwei Prozesse vergeben
  so nie dieselbe ID. Die Commits eines anderen Prozesses sieht ein Store bei
  seinem nächsten eigenen Schreiben; ein reiner Leser behält seinen Stand, bis er
  neu öffnet. Lesen sperrt nie.

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
| `graph_compare` | Aussagen mehrerer Entities je Prädikat nebeneinander: gleich, abweichend, Lücke | immer |
| `graph_remember` | Beobachtung/Hypothese festhalten | Schreibziel gesetzt |
| `graph_promote` | belegte Aussage ins dauerhafte Wissen | Promotionsziel gesetzt |
| `graph_retract` | falsche Aussage mit Grund zurückziehen | Schreib- oder Promotionsziel gesetzt |

**Kein Tool hat ein `scope`-, `agent`- oder `created_by`-Argument.** Autor, Lauf und
Ziel-Scope stecken im `GraphAccess`, den die Tool-Closure einschließt — ein Modell kann
sich nicht als jemand anderes ausgeben und nicht in fremde Scopes schreiben. Dasselbe
Prinzip wie `swarm_send` in agentkit-swarm, wo `from` immer aus dem Kontext kommt.

## Promotion

`graph_promote` verschiebt einen Claim samt seiner beiden Entities ins Promotionsziel
und setzt ihn auf `confirmed`. Geprüft wird: sichtbar, mindestens eine **fremde**
Quelle, noch nicht dort, weder ersetzt noch zurückgezogen.

Fremd ist jede Quelle außer `agent_turn` — der Quelle, die eine Aussage automatisch
bekommt, wenn nur das Modell sie gemacht hat. Den Quellentyp setzt immer die
Laufzeit: `graph_remember` macht aus der Belegstelle (`excerpt`) genau dann eine
`tool_result`-Quelle, wenn sie **wörtlich** in einem Tool-Ergebnis dieses Laufs
steht (Leerraum zählt nicht, mindestens 12 Zeichen). Dafür schneidet
`register_graph_tools` die Ergebnisse der Tools mit, die bei seinem Aufruf schon
in der Registry stehen — deshalb gehört es ans Ende. Was es so nicht sehen kann,
reicht das Frontend über `GraphFederation::evidence` nach: Ergebnisse später
registrierter Tools (MCP-Server, `witness_tool`) und das, was der Nutzer sagt
(`witness_user`) — ein wörtliches Zitat des Nutzers ergibt eine
`user_statement`-Quelle und zählt ebenfalls. Sieht eine Belegstelle nach einem
Geheimnis aus (bekannte Schlüsselformate, `password = …`), wird ihr Wortlaut nicht
gespeichert; die Quelle bleibt. `durable: true` befördert im
selben Aufruf nur eine so belegte `observation`; eine Hypothese wird nie in dem
Aufruf dauerhaft, in dem sie entsteht. Widersprechende kanonische Aussagen (gleiches
Subjekt und Prädikat, anderes Objekt) werden **nicht überschrieben**, sondern als
`superseded` markiert und behalten einen Verweis auf die neue — die alte Aussage bleibt
als Evidenz lesbar.

Der Datensatz behält dabei seine ID — die Evidenz-Verknüpfung bleibt dieselbe.

## Zwei Bundles, eine Sicht

Wissen liegt an zwei Orten, jeweils als eigenständiges OKF-Bundle:

| Ebene | Ort | Scope |
|---|---|---|
| Sitzung (vorläufig) | im Projekt-Bundle | `working@session:<id>` |
| Projekt | `--graph DIR` | `canonical@workspace:project` |
| Benutzer | `<config_dir>/graph/` | `canonical@user:default` |

`GraphFederation` verbindet beide: gelesen wird aus beiden (Projekt vor Benutzer, ein
gemeinsames Budget), geschrieben, befördert und zurückgezogen nur im Projekt. Weil
beide Bundles ihre IDs selbst zählen, gibt es `C-17` zweimal — an der Tool-Grenze
trägt deshalb alles aus dem Benutzer-Bundle das Präfix `u:` (`u:C-17`), in den
Dateien steht es nie.

Auf die Benutzer-Ebene führt ein einziger Weg, und er gehört dem Menschen:

```bash
agentkit graph promote --user C-17
```

Das Kommando zeigt die Aussage samt Belegstellen, fragt nach und läuft nur an einem
Terminal. Übernommen wird nur Bestätigtes mit fremder Quelle; im Benutzer-Bundle
entsteht eine Kopie mit eigener ID, `promoted_from` und dem Menschen als `verified`.
Kein Tool kann das — sonst wanderte Wissen über ein Projekt in jedes andere. Das
Benutzer-Bundle entsteht erst mit der ersten Freigabe.

Das Projektwissen hängt am Bundle, nicht am Pfad des Checkouts: befördert wird in den
festen Scope `workspace:project`. Ein Bundle aus der Zeit pfadbasierter IDs
(`workspace:<absoluter Pfad>`) behält sein Wissen — diese Scopes kommen lesend in die
Sicht, umgeschrieben wird nichts.

## Lernen ohne Zutun des Modells

Während der Arbeit lernt der Agent nur, wenn das Modell `graph_remember` aufruft. Drei
Dinge geschehen deshalb ohne es:

- **Recall vor jedem Auftrag** (siehe oben).
- **Konsolidierung am Sitzungsende** (`consolidate`): EIN zusätzlicher Modellaufruf
  liest den fertigen Verlauf und schlägt Aussagen vor. Jede landet vorläufig im
  Sitzungs-Scope; befördert wird nur eine Beobachtung, deren Belegstelle wörtlich in
  einem Tool-Ergebnis des Verlaufs steht — geprüft von der Laufzeit, mit der echten
  `tool_call_id` als Quelle. Erst ab fünf Tool-Aufrufen, nie bei einer Stufe mit
  strukturierter Ausgabe, nie unter `--no-graph`. Weil sie einen Aufruf je Sitzung
  kostet, ist sie **nicht der Default**: `agentkit --consolidate` oder
  `"graph_consolidate": true` in `config.json`. Scheitert der Aufruf, steht eine
  graue Zeile auf stderr; die Sitzung endet normal.
- **Fakten des Kontext-Managements** (ctxman, `--ctx`): was beim Verdichten als Fakt
  festgehalten wird, landet zusätzlich als Episode im Sitzungs-Scope
  (`context_fact`) und geht der Konsolidierung als Material mit. Bewusst keine
  Aussage: ein Fakt ist Freitext und hat ein Modell als Autor. `facts.jsonl` bleibt.

Am Sitzungsende werden außerdem alte Arbeits-Scopes weggeräumt: von den vorläufigen
`session`- und `swarm`-Scopes bleiben die 20 jüngsten, damit `working/` nicht mit
jeder Sitzung wächst. Dauerhaftes bleibt immer.

## Fremde Bundles

Der Graph wird in künftige Prompts eingespielt und ist damit ein dauerhafter Kanal für
Prompt-Injection: ein präparierter Inhalt, einmal als „Wissen" gespeichert, käme in
jeder Sitzung wieder. Drei Regeln folgen daraus:

- Der automatische Recall ist als **Daten** gerahmt und sagt dem Modell, dass er keine
  Freigaben erteilt.
- Ein Projekt-Bundle am Default-Ort, das dieser Benutzer nicht selbst angelegt hat —
  ein geklontes Repository mit eingechecktem Graphen —, wird **nicht gelesen**, bis er
  es bestätigt: interaktiv per Rückfrage beim Start, sonst mit `agentkit graph trust`.
  Erkannt wird das eigene Bundle an `graph.owner`, einem Zeichen unter einem Schlüssel
  aus `<config_dir>/graph.key`. Ein ausdrücklich genanntes `--graph DIR` gilt ohne
  Prüfung: wer es nennt, will es.
- Ein gezeichnetes Bundle bringt eine `.gitignore` mit, die es aus dem Repository
  heraushält. Team-Wissen über Git ist bewusst nicht im Umfang (überlappende IDs).

## Verlernen

Das Modell zieht zurück, der Mensch löscht. `agentkit graph forget <ID>` entfernt eine
Aussage endgültig, `merge` führt zwei Entities zusammen, `rename` benennt um, `doctor`
prüft die Bundles auf Unstimmigkeiten. Diese Eingriffe fragen nach, laufen nur an
einem Terminal und schreiben das Bundle vollständig neu (`GraphStore::rewrite`).

`graph_retract` setzt einen Claim auf `retracted` und hält am Datensatz fest, wer ihn
wann und warum zurückgezogen hat. Er bleibt, wo er ist, und über `graph_evidence`
lesbar — dieselbe Idee wie `superseded`, nur ohne Nachfolger. Aus `graph_search`,
`graph_neighbors` und dem Recall fällt er heraus; wer dieselbe Aussage erneut macht,
bekommt einen neuen Claim. Zurückziehen darf, wer an der Stelle schreiben dürfte:
Vorläufiges im eigenen Schreibziel, Dauerhaftes im eigenen Promotionsziel.

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
  graph.stamp                  wechselt mit jedem Commit (siehe „Lesen und Schreiben")
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
- **Fünf Claim-Status statt neun** (PRD §15.4). `candidate`, `supported`, `contested`,
  `rejected` und `promoted` hätten im MVP keinen Erzeuger — ein Zustandsraum ohne
  Übergänge ist Dokumentation, kein Code. `retracted` kam mit `graph_retract` dazu.
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
- **Steuerzeichen werden immer quotiert, Tabs nur als Einrückung abgelehnt.** Der
  Emitter schrieb einen Tab in einem Namen oder einer Belegstelle roh und plain, der
  Parser lehnte Tabs überall ab — ein einziger solcher Wert machte das ganze Bundle
  beim nächsten Öffnen unlesbar. Jetzt gehen Steuerzeichen durchs Double-Quoting
  (`\t`, `\u…`); der Parser verbietet Tabs weiterhin als Einrückung und als Trenner
  hinter `:`, liest sie aber mitten in einem Wert — damit öffnen sich auch Bundles
  wieder, die der alte Emitter so geschrieben hat.
- **Ein Kommentar beginnt nur nach einem ASCII-Leerzeichen.** Emitter (quotiert bei
  ` #`) und Parser teilen damit eine Definition. Vorher galt beim Lesen jeder
  Unicode-Leerraum: `Issue #12` mit geschütztem Leerzeichen kam als `Issue` zurück.
- **Ein Commit schreibt auch Entities mit, deren Dateiname sich nur verschiebt.** Den
  Slug bekommt die kleinere ID. Wandert per Promotion eine ältere Entity in ein
  Verzeichnis, in dem eine jüngere denselben Slug hält (oder verlässt sie eines), ändert
  sich der Pfad der jüngeren, ohne dass sie in der Mutation vorkommt. Sie wird deshalb
  mitgeschrieben, und zwar in einer Reihenfolge, in der nie ein Dokument überschrieben
  wird, dessen Entity noch nicht an ihrem neuen Pfad steht — vorher überschrieb die
  ältere das Dokument der jüngeren, und deren Aussagen fehlten nach dem Neustart.
- **`graph_evidence` achtet die Sicht** und antwortet für eine Aussage außerhalb der
  Sicht wortgleich wie für eine, die es nicht gibt. Claim-IDs sind fortlaufend und
  damit erratbar; eine abweichende Meldung wäre selbst die Auskunft.
- **`#` und `+` am Wortende trennen Namen** (`model::name_marks`). Das Alias verwirft
  alles Nicht-Alphanumerische; „C", „C#" und „C++" teilten sich deshalb das Alias `c`
  und lösten auf EINE Entity auf. Die Entity-Auflösung verlangt jetzt zusätzlich, dass
  die direkt an einen Buchstaben oder eine Ziffer angehängten `#`/`+` übereinstimmen.
  Bewusst so eng: Groß-/Kleinschreibung, Leerraum, Bindestriche, Klammern und ein
  freistehendes `#` („Issue #12" / „Issue 12") verschmelzen weiterhin. Gespeicherte
  Aliase und Dateinamen ändern sich nicht; ein Bundle, in dem die Namen schon
  verschmolzen sind, bleibt wie es ist — getrennt wird erst, was neu geschrieben wird.
  Beim LESEN (`graph_neighbors`) hat der passende Name Vorrang, ohne ihn bleibt der
  bisherige Treffer.
- **Ein Alias trifft im Fragetext nur an Tokengrenzen.** Als Teilstring fand das
  Alias `c` jede Frage mit einem „c" darin und machte die Entity zum Seed.
- **Die Episoden-`summary` endet am LETZTEN „# Quellen"-Marker**, und nur, wenn das
  Dokument laut Frontmatter Quellen hat. Ein Verlauf mit eigener Überschrift
  „# Quellen" wurde vorher am ersten Marker abgeschnitten; die Datei enthielt den Text
  die ganze Zeit, solche Episoden lesen sich jetzt wieder vollständig.
- **Sperre je Commit, nicht je Prozess.** Zwei Terminals im selben Workspace sind
  der Normalfall; eine Sperre über die Lebensdauer des Prozesses machte daraus einen
  Fehler. Die Sperrdatei entsteht mit `create_new` und trägt die PID. Verwaist ist
  sie, wenn ihr Halter nachweislich tot ist (unter Linux über `/proc`) oder sie älter
  ist als 30 Sekunden — ohne libc gibt es auf den anderen Plattformen keinen anderen
  Weg, und ein Commit dauert Millisekunden. Wer eine verwaiste Sperre übernimmt,
  schreibt einen neuen Stempel: der Vorgänger starb mitten im Commit, also laden
  alle Stores neu.
- **Ein Stempel statt eines Vergleichs der Dokumente.** `graph.stamp` wird am Ende
  jedes Commits neu geschrieben; ein Store lädt neu, wenn der Stempel nicht der
  zuletzt gesehene ist. Beim Öffnen wird er VOR dem Laden gelesen, damit ein Commit,
  der währenddessen endet, auffällt. Der Abgleich kostet einen kleinen Dateizugriff
  je Commit und im Fall einer fremden Änderung ein vollständiges Neuladen.
- **Der sperrfreie Lesepfad überspringt Dateien, die während des Lesens
  verschwinden** (Sperr- und Temp-Dateien, per Promotion umgezogene Dokumente). Ein
  so gelesener Stand kann einen Datensatz verpassen; ein Store bemerkt das am
  Stempel, ein reiner Leser beim nächsten Öffnen.
- **Das Beförderungs-Gate stützt sich auf den Quellentyp**, nicht auf einen neuen
  Zustand. Kein Tool hat ein Argument dafür; gesetzt wird er nur von Laufzeit-Code.
  `work_item`-Quellen des Work-Adapters gelten als fremd: dort befördert die
  Laufzeit nach abgeschlossenem Item, nicht das Modell.
- **Die Belegprüfung ist ein wörtlicher Abgleich, kein Wahrheitsbeweis.** Sie hebt
  die Schwelle von „das Modell sagt es" auf „ein Tool hat es geliefert". Wer eine
  Datei schreibt und wieder liest, kann sich selbst zitieren. Nicht als Beleg zählen
  Tools, deren Ergebnis selbst von einem Modell stammt (`task`, `swarm`,
  `update_plan`), und der Recall-Block, der vor dem Auftrag des Nutzers steht —
  sonst belegte sich der Graph mit sich selbst.
- **Die Quelle nennt die gelesene Datei** (`artifact_uri`, im Bundle `resource`), wenn das
  Zitat aus `read_file` oder `read_pdf` stammt. Das Feld gab es im Modell von Anfang an, es
  setzte nur niemand. Gesetzt wird es wie der Quellentyp von der Laufzeit — aus dem
  `path`-Argument des Aufrufs, den das Evidenz-Buch ohnehin mitschneidet —, nie aus einem
  Modellargument. `grep` zählt bewusst nicht: sein `path` ist der Startpunkt der Suche,
  nicht die Datei des Treffers. Die Konsolidierung am Sitzungsende belegt aus dem fertigen
  Verlauf und kennt die Aufrufargumente nicht; ihre Quellen bleiben ohne Datei.
- **`graph_compare` vergleicht Namen, nicht Bedeutungen** (`src/compare.rs`). Es ordnet zu,
  was der Graph über mehrere Entities zum selben Prädikat sagt, und benennt, was bei einer
  fehlt — die Zuordnung, die ein Modell über zwei `graph_neighbors`-Antworten selbst leisten
  müsste und bei der eine Lücke schlicht nicht auffällt. `3 Monate` und `ein Quartal` gelten
  als abweichend; das Urteil bleibt beim Leser. Gezählt wird eine Entity nur als SUBJEKT:
  die Form `Dokument — Kriterium — Wert` ist die einzige, in der zwei Dokumente einander
  beim Befördern nicht ersetzen (gleiches Subjekt und Prädikat, anderes Objekt ⇒
  `superseded`). Kein Budget, kein Ranking — ein Vergleich, der Zeilen weglässt, ist keiner.
- **Die Secret-Erkennung sitzt an der einen Stelle, durch die jede Quelle geht**
  (`write::build_source`), und irrt in Richtung „nicht speichern". Sie kennt Formate,
  keine Bedeutung: was nicht anschlägt, ist nicht deshalb unbedenklich. Ersetzt wird
  der Auszug, nicht die Quelle — dass es einen Beleg gab, ist kein Geheimnis.
- **`--graph-readonly` öffnet über `GraphStore::open_frozen`**: ein Store ohne Pfad
  über dem lesend geladenen Stand. Er legt kein Bundle an und migriert kein Journal.
- **Zwei Bundles statt eines benutzerweiten.** Projektwissen soll beim Projekt
  liegen — sichtbar, kopierbar, mit dem Checkout verschiebbar. Der Preis sind
  überlappende IDs, aufgelöst an genau einer Stelle (`federation.rs`).
- **Alle `workspace`-Scopes eines Bundles gelten als dieses Projekt.** Wer bisher
  EIN `--graph DIR` für mehrere Workspaces benutzt hat, hatte je Pfad einen eigenen
  Scope; mit dem festen Projekt-Scope teilen sie sich künftig ihr Wissen, und die
  alten Scopes sind für alle sichtbar. Getrennte Projekte brauchen getrennte Bundles.
- **Die Übernahme ins Benutzer-Bundle kopiert, statt zu verschieben**, und nimmt nur
  die fremden Quellen mit. Das Projekt-Bundle bleibt unverändert.
- **Die Freigabe prüft `human:` am Kommando und das Terminal in der Executable.**
  Die erste Prüfung verhindert, dass Bibliotheks-Code versehentlich einen Agenten
  einträgt; die zweite, dass ein Agent das Kommando über seine Shell selbst aufruft.
- **Das Zeichen in `graph.owner` hängt weder am Pfad noch am Inhalt.** Am Pfad nicht,
  damit ein verschobener Checkout bestätigt bleibt; am Inhalt nicht, weil der sich mit
  jedem Commit ändert. Folge: wer die Datei weitergibt, gibt das Zeichen weiter — sie
  steht deshalb in der mitgelieferten `.gitignore`. Der Prüfwert ist FNV, kein
  kryptografischer MAC: er verhindert, dass ein Fremder ein Zeichen erfindet, nicht,
  dass er eines kopiert.
- **Löschen läuft nicht über `GraphWriteCommand`.** Was ein Kommando ist, kann ein Tool
  absetzen. `forget`/`merge`/`rename` gehen über `GraphStore::rewrite` und sind nur
  über das CLI erreichbar.
- **Eine gelöschte ID kann nach einem Neustart neu vergeben werden.** Die Zähler
  folgen dem Bestand. Verweise auf die gelöschte Aussage (`superseded_by`) werden
  deshalb beim Löschen entfernt.
- **Die Konsolidierung befördert selbst**, aber nur, was das Gate ohnehin durchließe:
  eine belegte Beobachtung. Eine Hypothese bleibt vorläufig, auch mit Beleg.
- **Aufgeräumt wird nach Anzahl, nicht nach Alter.** Die Revision ordnet die Scopes
  deterministisch; eine Uhr bräuchte eine Annahme darüber, wie oft jemand arbeitet.
- **Dateinamen aus einer Whitelist** (`okf::bundle::slug`, nur `[a-z0-9-]`; reservierte
  und Windows-Gerätenamen bekommen ein `-doc`-Suffix). Entity-Namen kommen aus
  Modellargumenten, ein Pfad-Ausbruch muss strukturell unmöglich sein, nicht nur geprüft.

## Nicht im Umfang

Ein Konsolidierungs-Worker im Hintergrund, LLM-gestützte Entity-Resolution und
-Summarization, Promotion ohne Beleg, `support`/`contest`, Retention/TTL-Läufer,
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
`retract.rs` (verlernen), `concurrent_writers.rs` (mehrere Prozesse auf einem Bundle),
`federation.rs` (Projekt- und Benutzer-Ebene), `cli.rs` (das Verb `agentkit graph`),
`consolidate.rs` (Konsolidierung und Aufräumen),
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
