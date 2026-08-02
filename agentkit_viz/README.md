# agentkit-viz

Ein Beobachtungs-Werkzeug für agentkit-Läufe: liest den NDJSON-Trace
(`agentkit --trace DIR`) und zeigt ihn im Browser — **live während eines Laufs
und nachträglich aus der Datei, über denselben Codepfad**.

Es ist ausdrücklich ein **Debug-Werkzeug, kein Produkt**: localhost, keine Auth
über das Loopback-Token hinaus, kein Multi-User, kein Schreibzugriff.

```bash
# 1. einen Lauf mitschreiben …
agentkit --trace .agentkit/trace "Analysiere dieses Crate"
# … oder einen Work-Lauf
agentkit work run mein-vorhaben --trace .agentkit/trace

# 2. in einem zweiten Terminal zusehen (auch währenddessen)
agentkit viz --open --graph .agentkit/graph
```

Der Betrachter steckt hinter dem Cargo-Feature `viz`:

```bash
cargo build --manifest-path agentkit_app/Cargo.toml --features "viz work"
```

## Was es zeigt

| Ansicht | Woher die Daten kommen |
|---|---|
| **Agenten** | die `source`-Tags des Ereignisstroms — Haupt-Agent, Sub-Agenten (`task`), Schwarm-Mitglieder und, in einem Work-Lauf, jeder Item-Versuch (`W-1#2`) |
| **Verlauf** je Agent | die Ereignisse mit diesem Tag, aufklappbar bis zur rohen Nutzlast |
| **Kontext** je Agent | die `context_snapshot`-Datensätze (Segmente, Tokens, Budget, Nachrichten) |
| **Zeitleiste** | der ganze Lauf, eine Zeile je Ereignis |
| **Schwarm** | die `swarm_event`-/`swarm_result`-Datensätze: Sequenzdiagramm (Mitglieder als Spalten, Zeit nach unten, `MessageKind` als Farbe), Abstimmung mit Zustimmungen **und Ablehnungen**, Dead Letters |
| **Graph** | `GraphStore::open_read_only` + `export` — Knoten = Entities, Kanten = Claims, Filter nach Ebene/Scope/Status, Klick auf eine Kante zeigt die Provenance (Feature `graph`) |
| **Work** | `WorkStore::open_read_only` — sperrfrei, ohne jede Schreibwirkung (Feature `work`) |

## Sicherheit

Ein Trace enthält **alles**, was der Agent gelesen und geschrieben hat:
Dateiinhalte, Shell-Ausgaben, Modellantworten. In einem Repo mit `.env` also
potenziell Geheimnisse im Klartext. Der Betrachter liefert genau das aus.
Deshalb:

- Er bindet **ausschließlich** an `127.0.0.1`. Es gibt keine Option, das zu ändern.
- Jede Anfrage braucht das beim Start erzeugte **Zufalls-Token** aus der URL.
  Das schützt nicht vor dem Nutzer selbst, aber vor jedem anderen Programm auf
  demselben Rechner — auch vor einer beliebigen Webseite in seinem Browser.
- Er **schreibt nichts**. Es gibt keinen schreibenden Endpunkt.
- Ein Projektname unter `/api/work/<projekt>` und der `run=`-Parameter sind
  Verzeichnis- bzw. Dateinamen, keine Pfade — geprüft wird POSITIV (`ist_dateiname`),
  weil eine Liste verbotener Zeichen erfahrungsgemäß immer eines übersieht
  (unter Windows etwa `C:`, das ein `join` den ganzen Pfad ersetzen lässt).

Es gibt **keine Redaktion von Geheimnissen**. Ein Filter, dem man vertraut, ist
gefährlicher als eine ehrliche Warnung; die Warnung steht beim Anlegen des Trace
auf stderr und noch einmal beim Start des Betrachters.

## Aufbau (hexagonal)

```text
src/
  model.rs    Spiegeltypen des Trace-Formats (owned String) + Deserialize
  project.rs  Projektionen: Agenten, Verlauf, Kontext, Zeitleiste   ← Domäne
  swarm.rs    Projektion des Schwarm-Verkehrs (Sequenz, Abstimmung) ← Domäne
  trace.rs    NDJSON lesen und tailen (Offset-basiert)              ← Adapter Dateisystem
  api.rs      die Endpunkte als reine Funktionen (Pfad → Value)
  server.rs   tiny_http: Routing, Token, statische Seite            ← Adapter Browser
  assets/     index.html, app.js, style.css (per include_str! eingebettet)
```

