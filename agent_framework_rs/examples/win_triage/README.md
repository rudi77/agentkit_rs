# Windows-Incident-Triage mit agentkit — Fan-out, Korrelation und ein Sicherheitsnetz

„**Warum ist die Kiste heute Nacht neu gestartet?**"

Diese Frage ist der Grund, warum Admins morgens Kaffee brauchen. Die Antwort steht im
Event-Log — verteilt auf vier Logs, versteckt zwischen zehntausend Routine-Ereignissen, und
sie besteht nicht aus einer Zeile, sondern aus einer **Kette**. `Select-String` findet Zeilen.
Ketten findet es nicht.

Dieses Beispiel baut die Antwort als **Unix-Pipeline aus einzelnen agentkit-Agenten**: ein
Spezialist pro Subsystem (parallel), ein Korrelator darüber, und ein Reparatur-Agent, der
**unter `--dry-run` läuft und deshalb nichts kaputt machen kann**.

> ⚠️ Ein Demo. Der Reparaturvorschlag wird **nie** automatisch ausgeführt — siehe
> [Das Sicherheitsnetz](#das-sicherheitsnetz-dry-run).

## Die Pipeline

```
Get-WinEvent / Get-Service / Get-CimInstance      ← deterministisch, kein LLM
        │
        │   00_*.json   (170 Rohereignisse → 24 verdichtete)
        ▼
┌───────────────┬───────────────┬───────────────┬───────────────┐
│  system       │  security     │  application  │  update       │   ← VIER agentkit-Agenten,
│  Kernel,      │  Anmeldungen, │  Abstürze,    │  Patches,     │      gleichzeitig, je ein
│  Treiber,     │  Sperrungen   │  Hänger       │  Treiber      │      eigener Prozess
│  Dienste      │               │               │               │
└───────┬───────┴───────┬───────┴───────┬───────┴───────┬───────┘
        │  1x_*.json (Befunde je Subsystem, JSON)       │
        └───────────────┴───────┬───────┴───────────────┘
                                ▼
                         Korrelation            ← verbindet, was zusammengehört,
                         20_correlation.json       und TRENNT, was nicht zusammengehört
                                │
                                ▼
                         Reparatur-Agent        ← läuft unter --dry-run:
                         30_remediation.json       kann lesen, aber NICHTS verändern
                         remediation.ps1        ← Vorschlag. Nicht ausgeführt.
                                │
                                ▼
                         Bericht (40_report.md)
                                │
                                ▼
                         Mensch liest, gibt frei:  -Apply
```

Jede Stufe ist ein eigenständiges Kommando; der Datenfluss läuft über **stdin/stdout** mit
`--format json` als Vertrag. Keine Stufe weiß, wer vor oder hinter ihr steht.

## Warum vier Agenten und nicht einer?

Weil **Spezialisierung und Trennung** hier die Arbeit machen:

- **Kontext.** Ein einzelner Agent müsste alle vier Logs gleichzeitig im Kopf behalten. Vier
  Agenten sehen je einen Ausschnitt und bleiben scharf.
- **Parallelität.** Vier Prozesse, vier gleichzeitige Modell-Aufrufe. Die Stufe dauert so lang
  wie ihr langsamster Agent, nicht wie die Summe.
- **Unabhängigkeit ist ein Feature.** Der Sicherheitsanalyst weiß **nichts** vom Bluescreen.
  Er kann den Brute-Force-Versuch also gar nicht erst fälschlich damit verknüpfen. Erst der
  Korrelator sieht alles — und muss die Verknüpfung *begründen*. Genau das ist der Fehler, den
  Menschen um 8 Uhr morgens machen: zwei Dinge derselben Nacht zu einer Geschichte verschmelzen.

Der Korrelator muss deshalb ein Pflichtfeld füllen: `verworfene_verknuepfungen` — was sah
verdächtig aus und wurde **bewusst nicht** verknüpft.

## Das Sicherheitsnetz: `--dry-run`

Ein Agent, der eine kaputte Produktivmaschine „repariert", ohne dass ein Mensch draufgeschaut
hat, ist eine schlechte Idee. Deshalb läuft Stufe 30 mit **`--dry-run`**.

agentkit blockiert dann alle Werkzeuge, deren Name auf einen verändernden Vorgang hindeutet
(`is_likely_destructive` in [`src/tools.rs`](../../src/tools.rs)) — sie werden zu No-Ops, die nur
zurückmelden, dass sie blockiert wurden. Betroffen sind `run_shell`, `write_file`, `edit_file`.
**Die Tool-Schemas bleiben identisch**, das Modell „sieht" also denselben Werkzeugkasten — es
kann nur nichts damit anrichten.

Der Agent kann das System also **lesen, aber nicht anfassen**. Sein Reparaturvorschlag erreicht
die Maschine nur über eine Datei, die ein Mensch liest und freigibt:

```powershell
.\Invoke-WinTriage.ps1 -Apply     # zeigt remediation.ps1, fragt nach, führt erst dann aus
```

**Der Prompt bittet darum. `--dry-run` erzwingt es.** Das ist der Unterschied, auf den es
ankommt — und er ist geprüft, nicht behauptet: [`tests/Test-DryRunNet.ps1`](tests/Test-DryRunNet.ps1)
fordert den Agenten **ausdrücklich auf**, eine Datei zu schreiben und eine Shell zu starten, und
besteht nur, wenn danach nichts passiert ist. So sieht das im Trace aus:

```
⏺ write_file(content=HALLO, path=beweis.txt)
  ⎿ [dry-run] 'write_file' NICHT ausgeführt — zerstörerischer Schreibvorgang blockiert.
⏺ read_file(path=beweis.txt)
✖ Fehler in read_file: The system cannot find the file specified.
```

Er hat es versucht, wurde blockiert, hat nachgesehen — und ehrlich `{"getan": false}` geantwortet.

> **Zwei Eigenheiten, die man kennen muss.**
> 1. Die Heuristik geht über den **Namen**. `update_plan` enthält „update" und wird deshalb
>    ebenfalls blockiert — harmlos (der Plan ist nur Kosmetik), aber es taucht im Trace auf.
> 2. **`-p` schaltet die Werkzeug-Spur ab.** Im Print-Modus ist der Renderer stumm (`quiet`),
>    auch mit `--steps`. Wer die Blockaden sehen will, lässt `-p` weg und nutzt
>    `--format json --steps`: stdout bleibt trotzdem sauber (dafür sorgt `--format json`), die
>    Spur landet auf stderr. Genau so macht es Stufe 30.

## Ausführen

Voraussetzung: agentkit gebaut (`cargo build --release` im Ordner `agent_framework_rs`) und
LLM-Credentials in einer `.env` (`AZURE_OPENAI_*` oder `OPENAI_API_KEY`) — wie in den anderen
Beispielen.

```powershell
# Der mitgelieferte Beispiel-Störfall — läuft überall, auch ohne Adminrechte:
.\Invoke-WinTriage.ps1 -UseFixtures

# Der echte Rechner, letzte 24 Stunden:
.\Invoke-WinTriage.ps1

# Drei Tage, danach den Reparaturvorschlag prüfen und freigeben:
.\Invoke-WinTriage.ps1 -Hours 72 -Apply
```

**Echtes System mit Fallback.** Die Pipeline liest das echte Event-Log. Ist ein Log nicht
zugänglich (das **Security-Log verlangt Administratorrechte**), ist es leer, oder läuft das
Ganze gar nicht unter Windows, greift sie **pro Subsystem** auf die mitgelieferten Fixtures
zurück und sagt in der Ausgabe, was sie gerade nutzt (`[live]` / `[fixture]`). Die Demo ist damit
überall vorführbar, ohne zu schwindeln.

**Exit-Code.** `0` = nichts Kritisches, `1` = kritischer Vorfall. Damit taugt das Skript als
geplanter Task: Exit ≠ 0 heißt eskalieren.

## Der Beispiel-Störfall (Fixtures)

`fixtures/*.json` erzählen einen zusammenhängenden Vorfall auf dem fiktiven Server `SRV-WWS-01`
— erzeugt von [`tools/Build-Fixtures.ps1`](tools/Build-Fixtures.ps1):

| Zeit | Was |
|---|---|
| 02:14 | Kumulatives Update KB5061980 — **harmlos** (der Ablenker) |
| 03:47 | Treiber-Update **KB5062170**: Intel RAID/VMD, `iaStorVD.sys` |
| 04:07 | 3× Gerätereset auf `\Device\RaidPort1` — der Vorbote |
| 04:09 | **Bluescreen** `0xD1`, fehlerhaftes Modul `iaStorVD.sys` → harter Neustart |
| 04:11 | `MEMORY.DMP` (9,7 GB) geschrieben → **C: nur noch 1,3 % frei** |
| 04:12 | `postgresql-x64-16` startet nicht (Timeout) → `WWS-AppServer` hängt daran |
| 04:14+ | `WWS-AppServer` stürzt **43×** alle 5 Minuten ab (`NpgsqlException`) |

Dazu, **völlig unabhängig**: 01:47–02:04 ein Brute-Force gegen `administrator` von
`198.51.100.42` (96 Fehlversuche, abgewehrt, Konto gesperrt). Plus Routine-Rauschen.

Zu lösen ist damit dreierlei: die **Kette** finden, die **Rückkopplung** erkennen (das Abbild ist
Folge des Absturzes *und* Ursache des vollen Datenträgers — deshalb hilft ein simpler Neustart
nicht), und den Brute-Force **nicht** anzuflanschen.

> **Die Prompts verraten die Antwort nicht.** Alle Beispielwerte in `prompts/*.md` sind
> Platzhalter (`<Zeit>`, `<KB>`, `<Modul>`) — kein einziger echter Wert aus den Fixtures steht
> darin. Ein Agent, der `iaStorVD.sys` nennt, hat die Ereignisse **gelesen**; er kann es nicht
> abgeschrieben haben. Ohne diese Trennung wäre die Demo wertlos.

## Was dabei herauskommt

Aus 170 Rohereignissen werden **zwei** Vorfälle (aus dem echten Lauf, gekürzt):

> **V1 — kritisch:** Instabiler Intel-RAID/VMD-Treiber `iaStorVD.sys`, zeitlich nach KB5062170.
> Kette: Treiber-Update 03:47 → Geräteresets 04:07 → Bugcheck `0xD1` 04:09 → `MEMORY.DMP` 9,7 GB
> → C: voll → PostgreSQL startet nicht → WWS-AppServer stürzt seither im 5-Minuten-Takt ab.
> *Warum ein Neustart nicht reicht:* das Abbild belegt den Platz, den der Dienststart braucht.
>
> **V2 — hoch:** Brute-Force gegen `administrator` von `198.51.100.42`, abgewehrt, Konto gesperrt.
>
> **Bewusst nicht verknüpft:** Der Brute-Force liegt am selben Morgen wie der Absturz, es gibt
> aber keinen Beleg für eine Verbindung. Auch das kumulative Update KB5061980 wurde geprüft und
> verworfen.

Und ein Reparaturskript, das das Abbild **verschiebt statt löscht** (es ist die einzige Spur zur
Ursache), die Dienste in der richtigen Reihenfolge startet, idempotent ist — und den
Treiber-Rollback bewusst dem Menschen überlässt.

## Tests

```powershell
# Deterministisch, offline, ohne LLM: Fixture-Fallback, Normalisierung, Verdichtung
pwsh -File .\tests\Test-TriageHelpers.ps1      # 16 Prüfungen

# Das Sicherheitsnetz — braucht ein echtes Modell (wird ohne Credentials übersprungen):
pwsh -File .\tests\Test-DryRunNet.ps1          # 7 Prüfungen
```

Der erste Test prüft unter anderem die **Verdichtung**: 96 identische Anmeldeversuche werden zu
*einem* Befund mit `anzahl: 96` und Zeitraum. Das ist deterministische Arbeit — dafür braucht es
kein Modell, und ungefiltert würden die Rohereignisse den Kontext auffressen.

## Dateien

```
win_triage/
  Invoke-WinTriage.ps1          # Orchestrator (Sammeln → Fan-out → Korrelation → Reparatur → Bericht)
  modules/triage-helpers.ps1    # Adapter: Event-Log ODER Fixture → dieselbe normalisierte Form
  prompts/
    10_system.md   11_security.md   12_application.md   13_update.md   # die vier Spezialisten
    20_correlate.md                                                    # verbinden UND trennen
    30_remediation.md                                                  # läuft unter --dry-run
    40_report.md                                                       # Markdown für Menschen
  fixtures/                     # der Beispiel-Störfall (generiert, eingecheckt)
  tools/Build-Fixtures.ps1      # erzeugt die Fixtures
  tests/
    Test-TriageHelpers.ps1      # offline, kein LLM
    Test-DryRunNet.ps1          # beweist, dass --dry-run hält
  out/                          # alle Artefakte (generiert)
```

## Was dieses Beispiel zeigt

- **Fan-out auf Shell-Ebene.** Vier Agenten, vier Prozesse, gleichzeitig — Komposition im
  Orchestrator, nicht im Agenten versteckt. (`ForEach-Object -Parallel`, PowerShell 7+;
  unter PowerShell 5 läuft es sequenziell weiter.)
- **`--dry-run` als Sicherheitsnetz**, wenn ein Agent Vorschläge für ein Produktivsystem macht.
- **Das richtige Werkzeug pro Schritt.** Ereignisse sammeln, normalisieren und verdichten ist
  deterministische Arbeit (PowerShell). Aus 24 Befunden eine Ursachenkette machen — und die
  falsche Verknüpfung *nicht* zu machen — ist Urteilsarbeit (LLM).
- **Trennung als Qualitätsmerkmal.** Nicht alles zu verbinden ist schwerer, als alles zu
  verbinden — und wertvoller.
- **Der Mensch entscheidet.** Der Agent schlägt vor; die Maschine wird nur angefasst, wenn
  jemand `ja` tippt.

## Bezug zum Rest

- Das [Accounts-Payable-Demo](../accounts_payable/README.md) zeigt dieselbe Komposition auf einer
  Batch-Pipeline über Dokumente; hier läuft sie über einen **Live-Systemzustand**.
- `--dry-run`, `--format json`, `--steps` und die Exit-Codes sind allgemeine agentkit-Fähigkeiten
  — siehe [Benutzerhandbuch](../../docs/USER_MANUAL.md).
