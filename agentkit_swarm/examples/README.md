# Beispiele — agentkit-swarm

Alle Beispiele laufen **ohne Netz und ohne API-Key** (Ausnahme: `openai_swarm`).
Statt eines echten Modells steckt in ihnen ein kleines Regel-Modell, das — wie ein
echtes LLM — nur den Gesprächsverlauf sieht und daraus sein nächstes Werkzeug
wählt. Der Schwarm-Code darüber ist derselbe, den man mit
`openai_from_env()` produktiv fährt.

```bash
cargo run --manifest-path agentkit_swarm/Cargo.toml --example <name>
```

## Wann lohnt ein Schwarm?

agentkit kennt drei Formen von Mehr-Agenten-Arbeit. Der Schwarm ist die teuerste
— nimm ihn nur, wenn die beiden anderen die Aufgabe nicht tragen:

| Form | Kosten | Richtig, wenn … | Falsch, wenn … |
|---|---|---|---|
| **Ein Agent** mit Werkzeugen | am billigsten | die Aufgabe eine Kette von Schritten ist | Teilaufgaben unabhängig sind und Wartezeit dominiert |
| **`task`-Sub-Agenten** ([`coding_swarm`](../../agent_framework_rs/examples/coding_swarm/README.md)) | mittel | ein Orchestrator zerlegt und wieder zusammensetzt; Sub-Agenten brauchen kein Gedächtnis über ihren Auftrag hinaus | die Beteiligten mehrere Runden miteinander verhandeln sollen |
| **Schwarm** (dieses Crate) | am teuersten | Peers über mehrere Runden zusammenarbeiten, jeder sein eigenes, dauerhaftes Gedächtnis behält — und der Abschluss an ein Quorum gebunden sein soll | eine feste Stufenfolge genügt (dann: Pipeline aus einzelnen `agentkit`-Aufrufen) |

Die drei Gründe, die in der Praxis für einen Schwarm sprechen, hat je ein
Beispiel unten:

1. **Zeit** — unabhängige Teilaufgaben laufen echt parallel (ein Actor = ein
   OS-Thread) → `parallel_research_swarm`
2. **Qualität** — das Ergebnis muss ein Quorum überstehen, das der Autor nicht
   selbst öffnen kann → `red_team_swarm`
3. **Durchsatz auf echten Artefakten** — dieselbe Änderung über viele Dateien,
   disjunkt aufgeteilt und anschließend verifiziert → `codemod_swarm`

Und drei Gründe, es **nicht** zu tun: die Teilaufgaben hängen voneinander ab
(dann ist der Schwarm nur eine teure Kette); es gibt nichts zu verhandeln (dann
genügen Sub-Agenten); der Ablauf ist immer derselbe (dann ist eine Pipeline
billiger und reproduzierbar).

## Die Beispiele

| Beispiel | Zeigt | Kernpunkt |
|---|---|---|
| [`parallel_research_swarm`](parallel_research_swarm.rs) | Stern, Broadcast, echte Nebenläufigkeit | misst mit: 1350 ms Arbeit in ~600 ms Wanduhrzeit |
| [`red_team_swarm`](red_team_swarm.rs) | Mesh, adversariales Freigabe-Tor | ein Vorschlag **fällt durch** und wird nachgebessert |
| [`codemod_swarm`](codemod_swarm.rs) | echte Datei-Werkzeuge, geteilter Workspace | disjunkte Zuständigkeiten + eigener Prüfer |
| [`discussion_swarm`](discussion_swarm.rs) | Moderator + zwei Positionen | der kürzeste Weg zu Broadcast & Konsens |
| [`dev_team_swarm`](dev_team_swarm.rs) | Ketten-Topologie | Topologie als Capability: planner erreicht reviewer nicht |
| [`dynamic_swarm`](dynamic_swarm.rs) | das `swarm`-Tool | ein Agent baut sich seinen Schwarm zur Laufzeit selbst |
| [`openai_swarm`](openai_swarm.rs) | echtes Modell (`--features openai`) | dieselbe Verdrahtung mit `openai_from_env()` |

### `parallel_research_swarm` — wenn Wartezeit der Engpass ist

Ein Koordinator verteilt dieselbe Frage per `swarm_broadcast` an drei
Spezialisten (Doku, Codebasis, Betrieb), die unabhängig voneinander „arbeiten"
(300/450/600 ms `sleep`). Weil jeder Actor sein eigener OS-Thread ist, kostet
das die Zeit des langsamsten Beitrags, nicht die Summe — das Beispiel druckt
beide Zahlen und einen Trace mit Zeitstempeln:

```
[    0 ms] koordinator  → (alle)       request      msg-2
[  300 ms] doku         → koordinator  observation  msg-5
[  450 ms] codebase     → koordinator  observation  msg-6
[  600 ms] betrieb      → koordinator  observation  msg-7

Arbeit der Spezialisten zusammen: 1350 ms (so lange bräuchte ein Solo-Agent)
Tatsächliche Laufzeit:            601 ms
```

### `red_team_swarm` — wenn das Ergebnis verteidigt werden muss

Autor, Angreifer und Freigabe. Der Autor schlägt vor, die beiden Prüfer stimmen
mit **Nein** und schicken je eine Kritik; erst die nachgebesserte Fassung
bekommt beide Stimmen. Der Abschluss hängt damit nicht am Urteil eines
Orchestrators, sondern an `CompletionPolicy::Consensus { required_approvals: 2 }`
— ein deterministisches Tor. Nebenbei sichtbar: die Prüfer **erinnern sich** an
Fassung 1, wenn Fassung 2 eintrifft (Agent-Memory lebt über alle Nachrichten).

### `codemod_swarm` — wenn viele Artefakte dieselbe Änderung brauchen

Der einzige Schwarm hier, der echte Dateien anfasst: `CodingTools` auf einem
Wegwerf-Workspace unter `temp_dir()`. Der Koordinator sucht die Fundstellen per
`grep`, teilt sie **disjunkt** auf zwei Umbauer auf, die parallel `edit_file`
fahren; ein Prüfer grept anschließend selbst nach Restvorkommen und schlägt erst
dann den Abschluss vor. Werkzeuge gibt es je Rolle — wer nicht schreiben soll,
bekommt `edit_file` gar nicht erst.

Der wichtigste Satz dazu: **alle Mitglieder teilen sich EINEN Workspace.**
Paralleles Schreiben ist nur sicher, solange die Zuständigkeiten disjunkt sind
— das leistet die Aufteilung, nicht der Schwarm.

## Und aus der Executable heraus?

Für einen Schwarm muss man nichts in Rust schreiben: `agentkit` hat das
`swarm`-Tool ab Werk verdrahtet, der Agent baut sich seinen Schwarm zur Laufzeit
selbst (Mitglieder, Topologie, Quorum). `dynamic_swarm.rs` zeigt die Verdrahtung
als Bibliothek, der [README](../README.md#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)
das Spezifikationsformat:

```bash
agentkit --tui        # der Agent kann jetzt Schwärme erzeugen
agentkit --no-swarm   # … oder eben nicht
```
