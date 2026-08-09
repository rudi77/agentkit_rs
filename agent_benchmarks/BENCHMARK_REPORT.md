# Benchmark-Report — agentkit 0.21.0

**Datum:** 2026-08-09 · **Modell:** Azure OpenAI, Deployment `gpt-5.4-mini` ·
**Rohdaten:** `D:\agent_bench_data\results` (nicht im Repo)

Zwei Messreihen an zwei Tagen:

- **Runde 1** (2026-08-08, agentkit 0.20/0.21-in-Arbeit): vier Arme über drei
  Benchmarks, 81 Task-Agenten je Arm — eine Bestandsaufnahme.
- **Runde 2** (2026-08-09, nach den Korrekturen): vier Arme über
  Terminal-Bench und SWE-bench Lite, 17 Task-Agenten je Arm.

Zwischen beiden liegen acht Korrekturen, die aus Runde 1 hervorgingen. Runde 2
misst, was sie gebracht haben. Immer höchstens zwei Agenten-Container
gleichzeitig, je Arm sequenziell.

## Ergebnisse

| Arm | Modus | Terminal-Bench (7) | SWE-bench Lite (10) |
|---|---|---|---|
| **`v2-solo`** | Einzelagent | **2/7** | **4/10** |
| `v2-work` | Work-Runtime | 1/7 | 3/10 |
| `v2-graph` | Einzelagent + Graph | 1/7 | 2/10 |
| `v2-swarm` | Tech-Lead + Team | 1/7 | 2/10 |

Zum Vergleich Runde 1 (SWE „roh" = wie gemessen, „bereinigt" = nachträglich um
Test- und Kladdedateien gefiltert):

| Arm | Terminal-Bench | SWE roh | SWE bereinigt | Polyglot (64) |
|---|---|---|---|---|
| `a-solo` | 2/7 | 3/10 | 4/10 | 54/64 (84,4 %) |
| `b-solo-graph` | 1/7 | 2/10 | 3/10 | 50/64 (78,1 %) |
| `c-swarm` | 1/7 | 3/10 | 3/10 | 55/64 (85,9 %) |
| `d-swarm-graph` | 1/7 | 2/10 | 3/10 | 55/64 (85,9 %) |

**Einordnung.** Bei 7 bzw. 10 Aufgaben trägt keine dieser Zahlen einen
Unterschied von einer Aufgabe. Historisch schwankte derselbe Aufbau auf
Terminal-Bench zwischen 1/7 und 4/7. Was trägt, sind die *Verhaltensänderungen*
darunter — sie sind groß, systematisch und über alle Tasks eines Arms gemessen.

Zum SWE-bench-Leaderboard: Dessen 70–77 % sind **Verified** (500 Instanzen,
Spitzenmodelle). Hier laufen **10 Instanzen Lite** mit einem kleinen Modell.
4/10 daneben zu stellen wäre unseriös.

---

## Was die Korrekturen bewirkt haben

### 1. Das frühe Aufgeben ist weg (`--verify` gehärtet)

Vorher löste **jedes** erfolgreiche `run_shell` die Prüfpflicht ein — ein `ls`
genügte. Jetzt zählt nur ein erkannter Testrunner, und eine Delegation setzt die
Pflicht ebenfalls (`agent.rs`, `coding::shell_ist_pruefung`).

| | `a-solo` (Runde 1) | `v2-solo` (Runde 2) |
|---|---|---|
| Ø Schritte je Task (TB) | 20,7 | **37,6** (+82 %) |
| Ø Sekunden je Task (TB) | 95 | **195** |
| `run_shell`-Aufrufe (TB) | 83 | **188** |

Der sichtbarste Einzelfall: **`build-pmars`**. In Runde 1 gab der Agent nach 17
Schritten auf — „the tool environment is missing the required build utilities:
wget, dpkg-source, make". In Runde 2 hat er dieselbe Aufgabe **gelöst**. Nicht
weil er mehr konnte, sondern weil er nicht mehr abschließen durfte, ohne etwas
vorzuweisen.

### 2. Testdateien im Diff: von 7 auf 0 (`--protect-paths`)