`model` und `project` kennen weder HTTP noch Dateisystem — die Projektionen sind
deshalb ohne Server testbar (`tests/viz.rs`).

## Bewusste Design-Entscheidungen

- **Eigene Spiegeltypen statt der aus `agentkit`.** Der Kern leitet nur
  `Serialize` ab: `AgentEvent::etype` ist ein `&'static str` und wäre gar nicht
  deserialisierbar. Die eigenen Typen halten den Betrachter außerdem unabhängig
  von der Version, mit der ein Trace geschrieben wurde. Eine unbekannte
  Ereignis-Variante wird zu `TraceData::Unbekannt` **mit erhaltener Rohform**,
  statt die Zeile zu verwerfen — ein Betrachter, der an einem neuen Ereignistyp
  scheitert, wäre genau dann nutzlos, wenn man ihn braucht.

- **Eine Sitzung ist eine Trace-Datei, gesucht wird rekursiv.** Ursprünglich las
  der Betrachter ein flaches Verzeichnis. Das brach im Benchmark-Betrieb: dort
  schreibt jeder Task seinen eigenen Trace neben seine übrigen Artefakte, bei
  Harbor also unter `<benchmark>/<job>/<trial>/agent/trace/` — fünf Ebenen unter
  der Ergebniswurzel. Jetzt wandert `list_traces` bis zu acht Ebenen tief, und
  der Pfad einer Datei RELATIV zur Wurzel ist zugleich ihr Sitzungsname und die
  Kennung, die als `run=` zurückkommt (immer `/`-getrennt, auch unter Windows;
  `ist_sitzungspfad` prüft jedes Segment einzeln gegen Ausbrüche).
  `agentkit viz --trace <ergebniswurzel>` zeigt damit alle Benchmarks auf
  einmal. Der Durchlauf wird zwischengespeichert — aber **nur, wenn er teuer
  ist** (gemessen 175–380 ms bei 2176 Verzeichnissen, gegenüber Mikrosekunden
  bei einem gewöhnlichen `.agentkit/trace`). Pauschal zu cachen hieße, im
  Normalfall grundlos zu verzögern; gar nicht zu cachen hieße, im
  Benchmark-Fall mehrmals pro Sekunde den halben Baum zu erwandern.

- **Graph- und Work-Reiter folgen der Sitzung, nicht der Kommandozeile.** Ein
  einzelnes `--graph DIR` zeigte für jede Sitzung denselben Graphen — falsch,
  sobald eine Sitzung ein Benchmark-Task mit eigenem Graphen ist. Noch
  irreführender war `--work`: sein Default zeigt auf das Verzeichnis, in dem der
  BETRACHTER gestartet wurde, also bei einem Benchmark-Baum ins Leere, und die
  Oberfläche meldete dann „keine Work-Projekte" für einen Ort, der mit der
  gewählten Sitzung nichts zu tun hat. Beide werden deshalb aus der Ablage
  abgeleitet, die ein Lauf ohnehin hat (`<irgendwo>/trace/trace-*.jsonl` neben
  `<irgendwo>/graph` und `<irgendwo>/work`), mit den Flags als Rückfall. Im
  gewöhnlichen Fall trifft die Regel `.agentkit/trace` neben `.agentkit/graph`
  und `.agentkit/work` — die Flags werden dort überflüssig.

- **Polling statt Server-Sent Events.** *(Abweichung vom Plan
  `docs/plans/agentkit-viz-plan.md`, der SSE vorsah.)* SSE bräuchte in tiny_http
  einen Thread je offener Verbindung plus einen von Hand geschriebenen
  HTTP-Rahmen und dessen Abbau-Logik. Der Endpunkt `/api/events?since=N` liefert
  denselben Offset-basierten Nachschub, den auch das Tailen der Datei benutzt;
  der Browser fragt im Sekundentakt. Auf localhost ist der Unterschied nicht
  wahrnehmbar, die Maschinerie dagegen deutlich kleiner.

