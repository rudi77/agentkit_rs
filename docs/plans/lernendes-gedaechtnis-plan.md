# Implementierungsplan: Lernendes Gedächtnis — Graph und ctxman als Default

**Status:** Vorschlag, keine Implementierung. Umsetzung in einer eigenen Session.
**Stand des Codes:** agentkit 0.32.0 plus die Review-Fixes auf dem Branch `ccr-077abf96-lyteig` (Stand `de91a91`).
**Verwandte Issues:** #59 (Selbstkanonisierung), #77 (keine Prozesssperre), #78 (kein Löschen/Zusammenführen), #53, #22 (ctxman), #94 (Benchmarks neu messen).

---

## 1. Ziel

Agenten sollen über die Zeit dazulernen. Dafür ist der Wissensgraph im OKF-Format vorgesehen. Heute muss man ihn (`--graph DIR`) und ctxman (`--ctx DIR`) bei jedem Start ausdrücklich einschalten — ein Gedächtnis, das man vergessen kann einzuschalten, lernt nicht.

Der Plan macht beides zum Default und legt Wissen an zwei Orten ab:

| Ebene | Ort | Inhalt | Lebensdauer |
|---|---|---|---|
| Sitzung (working) | im Workspace-Bundle | Beobachtungen und Vermutungen des laufenden Auftrags | bis zur Beförderung oder zum Aufräumen |
| Projekt (canonical) | `<workspace>/.agentkit/graph/` | belegtes Wissen über dieses Projekt | dauerhaft |
| Benutzer (canonical) | `<config_dir>/graph/` | Vorlieben, Werkzeugwissen, projektübergreifende Lehren | dauerhaft |

Gelesen wird aus allen drei Ebenen, geschrieben in die Sitzung, befördert wird stufenweise.

---

## 2. Ist-Stand (am Code geprüft)

### 2.1 Graph