Runde 1 verlor 4 von 40 SWE-Instanzen daran, dass der Agent eine Testdatei
anfasste und der offizielle `test_patch` damit kollidierte — der ganze Patch
fiel durch, auch der korrekte Quellcode darin. Drei dieser Fixes waren
nachweislich richtig.

Jetzt weisen `write_file`/`edit_file` gesperrte Pfade ab, und der SWE-Treiber
filtert zusätzlich (`diff_saeubern`, protokolliert, was er wegnimmt):

| | `a-solo` | `v2-solo` |
|---|---|---|
| gefilterte **Testdatei**-Abschnitte | 7 | **0** |
| gefilterte Kladden | 3 | 4 |
| `patch_failed` | 2 | **0** |
| resolved | 3/10 (roh) | **4/10** |

Der Gewinn, den Runde 1 nur *nachträglich* zeigen konnte (3→4 durch manuelles
Filtern), fällt jetzt im Lauf selbst an: `v2-solo` ist instanzgenau identisch
mit dem bereinigten `a-solo`, ohne dass jemand nachbessert.

**Eine Lücke, in Runde 2 gemessen und danach geschlossen:** In `v2-work` wies
`write_file` die Datei `astropy/wcs/tests/test_wcs.py` ab — woraufhin derselbe
Agent sie per `python - <<'PY' … p.write_text(…)` schrieb. Die Sperre galt nur
für die Datei-Werkzeuge, und die Absage behauptete trotzdem, auch `run_shell`
sei gesperrt. Beides ist jetzt behoben (`shell_schreibt_geschuetztes`): Schreiben
über die Shell wird abgewiesen, **Lesen und Testen bleiben erlaubt** — eine
Sperre, die `pytest tests/test_x.py` verhindert, nähme dem Agenten die
Verifikation und schadete mehr, als sie nützt. Das ist implementiert und durch
Tests gedeckt, aber in den Zahlen oben noch **nicht** enthalten.

### 3. Der Graph teilt jetzt zuverlässig — und hilft trotzdem nicht

Runde 1 hatte einen Mechanismusfehler: Ohne `--session` fiel der Arbeits-Scope
auf `pid-<prozess-id>` zurück, und in frischen Containern kollidieren PIDs. 81
Task-Läufe verteilten sich auf **13 zufällige Wissensinseln** — `poker`,
`book-store`, `bowling` und `dot-dsl` liefen alle als PID 73 und lasen deshalb
einander, `grade-school` als PID 72 und sah nichts davon.

Mit `--graph-scope` ist es eine Zusage:

| | Runde 1 (`b-solo-graph`) | Runde 2 (`v2-graph`) |
|---|---|---|
| Scopes im Journal | **13** (`pid-45` … `pid-77`) | **1** (`v2-graph`) |
| `graph_promote` | 0 von 158 Gelegenheiten | nicht mehr nötig |
| Suchen mit Inhalt | 63/80 (teils Selbstfund) | 8/10 |

Damit ist die Frage beantwortet, die Runde 1 offenlassen musste: Der Rückstand
des Graph-Arms lag **nicht** am kaputten Mechanismus. Er teilt jetzt korrekt und
liegt weiter zurück (TB 1/7, SWE 2/10). Für in sich geschlossene
Benchmark-Aufgaben ist Wissen aus fremden Aufgaben offenbar kein Vorteil,
sondern Ablenkung plus Token-Kosten.

### 4. Der Swarm benutzt sein Team — und wird dadurch schlechter

Runde 1 hatte einen zweiten Mechanismusfehler, größer als der erste: **Der
Team-Prompt wurde nie übergeben.** `teamlead_bench.md` wurde in jeden Container
hochgeladen und zu `system_full.md` zusammengefügt — die dann in keinem
Kommando auftauchte, weil `--system-file` seit dem Umstieg auf den eigenen
System-Prompt des Agenten fehlte. Der „Tech-Lead" hatte also nie
Team-Instruktionen. Dasselbe galt für die Graph-Anleitung und die 84 Zeilen
Benchmark-Regeln.

