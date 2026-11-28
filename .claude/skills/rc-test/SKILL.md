---
name: rc-test
description: Einen agentkit-Release-Kandidaten bauen lassen, auf diesem Rechner installieren und durchtesten — offline und mit echtem Modell. Verwenden, wenn ein RC oder Release getestet, abgenommen oder vor einem stabilen Tag geprüft werden soll.
---

# Release-Kandidat testen

Ziel ist eine belegte Antwort auf eine Frage: Kann dieser Kandidat als stabiler
Release raus? Dafür zählt, was beobachtet wurde. Jede Zeile im Bericht nennt
entweder ein Ergebnis oder sagt ausdrücklich „nicht getestet“.

Dieser Skill wird nach jedem Durchlauf nachgezogen, siehe „Pflege“ am Ende.

## 1. Vorher klären

- **Kandidat oder stabiler Release?** Vor dem Taggen die Versionsnummer
  nennen und bestätigen lassen. „Einen Release bauen“ meint in der Regel den
  stabilen Tag, nicht den nächsten Kandidaten.
- **Was ist neu?** `git log --oneline --merges <letzter-tag>..origin/main`. Der
  Kern des Tests sind die Änderungen seit dem letzten Kandidaten; die Liste in
  Abschnitt 5 ist der feste Sockel, nicht das ganze Programm.
- **Offene PRs:** `gh pr list`. Für jeden entscheiden lassen, ob er in den
  Kandidaten gehört. Nichts stillschweigend draußen lassen.
- **Version:** steht in `agentkit_app/Cargo.toml` und
  `agent_framework_rs/Cargo.toml`, dazu in sechs Lockfiles (`agentkit_app`,
  `agent_framework_rs`, `agentkit_graph`, `agentkit_swarm`, `agentkit_viz`,
  `agentkit_work`). Der Commit heißt `release: vX.Y.Z-rc.N` und geht direkt auf
  `main`; danach der Tag gleichen Namens. Taggen und Pushen nur auf
  ausdrücklichen Auftrag.

## 2. Build abwarten

Der Tag startet `.github/workflows/release.yml`. Mit `gh run watch <id>
--exit-status` im Hintergrund warten, nicht in einer Schleife abfragen. Erwartet
sind acht grüne Jobs und acht Assets, darunter `agentkit-skills.json` und
`agentkit-plugins.json`.

## 3. Installieren

`scripts/agentkit_setup.ps1 -Version <tag> -NoCompletions`.

- **Laufende Sitzungen:** Das Skript bricht mit „agentkit läuft gerade“ ab,
  wenn die installierte Exe selbst läuft. Ein agentkit aus einem anderen
  Verzeichnis (etwa eine TUI-Sitzung aus `target\release`) stört nicht. Eine
  laufende Sitzung gehört dem Nutzer und wird nicht beendet: ihn bitten, sie zu
  schließen, oder die Assets mit `gh release download <tag>` in ein eigenes
  Verzeichnis holen und von dort testen.
- `~/.agentkit/config.json` darf sich durch die Installation nicht ändern.
- Danach `agentkit --version` prüfen.

## 4. Netz prüfen, bevor ein Modell gefragt wird

```
pwsh .claude/skills/rc-test/netz-pruefen.ps1 -Minutes 2
```

Meldet das Skript Ausfälle des Endpunkts bei erreichbarer Kontrolle, sind
„Netzwerkfehler“ in den folgenden Läufen kein Befund gegen den Kandidaten. Dann
entweder warten oder die Modell-Tests ausdrücklich als „wegen Netz nicht
bestätigt“ führen.

Kennzeichen im Fehlertext: `os error 10060` ist ein gescheiterter
TCP-Verbindungsaufbau, es wurde nie HTTP gesprochen. Ein `HTTP 4xx` dagegen ist
eine Antwort des Anbieters und gehört untersucht.