- **Aktivierung.** Nur über `--graph DIR` (`agentkit_app/src/bin/agentkit.rs`, `graph_active`). Geöffnet wird in `agentkit_app/src/lib.rs::open_graph`.
- **Scopes.** `GraphScope` ist ein Paar `kind`/`id`; real benutzt werden `workspace`, `session`, `swarm` (`agentkit_graph/src/model.rs`). Neue Arten kosten laut Kommentar keinen Code, nur eine Konvention. Eine Ebene über dem Workspace gibt es nicht.
- **Workspace-Identität.** Die Scope-ID des kanonischen Wissens ist der **absolute, kanonisierte Workspace-Pfad** (`open_graph`). Verschiebt man den Checkout, findet er sein Wissen nicht wieder.
- **Arbeits-Scope.** `graph_run_id`: `--graph-scope` > Stamm der `--session`-Datei > `run-<pid>-<ms>`. Ohne Sitzung ist der Arbeits-Scope also je Prozess neu. **Folge: Was nicht befördert wird, sieht der nächste Lauf nicht.** Lernen über Sitzungen hinweg hängt vollständig an der Beförderung.
- **Automatischer Recall ist im CLI/TUI nicht verdrahtet.** `GraphAgent` (`agentkit_graph/src/agent.rs`, Recall davor, Episode danach, Default 12 Aussagen / 800 Tokens) wird in `agentkit_app` nur vom Work-Adapter (`work_graph.rs`) benutzt. Das normale `agentkit --graph` registriert lediglich die fünf `graph_*`-Tools. Ob etwas erinnert oder nachgeschlagen wird, hängt heute allein am Modell.
- **Beförderung.** `graph_promote` verlangt Sichtbarkeit und mindestens eine Quelle. Weil jede Aussage automatisch eine Quelle bekommt, ist die Prüfung immer erfüllt; `graph_remember` mit `durable: true` befördert im selben Aufruf (#59).
- **Kein Vergessen.** Es gibt weder Löschen noch Zurückziehen, Umbenennen oder Zusammenführen (#78). Vorhanden ist nur `superseded` bei widersprechenden kanonischen Aussagen.
- **Keine Prozesssperre** (#77). Zwei Prozesse auf demselben Bundle vergeben dieselben IDs. Drei bestehende Tests öffnen bewusst zwei schreibende Stores im selben Prozess (`tests/store.rs` Migrationstest um Zeile 425, `tests/okf_konformitaet.rs` um Zeile 396/423, `tests/promotion.rs::die_promotions_spur_ueberlebt_den_bundle_rebuild`) — eine Sperre über die Lebensdauer des Prozesses würde sie brechen.
- **`--graph-readonly` läuft über das schreibende `GraphStore::open`** (`agentkit_app/src/lib.rs:84`), nicht über `open_read_only`.
- **IDs sind fortlaufend je Bundle** (`C-n`, `E-n`, `S-n`). Zwei Bundles haben überlappende IDs.
- **Retrieval ist lexikalisch** (Alias exakt, sonst Token-Overlap), linearer Scan. Episoden werden geschrieben, aber vom Retrieval nicht gelesen.
- **README „Nicht im Umfang“** nennt ausdrücklich Konsolidierungs-Worker, Promotion-Policies jenseits von „manuell“ und ctxman-Integration. Dieser Plan erweitert den Umfang; der Abschnitt muss mitgeführt werden.

### 2.2 ctxman

- **Aktivierung.** Nur über `--ctx DIR` (Feature `ctxman`); `attach_managed_context` in `agent_framework_rs/src/app.rs`.
- **Zustand.** `snapshot.json`, `blobs/` und `facts.jsonl` im angegebenen Verzeichnis. Dieses Repo ignoriert `.agentkit-ctx/` per `.gitignore`.
- **Fakten.** Der Major GC extrahiert Fakten und schreibt sie über `PromotionSink` weg. Ein `PromotedFact` ist **Freitext** (`fact`, `kind`, Sitzung, Turn), kein Tripel. Die Senke ist heute eine JSONL-Datei, die mit `--memory` auf dessen Datei zeigt.
- **Offene Fehler.** Helfer bricht bei kleinem Budget mit Render-Fehler ab (#53); `/reset` setzt den verwalteten Kontext nicht zurück (#22); `/rewind` wird mit ctxman abgelehnt (`Agent::rewind_check`).
- **Wachstum.** Evictete Segmente behalten ihren Inhalt im Snapshot; verwaiste Blobs werden nie aufgeräumt (Sweep laut README bewusst gestrichen).

### 2.3 Drei Gedächtnisse nebeneinander

`--memory` (Stichwort-`remember`/`recall`, `agent_framework_rs/src/memory.rs`), die Fakten-Datei von ctxman und der Graph wissen nichts voneinander.

### 2.4 Messlage

`agent_benchmarks/BENCHMARK_REPORT.md` (agentkit 0.21.0): Graph, Swarm und Work zeigten in drei Runden keinen messbaren Nutzen. Das ist kein Gegenargument gegen den Default, aber ein Auftrag: Der Nutzen ist zu messen, nicht anzunehmen.

---

## 3. Entschiedene Architektur

### 3.1 Zwei Bundles, eine Sicht

- **Projekt-Bundle** unter `<workspace>/.agentkit/graph/`, **Benutzer-Bundle** unter `<config_dir>/graph/`. Beide sind eigenständige OKF-Bundles und einzeln mit jedem OKF-Werkzeug lesbar.
- **Warum zwei Bundles statt eines benutzerweiten mit allen Workspaces:** Das Projektwissen soll beim Projekt liegen (sichtbar, kopierbar, optional ins Repo). Der Preis sind überlappende IDs.
- **Qualifizierte IDs an der Tool-Grenze.** In den Dateien bleiben die IDs wie sie sind. In Tool-Ausgaben und -Eingaben trägt alles aus dem Benutzer-Bundle ein Präfix (Vorschlag `u:C-17`); unqualifizierte IDs meinen das Projekt-Bundle. Die Auflösung sitzt an einer Stelle in `agentkit_graph` (ein kleiner Verbund aus zwei Stores mit je eigenem `GraphAccess`), nicht in jedem Tool.
- **Rangfolge im Retrieval:** Sitzung vor Projekt vor Benutzer, innerhalb wie bisher nach Score, dann ID. Die Ordnung bleibt total und deterministisch.

### 3.2 Stabile Workspace-Identität

Im Projekt-Bundle identifiziert das Bundle selbst den Workspace. Die kanonische Scope-ID wird dort eine **feste Konstante** statt des absoluten Pfads. Damit übersteht das Wissen ein Verschieben des Checkouts.

Migration: Findet sich beim Öffnen ein vorhandener Scope `workspace-<pfad>`, wird er **lesend** in die Sicht aufgenommen; neue Beförderungen gehen in den festen Scope. Es wird nichts umgeschrieben und nichts verloren.

### 3.3 Sperre je Commit, nicht je Prozess

- Eine Lockdatei im Bundle wird **nur für die Dauer eines Commits** gehalten (anlegen mit `create_new`, PID hinein, nach dem Commit entfernen). Verwaiste Sperren toter Prozesse werden übernommen.
- Vor dem Anwenden eines Schreibkommandos prüft der Store, ob ein anderer Prozess inzwischen committet hat, und lädt dann neu. So vergeben zwei Prozesse nie dieselbe ID.
- **Lesen sperrt nie.** `open_read_only` und der viz-Reiter bleiben unberührt; `--graph-readonly` wird auf `open_read_only` umgestellt.
- Die drei Tests mit zwei schreibenden Stores im selben Prozess bleiben grün, weil die Sperre nicht an `open` hängt.

### 3.4 Beförderungs-Gate

Lernen heißt hier: Etwas wandert von der Sitzung ins Projektwissen. Diese Schwelle entscheidet, ob der Graph mit der Zeit besser oder schlechter wird.

**Sitzung → Projekt** verlangt:
1. mindestens eine **fremde Quelle** — ein Tool-Ergebnis (gelesene Datei, Testlauf, Shell-Ausgabe) oder eine Aussage des Nutzers. Die automatisch erzeugte Quelle „hat das Modell gesagt“ zählt nicht.
2. dass die Aussage **nicht im selben Aufruf** entstanden ist (`durable: true` befördert Hypothesen nicht mehr).

**Projekt → Benutzer** geschieht **nie durch das Modell allein**. Es braucht eine ausdrückliche Freigabe des Menschen (Rückfrage am Terminal oder `agentkit graph promote --user <id>`). Grund: Sonst wandert Wissen über Kundencode in jedes andere Projekt.

### 3.5 Verlernen

- **`graph_retract`** (Tool): markiert eine Aussage als zurückgezogen, mit Grund. Sie bleibt als Evidenz lesbar und fällt aus dem Retrieval — dieselbe Idee wie `superseded`.
- **`agentkit graph …`** (CLI, für den Menschen): `status`, `list`, `show`, `forget`, `merge`, `rename`, `doctor`. Echtes Löschen gibt es nur hier.

### 3.6 Lernen ohne Modell-Disziplin

Heute lernt der Agent nur, wenn das Modell `graph_remember` aufruft. Zwei automatische Wege kommen dazu:

- **Recall vor jedem Nutzer-Zug** im CLI, TUI und ACP, wie ihn `GraphAgent` schon kann. Der Ausschnitt wird der Nutzer-Nachricht vorangestellt (nicht dem System-Prompt), damit der Prompt-Cache-Präfix stabil bleibt.
- **Konsolidierung am Sitzungsende:** ein einzelner LLM-Aufruf, der aus dem Verlauf der Sitzung Aussage-Kandidaten mit Quellenverweis bildet und in den Sitzungs-Scope schreibt. Befördert wird danach nur, was das Gate aus 3.4 besteht. Sie läuft nur bei Sitzungen mit nennenswerter Arbeit (Schwelle über Tool-Aufrufe) und ist abschaltbar.
- **ctxman-Fakten** landen nicht mehr nur in `facts.jsonl`, sondern als Material für die Konsolidierung im Graphen. Weil ein `PromotedFact` Freitext ist, wird er nicht direkt zur Aussage, sondern zur Episode der Sitzung; die Konsolidierung macht daraus Tripel.

### 3.7 Gedächtnis ist Daten, keine Anweisung

Der Graph wird in künftige Prompts eingespielt. Das macht ihn zu einem **dauerhaften Kanal für Prompt-Injection**: Ein präparierter Dateiinhalt, einmal als „Wissen“ gespeichert, käme in jeder Sitzung wieder — auf Benutzer-Ebene in jedem Projekt.

Daraus folgen feste Regeln:
- Der Recall-Block ist als Daten gerahmt und gewährt nie Freigaben.
- Die Herkunft jeder Aussage bleibt sichtbar (gibt es schon).
- Ein Projekt-Bundle, das dieser Benutzer nicht selbst angelegt hat (frisch geklontes Repo mit eingechecktem Graphen), gilt als unbestätigt. Es wird erst nach einmaliger Bestätigung gelesen — derselbe Mechanismus wie `agentkit config trust` für `allow:` (#57).
- Auszüge aus Tool-Ergebnissen können Secrets enthalten. Vor dem Schreiben greift eine einfache Erkennung bekannter Key-Formate; im Zweifel wird der Auszug nicht gespeichert.

### 3.8 ctxman

- **Ort:** `<config_dir>/ctx/<projekt>/<sitzung>/`, parallel zu den Sitzungsdateien. Kontext ist Sitzungszustand, kein Wissen, und gehört nicht in den Workspace.
- **Aufräumen:** Mit der Sitzung verschwindet ihr ctxman-Verzeichnis.
- **Ausweg:** `--no-ctx`; `--ctx DIR` überschreibt den Ort wie bisher.

### 3.9 Schalter

| Schalter | Wirkung |
|---|---|
| (nichts) | Graph und ctxman aktiv, sofern das Feature gebaut ist |
| `--no-graph` / `--no-ctx` | aus für diesen Lauf |
| `"graph": false` / `"ctx": false` in `config.json` oder Profil | dauerhaft aus |
| `--graph DIR` / `--ctx DIR` | anderer Ort, wie bisher |
| `--graph-readonly` | lesen, nicht schreiben |
| `--tools none` bzw. Liste ohne Graph-Tools | weder Tools noch Recall |

---

## 4. Phasen

Jede Phase ist für sich auslieferbar und endet mit grünen Tests. Reihenfolge ist Abhängigkeit, nicht Wunsch.

### Phase 0 — Der Graph hält Default-Betrieb aus

Ohne diese Phase schadet Default-an.

- Sperre je Commit mit Neuladen bei fremder Änderung (3.3); `--graph-readonly` über `open_read_only`.
- Beförderungs-Gate Sitzung → Projekt (3.4, Punkt 1 und 2).
- `graph_retract` und der Zustand „zurückgezogen“ im Modell, im OKF-Dokument und im Retrieval (3.5).

**Akzeptanz**
- Zwei Prozesse schreiben gleichzeitig ins selbe Bundle; nach dem Neuöffnen sind alle Datensätze da und alle IDs eindeutig.
- Eine verwaiste Sperre eines toten Prozesses blockiert nicht.
- Eine Hypothese ohne fremde Quelle lässt sich nicht befördern, auch nicht mit `durable: true`.
- Eine zurückgezogene Aussage erscheint weder in `graph_search` noch im Recall, bleibt aber über `graph_evidence` lesbar.
- Bestehende Bundles öffnen unverändert; die drei Zwei-Store-Tests bleiben grün.

**Dateien:** `agentkit_graph/src/store/`, `okf/bundle.rs`, `write.rs`, `model.rs`, `tools.rs`, `retrieval.rs`; `agentkit_app/src/lib.rs`.

### Phase 1 — Zwei Ebenen

- Benutzer-Bundle unter `<config_dir>/graph/`, Scope-Art `user`.
- Verbund aus Projekt- und Benutzer-Store mit qualifizierten IDs (3.1).
- Feste Workspace-Scope-ID samt lesender Migration (3.2).
- Beförderung Projekt → Benutzer nur mit Freigabe des Menschen (3.4).

**Akzeptanz**
- Eine im Projekt A auf Benutzer-Ebene beförderte Aussage erscheint im Recall von Projekt B.
- Projektwissen von A erscheint in B nicht.
- Das Modell kann ohne Freigabe nichts auf die Benutzer-Ebene heben.
- Ein verschobener Checkout findet sein Projektwissen.
- Ein altes Bundle mit `workspace-<pfad>` zeigt sein Wissen weiter.
- `u:C-17` und `C-17` werden in allen fünf Tools richtig aufgelöst.

**Dateien:** `agentkit_graph/src/` (neuer Verbund, `access.rs`, `tools.rs`, `retrieval.rs`), `agentkit_app/src/lib.rs`, `agent_framework_rs/src/config.rs` (nur der Pfad).

### Phase 2 — Default-an und Sichtbarkeit

- Graph aktiv ohne Flag; Schalter aus 3.9. Die Bundle-Verzeichnisse entstehen erst beim ersten Schreiben.
- Automatischer Recall im CLI, TUI und ACP (3.6); Schwarm-Mitglieder und Work-Item-Agenten erben den Verbund wie heute den Store.
- `agentkit graph status|list|show|forget|merge|rename|doctor|promote --user` (3.5) und `/graph` im REPL und TUI.
- Startzeile mit dem Stand, zum Beispiel „Gedächtnis: 42 Aussagen (Projekt), 7 (Benutzer)“.
- Vertrauensprüfung für fremde Projekt-Bundles (3.7).
- Benchmark-Harness und Release-Smoke-Tests setzen `--no-graph`, wo sie Vergleichbarkeit brauchen.

**Akzeptanz**
- `agentkit` ohne Flags in einem frischen Verzeichnis legt nichts an, solange nichts erinnert wird.
- Ein leerer Graph kostet keinen Recall-Text.
- `--no-graph` verhält sich Byte für Byte wie heute ohne `--graph` (gleiche Tool-Schemas, gleicher System-Prompt).
- Die `cli`-Release-Variante und ein Build ohne Feature `graph` verhalten sich wie bisher.
- Ein geklontes Repo mit eingechecktem Graphen wird erst nach Bestätigung gelesen.

**Dateien:** `agentkit_app/src/bin/agentkit.rs` (Schalter, Verb, Startzeile), `agentkit_app/src/bin/tui.rs`, `agent_framework_rs/src/tui.rs` und `acp.rs` (nur der Recall-Einhängepunkt), `action.yml`, `agent_benchmarks/`, `.github/workflows/release.yml` (Smoke-Tests).

### Phase 3 — Lernen ohne Zutun des Modells

- Konsolidierung am Sitzungsende (3.6), abschaltbar, mit Schwelle.
- ctxman-Fakten als Episoden der Sitzung im Graphen; `facts.jsonl` bleibt für `--memory`-Nutzer bestehen.
- Aufräumen alter Sitzungs-Scopes im Projekt-Bundle (nach Alter oder Anzahl), damit `working/` nicht unbegrenzt wächst.

**Akzeptanz**
- Nach einer Sitzung, in der das Modell `graph_remember` nie aufgerufen hat, enthält der Sitzungs-Scope Aussagen mit Quellenverweis auf Tool-Ergebnisse dieser Sitzung.
- Ohne fremde Quelle wird nichts befördert.
- Scheitert der Konsolidierungs-Aufruf, endet die Sitzung normal und ohne Fehlermeldung an prominenter Stelle.
- Die Konsolidierung läuft nicht bei Sitzungen unter der Schwelle und nicht unter `--no-graph`.

**Dateien:** `agentkit_graph/src/` (Konsolidierung, Aufräumen), `agent_framework_rs/src/context.rs` (zweite Senke über die vorhandene `PromotionSink`-Naht — der Kern darf den Graphen weiterhin nicht kennen, die Senke kommt von `agentkit_app`), `agentkit_app/src/`.

### Phase 4 — ctxman als Default

Voraussetzung sind drei Fixes:
- Helfer bricht bei kleinem Budget ab (#53).
- `/reset` setzt den verwalteten Kontext zurück (#22).
- `/rewind` funktioniert mit ctxman oder erklärt klar, warum nicht, und bietet `/fork` an.

Dann:
- ctxman aktiv ohne Flag, Zustand unter `<config_dir>/ctx/<projekt>/<sitzung>/` (3.8).
- Aufräumen zusammen mit der Sitzung.
- `--no-ctx`.

**Akzeptanz**
- Eine Sitzung ohne Flags übersteht das Überschreiten des Kontextfensters ohne Fehler.
- `--continue` nimmt den ctxman-Zustand der Sitzung wieder auf.
- `/reset`, `/rewind` und `/fork` verhalten sich mit und ohne ctxman gleich oder sagen, was abweicht.
- `--no-ctx` verhält sich wie heute ohne `--ctx`.
- Die Golden-Fixtures von ctxman bleiben unangetastet.

**Dateien:** `agent_framework_rs/src/context.rs`, `agent.rs`, `app.rs`, `sessions.rs`; `agentkit_app/src/bin/agentkit.rs`; `ctxman_rs/` nur für #53.

### Phase 5 — Messen, dann ausliefern

Zwei Fragen, zwei Messungen:

1. **Schadet der Default?** Die vorhandenen Benchmarks mit und ohne Graph sowie mit und ohne ctxman, auf dem aktuellen Stand und mit mindestens einem Anthropic-Arm (#94). Kennzahlen: gelöste Aufgaben, Tokens, Tool-Aufrufe, Wandzeit.
2. **Lernt der Agent?** Ein neuer Aufgabensatz, in dem dieselbe Aufgabenfamilie zweimal im selben Workspace läuft. Die Hypothese ist, dass der zweite Durchgang weniger Schritte und Tokens braucht. Ohne diesen Nachweis ist „lernt dazu“ eine Behauptung.

**Akzeptanz für den Default im Release**
- Kein Rückgang bei gelösten Aufgaben jenseits der Lauf-zu-Lauf-Streuung.
- Der Token-Aufschlag je Lauf ist beziffert und dokumentiert.
- Der Lern-Aufgabensatz zeigt einen messbaren Effekt — oder der Default für die Konsolidierung bleibt aus, bis er es tut.

---

## 5. Entscheidungen, die beim Eigentümer liegen

Jeweils mit Empfehlung; die Umsetzung sollte sie nicht stillschweigend treffen.

1. **Graph auch im Headless-Betrieb an?** Empfehlung: ja. Dort leisten Agenten die lange, unbeaufsichtigte Arbeit, aus der am meisten zu lernen ist. `--tools` und `--no-graph` bleiben als Ausweg, und reine Filter-Pipelines (`examples/accounts_payable`) setzen `--no-graph`.
2. **Projekt-Bundle ins Repo einchecken?** Empfehlung: zunächst nein, `.agentkit/` bleibt ignoriert. Fortlaufende IDs kollidieren, sobald zwei Personen unabhängig schreiben und zusammenführen. Team-Wissen über Git braucht kollisionsfreie IDs und ist ein eigenes Vorhaben.
3. **Konsolidierung per Default an?** Empfehlung: erst nach Phase 5. Sie kostet einen LLM-Aufruf je Sitzung.
4. **Was wird aus `--memory`?** Empfehlung: bleibt unverändert bestehen, wird aber nicht weiterentwickelt. Eine Ablösung durch den Graphen ist erst sinnvoll, wenn Phase 5 den Nutzen belegt.
5. **Präfix der Benutzer-IDs.** Vorschlag `u:`; jede kurze, im YAML unkritische Schreibweise geht.

---

## 6. Was der Plan bewusst nicht enthält

- **Embeddings oder Vektor-Suche.** Das Retrieval bleibt lexikalisch; ein invertierter Index ist erst Thema, wenn eine Messung den linearen Scan als Problem zeigt.
- **Team-Sharing über Git** (siehe Entscheidung 2).
- **Automatische Beförderung auf die Benutzer-Ebene.**
- **Mandantenfähigkeit, TTL-Läufer, verteilte Graphen** — weiterhin außerhalb des Umfangs von `agentkit_graph`.
- **Änderungen am C#-treuen Verhalten von ctxman** über #53 hinaus.
- **Eine neue Speicher-Abhängigkeit.** Kein SQLite, kein C-Compiler; der statische musl-Build und der Offline-Default bleiben erhalten.

---

## 7. Hinweise für die umsetzende Session

- **Zuerst lesen:** `CODING_GUIDELINES.md`, `CLAUDE_BEHAVIORAL.md`, `agentkit_graph/README.md` (Abschnitte „Bewusste Design-Entscheidungen“ und „Nicht im Umfang“), `agent_framework_rs/CLAUDE.md`.
- **Abhängigkeitsrichtung:** Der Agentenkern kennt weder Graph noch Swarm noch Work. Alles Neue kommt über die vorhandenen Nähte (`extra_tools`/`ExtraToolCtx`, `PromotionSink`, `SwarmToolConfig::extra_member_tools`) aus `agentkit_app`.
- **Tests sind die Spezifikation.** Bestehende Tests nicht ändern oder abschwächen; schreibt einer ein Verhalten fest, das dieser Plan ändert, ist das eine Entscheidung des Eigentümers.
- **Abwärtskompatibilität:** Bestehende Bundles, Journale und Sitzungen müssen ohne Datenverlust weiter öffnen. Neue Felder mit `serde(default)`.
- **Dokumentation mitführen:** die README-Abschnitte der betroffenen Crates, `INSTALL.md`, die Wurzel-`CLAUDE.md` (Komponententabelle und Release-Abschnitt nennen die Aktivierung), die Hilfe-Texte und Shell-Completions im Binary, die Website nach `website/EDITORIAL_GUIDE.md`.
- **CI:** `.github/workflows/ci.yml` prüft alle sieben Crates mit `clippy -D warnings` und `cargo fmt --check`.
- **Offene Punkte aus dem Review, die diesen Plan berühren:** Retrieval-Kosten (#78), `fsync` und Reste gescheiterter Commits im Bundle (#77), Kosten der ctxman-Eviction für den Prompt-Cache (#74).

---

## 8. Verifikation

```bash
cargo test --manifest-path agentkit_graph/Cargo.toml
cargo test --manifest-path ctxman_rs/Cargo.toml --all-features
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features --features ctxman
cargo test --manifest-path agentkit_app/Cargo.toml --no-default-features --features "viz work graph"
cargo test --manifest-path agentkit_app/Cargo.toml --no-default-features          # ohne Graph: Verhalten wie bisher
cargo build --manifest-path agentkit_app/Cargo.toml --features "tui pdf ctxman tiktoken graph work viz"
```

Dazu von Hand am gebauten Binary, weil kein Test ein Terminal hat:

1. Zwei Terminals im selben Workspace, beide erinnern etwas; danach `agentkit graph doctor`.
2. Sitzung in Projekt A, Aussage auf Benutzer-Ebene freigeben, neue Sitzung in Projekt B: Die Aussage erscheint im Recall.
3. Frisch geklontes Repo mit eingechecktem Graphen: Rückfrage vor dem ersten Lesen.
4. `agentkit --no-graph --no-ctx` gegen `agentkit` 0.32 vergleichen: gleiche Tool-Liste, gleicher System-Prompt.