Behoben: Die Team-Instruktionen stehen jetzt im **Auftrag** (dem einzigen Kanal,
der ankommt), und `--agents-only` entfernt die eingebauten Rollen, damit das
Modell nicht zu den vertrauten Namen greift.

| Delegationsziel | `c-swarm` (81 Tasks) | `v2-swarm` (17 Tasks) |
|---|---|---|
| `architect` | 2 | 19 |
| `developer` | 13 | 18 |
| Anteil Team-Rollen | **5,9 %** | **36 %** |

Das Verhalten hat sich also grundlegend geändert — und das Ergebnis wurde
schlechter: SWE 2/10 gegen 4/10 solo, mit **zwei `empty_patch`**.

---

## Der Befund, den Runde 2 hinzufügt: Wer delegiert, schreibt nicht

Über beide Runden hinweg ist dies der einzige Zusammenhang, der *nicht* im
Rauschen liegt. Aufgeschlüsselt, wer die Datei-Änderungen tatsächlich vornimmt:

| Arm | Schreibvorgänge | Orchestrator | Sub-Agent | `empty_patch` | SWE gelöst |
|---|---|---|---|---|---|
| **`v2-solo`** | 67 | **65 (97 %)** | 2 | **0** | **4/10** |
| `v2-graph` | 42 | 38 (90 %) | 4 | 1 | 2/10 |
| `v2-swarm` | 55 | 27 (49 %) | 28 (51 %) | 2 | 2/10 |
| `v2-work` | 22 | 0 | **22 (100 %)** | 1 | 3/10 |

Je weiter das Schreiben vom Orchestrator wegwandert, desto häufiger endet ein
Lauf mit einer Erfolgsmeldung ohne Änderung. Der Prototyp dieses Falls stammt
aus Runde 1 (`c-swarm/django-11019`): fünf Delegationen, alle an lesende Rollen,
leerer `git diff` — und eine Schlussantwort, die eine Änderung in
`django/forms/widgets.py` im Detail beschreibt, die es nicht gibt.

Die naheliegende Erklärung — der Orchestrator delegiert das Schreiben und
niemand fühlt sich zuständig — passt zu allen vier Armen. Sie ist aus 34
Instanzen aber nicht bewiesen, sondern nahegelegt; ein gezielter Test wäre ein
Arm mit `developer` als *einziger* schreibender Rolle und einer harten
Schlussprüfung „Antwort behauptet Änderung, Arbeitsbaum unverändert".

## Die Work-Runtime im ersten Test

Zum ersten Mal gemessen (`BENCH_WORK=1`), und der Befund ist eindeutig negativ
für Aufgaben dieser Größe:

| | `v2-solo` | `v2-work` |
|---|---|---|
| Terminal-Bench | **2/7** | 1/7 |
| SWE-bench | **4/10** | 3/10 |
| Ø Schritte je Agent | 34,8 | **13,4** |
| Werkzeugaufrufe gesamt | 1309 | **752** |
| SWE-Instanzen mit `exit 1` (max-steps) | 0 | **6 von 10** |

Der Grund liegt in der Konstruktion: Jeder Item-Versuch bekommt einen frisch
gebauten Agenten mit leerem Kontext. Das ist als Isolation gedacht und wirkt
hier als Amnesie — die Erkundung des Repos beginnt in jedem Item von vorn, und
`max_steps` gilt **je Versuch**, weshalb sechs von zehn Instanzen ins Limit
liefen, während der Einzelagent bei keiner einzigen anstieß. Im Durchstich auf
`hello-world` zerlegte die Runtime „schreibe hello.txt" in ein
Inspektionsprojekt und endete `blocked` (0,0 gegen 1,0 für den Einzelagenten).

Das widerlegt nicht die Erwartung, dass sie bei **langen** Vorhaben hilft — es
zeigt, dass eine SWE-bench-Lite-Instanz dafür zu kurz ist. Der Modus verdient
einen Test an einem Vorhaben, das ein einzelner Kontext nicht fasst.

## Korrekturen an Runde 1

Zwei Befunde des ersten Berichts waren richtig beobachtet, aber falsch begründet:

- **„Der Tech-Lead ignoriert seinen Team-Prompt."** Er hat ihn nie bekommen.
- **„`graph_promote`-Compliance ist 0 %."** Die Aufforderung dazu stand in einer
  Datei, die nie übergeben wurde. Das Modell wurde nie gefragt.

Beide Male war die Ursache dieselbe: tote Prompt-Verkabelung, die sich wie eine
Zusage las. Sie ist entfernt.

Ein dritter Befund hat sich beim Nachprüfen umgedreht: Der Graph war **nicht**
schreibgeschützt-nutzlos — Tasks lasen einander sehr wohl, nur über eine
PID-Kollision statt über den vorgesehenen Weg.

## Was jetzt zu tun wäre

| # | Vorschlag | Begründung |
|---|---|---|
| 1 | Schlussprüfung: „Antwort behauptet Änderung, `git diff` leer" → Einwurf | 4 `empty_patch` in Runde 2, alle mit zuversichtlicher Erfolgsmeldung |
| 2 | Im Team-Modus `developer` als EINZIGE schreibende Rolle | testet die Delegations-Hypothese gezielt |
| 3 | Work-Runtime: `max_steps` als Projekt-Budget statt je Versuch | 6 von 10 Instanzen im Limit |
| 4 | Work-Runtime an einem mehrstündigen Vorhaben messen | dort liegt ihr Zweck, nicht bei 10-Minuten-Aufgaben |
| 5 | Graph: nicht weiter an Benchmarks messen | zwei Runden ohne Vorteil, bei sauberem Mechanismus |
| 6 | Terminal-Bench-Subset vergrößern oder Wiederholungen fahren | 7 Aufgaben tragen keinen Unterschied |

Die Korrekturen selbst sind erledigt: Sub-Agenten-Regeln (`--sub-rules`),
Pfadschutz (Datei-Werkzeuge **und** Shell), gehärtetes `--verify`,
`--graph-scope`, `--agents-only`, `durable` bei `graph_remember`, Diff-Filter,
`eval_local`-Zitierung samt eigenem Status `eval_error`.

## Reproduktion

```bash
cd agent_benchmarks
make setup build-agent

# ein Arm (Beispiel: Einzelagent), sequenziell:
BENCH_WORK=0 BENCH_GRAPH=0 AGENTKIT_MAX_STEPS=100 \
  uv run harbor run -a agentkit_bench.harbor_agent:AgentkitAgent --n-concurrent 1 \
  -d terminal-bench@2.0 -i "hello-*" -i "csv-*" -i "git-*" -i "build-*" -i "regex-*" \
  -o "$BENCH_RESULTS_DIR/terminal_bench" --job-name v2-solo
uv run python -m agentkit_bench.swebench.run_swebench --limit 10 --workers 1 --run-id v2-solo
uv run python -m agentkit_bench.swebench.eval_local \
  --predictions "$BENCH_RESULTS_DIR/swebench/v2-solo/preds.jsonl" --run-id v2-solo

# zuschauen, während es läuft (Binary mit --features viz):
agentkit viz --trace "$BENCH_RESULTS_DIR" --open
```

## Frühere Läufe (dasselbe Modell)

| Lauf | Terminal-Bench (7) | Polyglot (64) |
|---|---|---|
| Erstlauf 2026-07-21 | 1/7 | 26/64 (40,6 %) |
| Smoke 2026-07-22 | 2/7 | 58/64 (90,6 %) |
| Volllauf 2026-08-01 | — | 43/64 (67,2 %) |
| Runde 1, bester Arm (2026-08-08) | 2/7 | 55/64 (85,9 %) |
| **Runde 2, bester Arm (2026-08-09)** | **2/7** | — |

Die Streuung zwischen diesen Läufen ist größer als jeder Unterschied zwischen
den Armen einer Runde. Wer künftig Varianten vergleichen will, braucht mehr
Aufgaben oder Wiederholungen je Arm — Polyglot mit 64 Aufgaben trägt
Unterschiede ab etwa 10 Punkten, Terminal-Bench mit 7 trägt keine.