- **Der Server ist einfädig und liest je Anfrage nach.** Jede Antwort ist ein
  JSON aus dem Speicher; ein Threadpool wäre Maschinerie ohne Nutzen. Weil das
  Nachlesen der Trace-Datei in derselben Anfrage passiert, gibt es keinen
  Hintergrund-Thread und keinen zweiten Codepfad für „live" — nachträglich laden
  ist dasselbe, nur einmal statt oft. Das Token wird VOR dem ersten Dateizugriff
  geprüft: ein Aufrufer ohne Zugang soll den Server nicht dazu bringen können,
  das Dateisystem anzufassen.

- **Der Browser hängt an, statt neu zu zeichnen.** `/api/events?since=N` liefert
  denselben Nachschub in derselben Form wie der Verlauf (inklusive `label`), und
  die Ansicht bekommt ihn angehängt. Ein Neuaufbau im Sekundentakt würde jedes
  gerade aufgeklappte Tool-Ergebnis wieder zuschnappen lassen — also genau dann
  versagen, wenn das Werkzeug benutzt wird. Vollständig neu gezeichnet wird nur
  bei einem Wechsel von Reiter, Agent oder Trace-Datei.

- **Eine einmal gewählte Trace-Datei bleibt gewählt.** Beim Start nimmt der
  Betrachter die jüngste; danach wechselt er nur noch auf ausdrücklichen Wunsch
  (`run=<name>`, im Kopf als Auswahlliste, sobald es mehr als eine gibt). „Immer
  die jüngste" sah einfacher aus, war aber falsch: laufen zwei Agenten im selben
  Verzeichnis, wechselte der Betrachter im Sekundentakt zwischen ihren Dateien
  hin und her und las jedes Mal alles neu ein. Der Browser hängt seinen Stand
  ebenfalls an den DATEINAMEN, nicht an die Sequenznummer — zwei Läufe fangen
  beide bei 1 an.

- **Die Ereignis-Kopfzeile wird EINMAL formuliert, in Rust.** `label_of` liefert
  sie für Zeitleiste, Verlauf und Nachschub; der Browser wählt nur noch die
  Farbe. Sonst müsste jeder neue Ereignistyp in zwei Sprachen nachgezogen werden.

- **Eine Seite, alles inline.** Stil und Skript werden beim Ausliefern in die
  HTML-Datei gelegt. Nicht aus Sparsamkeit: ein relatives `<script src>` trüge
  die Token-Query nicht mit, ein zweiter Request wäre also ein zweiter Weg an
  der Zugangsprüfung vorbei.

- **Die Schwarm-Sicht deutet JSON, keine Typen.** `swarm.rs` liest die
  `swarm_event`-Nutzlast über Feldnamen (`message_queued.message.from`, …), statt
  `agentkit_swarm` zu importieren — dieselbe Unabhängigkeit wie beim Trace-Format,
  und tolerant: ein fehlendes Feld bleibt leer, statt den Datensatz zu verwerfen.
  Zwei Dinge zeigt sie, die in keinem `SwarmResult` stehen: **wer abgelehnt hat**
  (das Ergebnis führt nur Zustimmungen) und der Verkehr, der nie zugestellt wurde.
  Umgekehrt gilt eine mit `swarm_completed` abgelehnte Zustellung NICHT als
  Verlust — die Laufzeit legt dafür bewusst keinen Dead Letter an, und ein
  sauberer Abschluss soll nicht wie eine Panne aussehen.

- **Das Sequenzdiagramm ist handgeschriebenes SVG.** Linien, Dreiecke, Text —
  kein Diagramm-Framework, kein vendorter JS-Blob im Repo. Broadcasts enden in
  einem Balken rechts statt in einem Pfeil ins Nichts; abgelehnte Zustellungen
  sind gestrichelt und tragen ihren Grund.