Das Skript liest den Endpunkt aus `AZURE_OPENAI_ENDPOINT`; für andere Anbieter
`-Endpoint HOST` angeben. Meldet es, dass der Name nicht auflösbar ist oder die
Kontrolle nie erreichbar war, ist nichts gemessen — dann mit `-Control` ein
anderes Ziel wählen.

Stand 27.11.2026: Auf dem Testrechner lief der Verkehr zum Endpunkt über ein
VPN und fiel dort im Wechsel rund 90 s aus, während direkt geroutete Ziele
erreichbar blieben. agentkit gibt nach etwa 65 s auf (drei Verbindungsversuche).

## 5. Fester Sockel

### Ohne Modell

```
bash .claude/skills/rc-test/pakete-pruefen.sh "$(which agentkit)" <skills.json> <plugins.json>
```

Das Skript arbeitet in einem Wegwerf-`AGENTKIT_HOME` und prüft die Verben
`skills` und `plugins`, zwölf präparierte Pakete und die Prüfung beim Laden.
Erwartet ist „Alle Prüfungen bestanden“.

Dazu von Hand:

- **cli-Variante:** `--version`, und `--tui` muss mit „kein TUI“ abweisen.
- **`pack`:** `agentkit skills pack agentkit_app/skills` und `plugins pack
  agent_framework_rs/examples/dokumente/plugin` mit den Release-Assets
  vergleichen. Unter Windows weichen Dateien mit CRLF im Checkout ab; das ist
  der Checkout, kein Fehler von `pack`.
- **MCP-Server:** `printf '/mcp\n/exit\n' | agentkit --repl --no-graph` muss
  die Server aus `~/.agentkit/.mcp.json` als verbunden zeigen. „program not
  found" heißt ab 0.33.0: der Starter liegt nicht im `PATH` dieser Shell. Die
  Shell des Tests kann einen anderen `PATH` haben als das Terminal des Nutzers;
  vor einem Befund dort gegenprüfen. „Verbindung geschlossen“ bei einem
  `npx`-Server heißt meist, dass `npx` sein `node` nicht findet.
- **Installation per Name** (nur nach einem stabilen Tag): in einem
  Wegwerf-`AGENTKIT_HOME` `agentkit skills install diff-review` und `agentkit
  plugins install dokumente`; beide müssen aus dem Release laden.
- **ACP über TCP:** `acp --listen 127.0.0.1:PORT -w DIR` mit
  `AGENTKIT_ACP_TOKEN`, dann über `acp --connect` ein `initialize` und
  `session/new` schicken. Ein falsches Token muss mit Exit 2 enden.

### Mit echtem Modell

Arbeitsverzeichnis im Scratchpad anlegen, nie im Repo. Als Testdokument dient
ein PDF mit Gliederung und mindestens 50 Seiten; die Beispiele unten nennen es
`tvl.pdf` (ein Tarifvertrag). Liegt keines bereit, den Nutzer danach fragen.

Die Modell-Tests brauchen Skills und Plugin im echten `~/.agentkit`, weil
`pakete-pruefen.sh` nur in sein Wegwerf-Verzeichnis installiert:

```
agentkit skills install <agentkit-skills.json>
agentkit plugins install <agentkit-plugins.json>
```

Beides bleibt danach installiert und wirkt in jedem Lauf des Nutzers. Das
gehört in den Bericht (Abschnitt 6, „Eigener Zustand“).