- **Das Graph-Layout ist eine handgeschriebene Kraftsimulation**
  (Fruchterman-Reingold: Abstoßung k²/d, Anziehung d²/k, fallende Temperatur)
  — kein d3, kein cytoscape, kein vendorter Blob. Zwei Dinge, die dabei
  wichtig sind und nicht offensichtlich: die Startlage liegt auf einem KREIS
  statt zufällig (dieselbe Eingabe ergibt dasselbe Bild — ein Layout, das bei
  jedem Nachladen anders aussieht, taugt nicht zum Vergleichen), und
  eingepasst wird ERST ZUM SCHLUSS statt während der Simulation zu klemmen
  (geklemmte Knoten kleben am Rand fest, und das Bild wird zum Rahmen statt
  zum Graphen). Wird es zu langsam, ist eine Bibliothek der dokumentierte
  nächste Schritt.

- **Kein npm, kein Framework.** Eine `index.html`, eine `app.js` mit `fetch`.
  Wer das Werkzeug erweitern will, soll es lesen können, ohne einen Build zu
  starten — und das Repo soll keinen vendorten JS-Blob tragen.

- **Die Rekonstruktion ist nur noch die Rückfallebene.** Seit agentkit 0.14 legt
  JEDER Agent am Ende seines Laufs seinen Kontext als `context_snapshot` auf den
  Bus (`Agent::run_on_bus`) — auch Sub-Agenten, Schwarm-Mitglieder und
  Work-Item-Versuche. Vorher konnte nur die CLI diesen Datensatz schreiben, und
  nur für den Haupt-Agenten. Fehlt er (älterer Trace), baut der Betrachter aus
  dem Ereignisstrom nach, was der Agent *getan* hat — System-Prompt und
  Verdichtungen fehlen dann. Die Ansicht sagt das ausdrücklich
  (`rekonstruiert`), statt eine Vollständigkeit zu behaupten, die sie nicht hat.

- **`tiny_http` ist die einzige neue Fremd-Dependency.** Synchron (Repo-Konvention:
  kein tokio), ohne TLS, vier kleine Transitive. Sie landet nie im Kern und —
  weil `viz` kein Feature der Release-Binaries ist — auch nicht in den
  veröffentlichten Artefakten.

## Grenzen des heutigen Stands

- **Der Work-Reiter liest sein Journal bei jedem Takt vollständig neu.**
  `WorkStore::open_read_only` spielt das ganze Journal ab; einen Offset-Tail wie
  beim Trace gibt es dort nicht. Deshalb zieht der Betrachter den Work-Zustand
  NUR nach, solange sein Reiter offen ist. Bei einem Vorhaben mit einem
  megabytegroßen Journal ist das spürbar; der nächste Schritt wäre ein
  inkrementeller Leser — und der gehört nach `agentkit_work`, nicht hierher.
- **Aus älteren Traces bleibt der Kontext fremder Agenten unvollständig**
  (siehe oben, „rekonstruiert").
- **Die TUI schreibt keinen Trace.** `agentkit --tui --trace DIR` warnt und
  schreibt nichts; der Mitschnitt hängt am `EventBus`, das Einhängen im TUI ist
  eine offene Kleinigkeit.

## Was bewusst fehlt

- **Kein Schreibzugriff.** Der Betrachter beobachtet; er startet, stoppt und
  ändert nichts. Ein „Freigabe erteilen"-Knopf wäre verlockend, macht aus einem
  Leseloch aber ein Schreibloch.
- **Keine Multi-User-/Remote-Fähigkeit**, keine Auth über das Loopback-Token hinaus.
- **Kein Ersatz für die TUI.** Die bleibt das Werkzeug für den interaktiven Lauf;
  der Betrachter ist für Analyse und lange Läufe.

## Tests

```bash
cargo test --manifest-path agentkit_viz/Cargo.toml
cargo test --manifest-path agentkit_viz/Cargo.toml --features "work graph"
node --check agentkit_viz/src/assets/app.js   # das Frontend
```

Die letzte Zeile ist keine Zierde: `app.js` wird per `include_str!` eingebettet
und ausgeliefert, ein Syntaxfehler darin fällt in KEINEM Rust-Test auf — die
Seite bleibt einfach leer. Wer `app.js` anfasst, prüft sie damit.

Kein Test geht ins Netz: die Projektionen laufen gegen eine Fixture-Datei, die
Server-Tests gegen einen selbst gestarteten Server auf `127.0.0.1` — mit einem
`TcpStream` als HTTP-Client, damit auch dafür keine Dependency nötig ist.