| Prüfung | Aufruf | Erwartet |
|---|---|---|
| Plugin „dokumente“ | `agentkit run dokumente -y --no-graph "Wie lange dauert die Probezeit nach tvl.pdf …"` | sechs Monate, § 2 Abs. 4, mit Fundstelle; Spur zeigt `doc_ingest`, dann `grep`/`read_file` auf Abschnitten |
| `doc_ingest` wiederholt | dieselbe Datei noch einmal | „bereits zerlegt und unverändert“ |
| `doc_ingest` nach Änderung | Quelle ergänzen, noch einmal | „hat sich geändert“, neuer Abschnitt vorhanden, eigene Datei im Bundle bleibt |
| `doc_ingest` absoluter Pfad | dieselbe Datei absolut | gilt als dieselbe Quelle |
| `doc_ingest` fremdes Verzeichnis | Zielordner mit fremdem Inhalt | `ERROR`, nichts gelöscht |
| Skills mitladen | „Welche Skills stehen dir zur Verfügung?“ | alle installierten; mit `--tools none` keine; über `agentkit run dokumente` nur die zwei des Plugins |
| `--skills DIR` | eigenes Verzeichnis | dessen Skill erscheint |
| Skill in Benutzung | `diff-review` auf einem Repo mit eingebautem Fehler | der Fehler wird genannt |
| Denktiefe | `--reasoning-effort low`, dann ein ungültiger Wert | Antwort; dann klare Meldung und Exit 2 |
| ACP-Prompt | `agentkit run dokumente acp --listen …`, `session/prompt` über die Brücke | Antworttext und Abschluss mit `stopReason` |

Fundstellen nachprüfen: Die zitierte Stelle in den Abschnittsdateien unter
`.agentkit/docs/<name>/` suchen. Eine nicht nachgeprüfte Seitenzahl im Bericht
als solche kennzeichnen.

## 6. Fallen beim Testen

- **stdin:** Jeder nicht interaktive Aufruf braucht `</dev/null`, sonst liest
  agentkit stdin bis EOF und hängt.
- **Exit-Code:** direkt nach dem Aufruf sichern. Ein `echo` oder eine Pipe
  dazwischen überschreibt ihn (`${PIPESTATUS[0]}` verwenden).
- **Stdout und Stderr trennen:** Die Antwort steht auf stdout, die Werkzeugspur
  auf stderr. Beide in eigene Dateien schreiben, sonst geht bei einem Fehler
  die Ursache verloren.
- **„latest“ ist der letzte stabile Release.** `agentkit skills install NAME`
  ohne Pfad endet bei einem Kandidaten mit 404. Das ist erwartet und erst nach
  dem stabilen Tag prüfbar.
- **Kosten im Blick behalten:** Tokenzahl am Ende der Spur notieren. Startet
  eine einfache Frage Sub-Agenten, ist das ein Befund.
- **Eigener Zustand:** Was der Test in `~/.agentkit` hinterlässt (installierte
  Skills und Plugins), im Bericht nennen. Testserver am Ende beenden.

## 7. Bericht

Zuerst das Urteil in einem Satz: Kann der Kandidat raus, ja oder nein. Dann:

1. Tabelle der Prüfungen mit Ergebnis.
2. Auffälligkeiten, getrennt nach „Fehler des Kandidaten“ und „Umgebung“.
3. Nicht getestet, mit Grund.
4. Zustand auf dem Rechner.
5. Der nächste Schritt als Empfehlung.

## Pflege

Nach jedem Durchlauf, bevor die Sitzung endet:

- **Neues Feature im Kandidaten:** eine Zeile in den Sockel (Abschnitt 5), und
  wo es ohne Modell geht, eine `erwarte`-Zeile in `pakete-pruefen.sh` oder ein
  eigenes Skript daneben.
- **Neue Falle:** in Abschnitt 6, mit dem Kennzeichen, an dem man sie erkennt.
- **Überholtes:** streichen. Der datierte Netz-Stand in Abschnitt 4 gilt nur,
  solange er sich nachmessen lässt.
- **Offene Lücken** des letzten Durchlaufs stehen hier, bis sie geschlossen
  sind:
  - vollständiger ACP-Prompt (27.11.2026 am Netzausfall gescheitert)
  - TUI, die Linux-Binaries, volle Auswertung und Vergleich zweier Dokumente
  - das Setup-Skript, während die installierte Exe läuft

  Dieselbe Liste steht als Issue #121 im Projekt; dort abhaken, hier streichen.
