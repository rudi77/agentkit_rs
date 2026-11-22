# Benchmark-Report — agentkit 0.21.0, Nachtrag 0.33.0-rc.4

**Datum:** 2026-08-10 · **Modell:** Azure OpenAI, Deployment `gpt-5.4-mini` ·
**Rohdaten:** `D:\agent_bench_data\results` (nicht im Repo)

**Nachtrag:** 2026-11-22 · agentkit 0.33.0-rc.4 · **anderes Modell**
(`gpt-6.1-terra`) — siehe [Runde 5](#nachtrag-runde-5--agentkit-0330-rc4-anderes-modell).
Alles darunter ab „Ergebnisse Runde 3" ist der unveränderte Stand vom August.

## Nachtrag Runde 5 — agentkit 0.33.0-rc.4, anderes Modell

**Stand:** 2026-11-22 · **Modell:** Azure OpenAI, Deployment `gpt-6.1-terra` ·
**Rohdaten:** WSL, `~/bench/agent_benchmarks/results` (Jobs `v6-*` bis `v9-*`,
`gold-v7`; nicht im Repo)

Gemessen wurden die drei Wege, die seit August dazugekommen oder gewachsen sind:
`-s plan` als Basislinie, der Treiber `-s plan_execute` und der Work-Modus
(`BENCH_WORK=1`, Item-Agenten mit `--strategy plan`). Dieselben 89 Aufgaben wie
in Runde 3 und 4, Graph und Swarm aus.

| Arm | Polyglot (64) | SWE-bench Lite (25) | zusammen |
|---|---|---|---|
| **`-s plan`** | **64/64** | **18/25** | **82/89 — 92,1 %** |
| `-s plan_execute` | 63/64 | 17/25 | 80/89 — 89,9 % |
| Work-Modus | 63/64 | 16/25 | 79/89 — 88,8 % |

Eingabe-Tokens je Aufgabe (Mittel):

| Arm | Polyglot | SWE-bench Lite |
|---|---|---|
| `-s plan` | 185.000 | 545.000 |
| `-s plan_execute` | 229.000 | 460.000 |
| Work-Modus | 141.000 | 409.000 |

**Was das trägt und was nicht.**

- **Die Arme liegen gleichauf.** Zwei bis drei Aufgaben Abstand bei 89 sind
  Rauschen (Befund 4). 16 SWE-Instanzen lösen alle drei Arme. Polyglot trennt
  bei diesem Modell nicht mehr: 64, 63, 63.
- **Nicht mit Runde 4 vergleichbar.** Gegenüber 67/89 haben sich drei Dinge
  zugleich geändert: das Modell (`gpt-5.4-mini` → `gpt-6.1-terra`), der
  SWE-Auswerter (siehe unten) und agentkit. Die Gegenprobe mit
  `gpt-5.4-mini` steht weiter unten (`v10`).
- **Nicht ganz gleichzeitig.** `plan` und Work stammen aus demselben Lauf
  (`v8-*`), `plan_execute` aus einem eigenen Lauf danach (`v9-planexec`), weil
  dort nach `v8` noch eine Korrektur dazukam. Das verstößt gegen die Regel aus
  Runde 4 und ist ein weiterer Grund, die Abstände nicht zu deuten.
- **24 statt 25 bewertbare SWE-Instanzen.** `django__django-11564` scheitert im
  lokalen Auswerter auch mit dem Gold-Patch.

### Erst reparieren, dann messen

Der erste Durchgang war keine Messung, sondern eine Fehlersuche. Alle drei Arme
lösten die Aufgaben, brauchten dafür aber ein Vielfaches des Nötigen, und der
Work-Modus verlor Aufgaben an seine eigene Mechanik. Die Ursachen standen in
den Traces, nicht in den Lösungsquoten. Vier Korrekturen im Agenten, eine im
Auswerter:

| # | Defekt | Korrektur | Commit |
|---|---|---|---|
| 1 | `--verify`: **Jede** Delegation setzte die Prüfpflicht neu, auch ein lesender `explorer`. Ein grüner Testlauf des `tester` löste sie nicht ein, und der Einwurf wiederholte sich nach einem grünen Lauf. Folge: fast zwei Abschlussversuche je Aufgabe. | Nur eine schreibende Delegation setzt die Pflicht; ein grüner Runner-Lauf des Sub-Agenten löst sie ein; nach Grün kein zweiter Einwurf. | `6e2787a` |
| 2 | `plan_execute`: Jeder Schritt behandelte seinen Auftrag wie die ganze Aufgabe — neuer Plan mit `update_plan`, Review je Schritt — und lief ins Schrittbudget von 8. | Schritt-Auftrag abgegrenzt, `step_max_steps` 8 → 20. | `051dfdb` |
| 3 | `plan_execute`: Der Planer zerlegte eine Ein-Funktions-Aufgabe in durchschnittlich 5,5 Schritte („ansehen", „testen", „reviewen" als eigene Schritte). | Plan-Prompt: so wenige Schritte wie nötig, eine kleine Aufgabe ist ein Schritt. Jetzt 1,2 (Polyglot) bzw. 1,3 (SWE). | `c87ffb6` |
| 4 | Work: Der Planer schrieb Prüfkommandos mit Shell-Syntax oder Programmen, die es im Container nicht gibt. `verify_command` läuft ohne Shell, das Item konnte also nie bestehen. Dazu erfand er Items, die das Vorhaben nicht verlangte. | `work_add_item` weist unausführbare Prüfkommandos beim Anlegen ab; Zerlegungs-Auftrag: nur was das Vorhaben verlangt. | `7b8506b` |
| 5 | Auswerter: `eval_local.py` rief bei Django Tests auf, deren ID der Django-Runner nicht ansprechen kann, und lastete den Fehlschlag dem Patch an — acht identische „regression" in jedem Arm. Gegenprobe: Die Gold-Patches bestanden nur 14 von 25. | Nicht ansprechbare Tests werden ausgelassen und im Ergebnis ausgewiesen (`tests_ausgelassen`). Gold-Patches danach 24 von 25. | `f26b92b` |

Wirkung, jeweils mit demselben Modell vorher und nachher gemessen:

| Arm | Kennzahl | vorher | nachher |
|---|---|---|---|
| `-s plan` | Tokens je Polyglot-Aufgabe | 361.000 (`v6`) | 185.000 |
| `-s plan` | Abschlussversuche je Aufgabe | 1,9 (`v6`) | 1,0 |
| `-s plan_execute` | Tokens je Polyglot-Aufgabe | 528.000 (`v8`) | 229.000 |
| `-s plan_execute` | Tokens je SWE-Instanz | 1.520.000 (`v8`) | 460.000 |
| Work-Modus | Polyglot gelöst | 57/64 (`v7`) | 63/64 |
| Work-Modus | Tokens je SWE-Instanz | 756.000 (`v7`) | 409.000 |

`v6` enthielt bereits den ersten Teil von Korrektur 1 (lesende Delegation); ein
vollständiger Lauf ganz ohne Korrektur liegt nicht vor. Der Abstand zum
Ausgangsstand ist also eher größer als hier ausgewiesen.

Die Lösungsquote von `plan` und `plan_execute` hat sich dabei kaum bewegt
(`plan` 81 → 82, `plan_execute` 80 → 82 → 80 über `v7`, `v8`, `v9`). Die
Korrekturen 1 bis 3 sind Effizienzgewinne, keine Qualitätsgewinne. Nur
Korrektur 4 hat Aufgaben zurückgeholt.

### Gegenprobe mit dem August-Modell (`v10`)

Derselbe Stand (`c87ffb6`), dieselben drei Arme gleichzeitig, je zwei parallele
Instanzen — aber mit `gpt-5.4-mini`, dem Modell der Runden 1 bis 4. Das trennt
den Anteil des Modells von dem des Agenten.

| Arm | Polyglot (64) | SWE Lite (25), neuer Auswerter | SWE Lite (25), August-Auswerter |
|---|---|---|---|
| Runde 4, `-s plan` (August) | 59/64 | nicht nachrechenbar | 8/25 |
| `v10`, `-s plan` | 57/64 | 12/25 | 6/25 |
| `v10`, `-s plan_execute` | 51/64 | 10/25 | 4/25 |
| `v10`, Work-Modus | 58/64 | 12/25 | 7/25 |

Die rechte Spalte bewertet die `v10`-Patches mit `eval_local.py` vor `f26b92b`;
nur sie ist mit dem August vergleichbar, weil dessen Rohdaten nicht mehr
vorliegen.

| Arm | Ø Schritte Hauptagent | Abschlussversuche | Tokens Polyglot | Tokens SWE | Ø s Polyglot / SWE |
|---|---|---|---|---|---|
| `-s plan` | 20 / 24 | 1,4 / 1,8 | 261.000 | 435.000 | 42 / 112 |
| `-s plan_execute` | — | — | 895.000 | 1.249.000 | 105 / 121 |
| Work-Modus | — | — | 215.000 | 537.000 | 42 / 134 |

**Was daraus folgt.**

- **Die Lösungsquote von `-s plan` hat sich mit dem kleinen Modell nicht
  verbessert.** 57 + 6 gegen 59 + 8 im August ist kein Rückschritt, sondern
  Streuung (Befund 4; `plan` lag im August selbst bei 57 und 59). Der Sprung
  auf 82/89 in Runde 5 kommt also vom Modell und vom Auswerter, nicht vom
  Agenten.
- **Der Work-Modus hat aufgeholt.** Im August verlor er gegen den
  Einzelagenten, jetzt liegt er mit `plan` gleichauf (70 gegen 69 von 89) und
  braucht auf Polyglot die wenigsten Tokens.
- **`plan_execute` ist mit dem kleinen Modell der schlechteste Arm**: sechs
  Polyglot-Aufgaben weniger, vier leere Patches, drei- bis vierfacher
  Token-Verbrauch. Mit `gpt-6.1-terra` war davon nichts zu sehen — die
  schlanke Planung aus Korrektur 3 greift bei `gpt-5.4-mini` offenbar nicht.
  Nicht untersucht.
- **Die Prüfschranke kostet das kleine Modell weiter Abschlussversuche** (1,4
  und 1,8 statt 1,0). Auch das ist nicht untersucht.
- **Schneller, bei gleicher Quote:** Polyglot 42 s je Aufgabe gegen 59 s in
  Runde 4. Tokens wurden im August nicht erfasst.

### Was sich gegenüber dem August-Stand ändert

- **Die SWE-Zahlen der Runden 1 bis 4 sind vermutlich zu niedrig.** Sie
  entstanden mit demselben Auswerter auf denselben Instanzen. Nachgerechnet
  sind sie nicht — die Rohdaten liegen auf einem anderen Laufwerk —, aber
  „regression" in den Tabellen oben enthält wahrscheinlich Fälle, die kein
  Patch hätte bestehen können. Das betrifft die absoluten Werte; ob es auch
  die Reihenfolge der Arme berührt, ist offen.
- **„Die Work-Runtime verliert" gilt so nicht mehr.** Mit Korrektur 4 liegt der
  Work-Modus drei Aufgaben hinter `plan` und braucht die wenigsten Tokens.
  Einen Vorteil zeigt er bei Aufgaben dieser Größe weiterhin nicht; offener
  Punkt 6 bleibt.
- **Befund 1 (Delegation kostet mehr, als sie einbringt) ist neu zu prüfen.**
  Defekt 1 bestand schon im August: Jede Delegation löste die Prüfpflicht aus.
  Wie viel der damals gemessenen Delegationskosten darauf zurückgeht, ist nicht
  gemessen.

### Nicht behoben

- **Stumme Modellaufrufe.** Im `plan_execute`-Arm von `v8` blieb der Endpunkt
  bei zwei Aufgaben minutenlang stumm (`python_phone-number` 1.315 s,
  `rust_react` rund 28 Minuten). Der Agent wiederholt nach 300 s Stille; die
  Ursache liegt beim Anbieter und ist unbekannt.
- **Hintergrund-Sub-Agenten** (`run_in_background`) setzen die Prüfpflicht
  weiter unabhängig von der Rolle. In den Traces kam der Fall nicht vor.
- **Der Auswerter-Fix hat keinen eigenen Test**, nur die Gegenprobe mit den
  Gold-Patches.

### Zwei Regeln aus dieser Runde

- **Erst in die Traces sehen, dann zählen.** Eine Lösungsquote von 64/64 verbarg
  einen Agenten, der die doppelte Arbeit machte. Schritte, Abschlussversuche
  und Tokens je Aufgabe gehören in jede Auswertung.
- **Den Auswerter mit den Gold-Patches gegenprüfen**, bevor ein SWE-Wert
  gedeutet wird. Identische Fehlschläge in allen Armen sind ein Zeichen für den
  Auswerter, nicht für den Agenten.

---

# Stand August 2026 — agentkit 0.21.0

Vier Messreihen an drei Tagen:

| Runde | Datum | Was gemessen wurde |
|---|---|---|
| 1 | 2026-08-08 | Bestandsaufnahme: solo/swarm × mit/ohne Graph, 81 Task-Agenten je Arm |
| 2 | 2026-08-09 | Wirkung von acht Korrekturen, dazu erstmals die Work-Runtime |
| 3 | 2026-08-09/10 | **Auflösung**: 89 Aufgaben je Arm, und zwei Schalter, die agentkit längst hat |
| 4 | 2026-08-10 | Kombination der beiden Schalter, gegen eine gleichzeitig gemessene Basislinie |

Die kurze Fassung: Die Korrekturen aus Runde 1 haben mechanische Defekte
beseitigt und den Agenten messbar hartnäckiger gemacht. Den größten Gewinn
brachte aber keine davon, sondern **ein vorhandener Schalter, der in acht
Läufen nie benutzt worden war** — `-s plan`. Der zweite Kandidat,
`--no-subagents`, sah einzeln gut aus und fiel in Runde 4 durch: Kombiniert
verliert er, und der Grund entwertet auch seine Einzelmessung.

## Ergebnisse Runde 3

Alle Arme: `react`/`plan` wie angegeben, Graph und Swarm aus, `max_steps=100`,
vier parallele Instanzen, dasselbe Binary.

| Arm | Polyglot (64) | SWE-bench Lite (25) | zusammen |
|---|---|---|---|
| `v3-basis` — ReAct + Sub-Agenten (Default) | 54/64 | 4/25 | 58/89 — 65,2 % |
| `v3-nosub` — `--no-subagents` | 56/64 | 6/25 | 62/89 — 69,7 % |
| **`v3-plan` — `-s plan`** | **57/64** | **8/25** | **65/89 — 73,0 %** |

Fehlerarten auf SWE-bench, die dieselbe Reihenfolge bestätigen:

| Arm | resolved | regression | empty_patch | unresolved |
|---|---|---|---|---|
| `v3-basis` | 4 | 6 | 3 | 12 |
| `v3-nosub` | 6 | 4 | 1 | 14 |
| **`v3-plan`** | **8** | **3** | 1 | 13 |

Und die Kosten (Werkzeugaufrufe über alle 89 Aufgaben):

| Arm | Ø Schritte | Werkzeugaufrufe | `task` | `swarm_*` | Ø Sekunden |
|---|---|---|---|---|---|
| `v3-basis` | 29,6 | 5258 | 311 | 0 | 72 |
| `v3-nosub` | 23,0 | **3196** (−39 %) | 0 | **491** | 71 |
| `v3-plan` | 25,5 | 4555 | 266 | 0 | 83 |

> Die `swarm_*`-Spalte ist in Runde 4 nachgetragen. Die ursprüngliche Auswertung
> zählte nur `task` und schrieb für `v3-nosub` „Delegationen 0" — das war falsch.
> Siehe Runde 4.

**Was das trägt und was nicht.** Bei 25 SWE-Instanzen ist 4 → 8 nicht
statistisch gesichert; identische Konfigurationen schwankten in dieser Woche
zwischen 4/10 und 1/10 auf denselben Instanzen. Belastbar ist die
*Übereinstimmung*: `-s plan` gewinnt in **beiden unabhängigen Benchmarks**,
senkt Regressionen (6→3) und leere Patches (3→1).

Zum SWE-bench-Leaderboard: Dessen 70–77 % sind **Verified** (500 Instanzen,
Spitzenmodelle). Hier laufen 25 Instanzen **Lite** mit einem kleinen Modell.
Die Zahlen sind nicht vergleichbar; vergleichbar sind nur die Arme
untereinander.

## Ergebnisse Runde 4 — die Kombination verliert

Offene Frage aus Runde 3: Addieren sich `-s plan` und `--no-subagents`? Beide
Arme liefen **gleichzeitig** (je zwei parallele Instanzen), mit **demselben,
frisch gebauten Binary**. Die mitlaufende Basislinie ist Absicht: Runde 3 hatte
gezeigt, dass die Lauf-zu-Lauf-Streuung größer ist als jeder gemessene Effekt,
also ist ein Vergleich gegen eine an einem anderen Tag gemessene Zahl wertlos.

| Arm | Polyglot (64) | SWE-bench Lite (25) | zusammen |
|---|---|---|---|
| **`v4-plan` — `-s plan`** | **59/64** | **8/25** | **67/89 — 75,3 %** |
| `v4-plan-nosub` — `-s plan --no-subagents` | 54/64 | 7/25 | 61/89 — 68,5 % |

Zwei Dinge stehen darin.

**Erstens: `-s plan` ist reproduzierbar.** 65/89 in Runde 3, 67/89 in Runde 4 —
mit anderem Binary, anderem Tag, anderer Nebenläufigkeit. Das ist der erste
Arm dieser Woche, der eine Wiederholung überlebt hat. Die SWE-Zahl ist sogar
identisch (8/25), die zwei Aufgaben Unterschied liegen in Polyglot.

**Zweitens: Die Kombination ist schlechter als `-s plan` allein**, und zwar um
sechs Aufgaben — mehr als der Abstand, den `-s plan` überhaupt zum alten
Default hat. Fünf davon fallen in Polyglot an (59 → 54).

### Warum — `--no-subagents` entfernt die Delegation gar nicht

Das Flag nimmt dem Agenten das `task`-Werkzeug. Das `swarm`-Werkzeug nimmt es
ihm nicht: Das registriert `agentkit_app` über den `extra_tools`-Seam
**bedingungslos**, unabhängig von `AGENTKIT_SWARM`. Damit bleibt ein zweiter,
teurerer Weg zur Delegation offen — und der Agent findet ihn:

| Arm | Benchmark | `task` | `swarm_*` | Instanzen mit Delegation |
|---|---|---|---|---|
| `v4-plan` | SWE-bench | 143 | 0 | 25 / 25 |
| `v4-plan-nosub` | SWE-bench | 0 | **309** | 24 / 25 |
| `v4-plan` | Polyglot | 167 | 0 | — |
| `v4-plan-nosub` | Polyglot | 0 | **0** | — |

Auf den langen SWE-Aufgaben baut sich der Agent zur Laufzeit einen Swarm
(`swarm` 33×, dann `swarm_propose`/`swarm_vote`/`swarm_reply` — die
Konsens-Maschinerie). Die Delegation verschwindet also nicht, sie wird nur
teurer und indirekter. Auf den kurzen Polyglot-Aufgaben greift er gar nicht
erst dazu; dort kostet der fehlende `task` schlicht fünf Aufgaben.

Dasselbe Bild in Runde 3 (491 `swarm_*`-Aufrufe bei `v3-nosub`) — meine
damalige Auswertung zählte nur `task` und meldete deshalb „Delegationen 0".
**Der Satz „`--no-subagents` braucht 39 % weniger Werkzeugaufrufe" stimmt als
Zahl, aber seine Deutung war falsch:** Der Rückgang kommt daher, dass der
Swarm-Weg pro Aufruf mehr leistet, nicht daher, dass weniger delegiert wird.

Die Delegationsmenge trennt in `v4-plan` weiterhin die Ergebnisse — leere
Patches gehen mit der meisten Delegation einher (Ø 10,3 Aufrufe gegen Ø 4,4 bei
gelösten). Im Swarm-Arm ist diese Trennschärfe weg (Ø 10,6 bis 13,7 über alle
Ausgänge): Wer immer über den Swarm geht, delegiert unabhängig davon, ob die
Aufgabe es hergibt.

### Nebenbefunde

- **Fehlerarten SWE-bench:** `v4-plan` 8 resolved / 5 regression / 3 empty,
  `v4-plan-nosub` 7 / 7 / 2. Der Swarm-Arm produziert mehr Regressionen.
- **Kosten:** `v4-plan-nosub` ist deutlich billiger (Polyglot 1323 statt 2890
  Werkzeugaufrufe, −54 %; Ø 42,5 s statt 59,3 s je Aufgabe) — er löst dafür
  weniger. Wer Durchsatz über Trefferquote stellt, hat hier einen Hebel.
- **Der no-change-Einwurf** (neu in diesem Binary) hat in 178 Task-Läufen
  **zweimal** ausgelöst, beide Male auf SWE-bench. Er schadet nicht, aber ein
  Effekt auf die Trefferquote ist bei dieser Häufigkeit nicht messbar.
- **Zwei abgebrochene Trials** in `v4-plan-nosub` (Verifier-Timeout nach 1800 s
  bei `python_forth`, API-Fehler Exit 2 bei `rust_gigasecond`) zählen als
  ungelöst. Selbst wenn man beide großzügig als gelöst wertet, bleibt der Arm
  mit 56/64 hinter `v4-plan` (59/64).

### Konsequenz

`STANDARD_AGENT_FLAGS` ist auf `-s plan` zurückgesetzt; `--no-subagents` ist
wieder heraus. Wer wirklich delegationsfrei messen will, braucht ein Binary
ohne `swarm`-Werkzeug — das Flag allein reicht nicht.

---

## Befund 1 — Delegation kostet mehr, als sie einbringt

Sub-Agenten sind teuer: Jede Delegation ist ein eigener Modell-Lauf mit
eigenem Kontext. Der Nutzen soll sein, dass der Kontext des Orchestrators
klein bleibt. Auf Polyglot, wo die Aufgabengröße überschaubar ist, kehrt sich
das um:

| Polyglot, 64 Aufgaben | `v3-basis` | `v3-nosub` |
|---|---|---|
| gelöst | 54/64 | **56/64** |
| Ø Schritte | 27,4 | **18,9** (−31 %) |
| Werkzeugaufrufe | 3042 | **1330** (−56 %) |
| Ø Sekunden | 59 | **33** (−44 %) |
| `read_file` | 592 | **176** (−70 %) |

Der `read_file`-Einbruch ist der Kern: Ohne Delegation liest der Agent
**weniger**, nicht mehr. Der Apparat erzeugt die Lesearbeit selbst, weil jeder
`explorer` sich sein Bild neu erlesen muss, ohne zu wissen, was der
Orchestrator schon gesehen hat.

Auf SWE-bench (große Repos) bleibt der Qualitätsvorteil (6 gegen 4), die
Wandzeit dreht sich aber um: 170 s gegen 105 s je Instanz — dort kauft
Delegation Zeit, bezahlt sie aber mit Ergebnis.

Dazu passt der Zusammenhang aus Runde 2, über vier Arme hinweg gemessen: Je
größer der Anteil der Schreibvorgänge, die der Orchestrator **selbst** macht,
desto weniger leere Patches und desto mehr gelöste Instanzen (97 % → 0
`empty_patch` und 4/10; 49 % → 2 und 2/10; 0 % → 1 und 3/10).

**Empfehlung:** `--no-subagents` als Default für Aufgaben, die ein Kontext
fasst. Sub-Agenten dort einsetzen, wo sie ihren Zweck haben — sehr große
Repositories und lange Recherchen —, nicht als Grundeinstellung.

## Befund 2 — `-s plan` war die ganze Zeit da

`PLAN_PREAMBLE` („Erstelle ZUERST einen kurzen, nummerierten Plan; arbeite ihn
dann Schritt für Schritt ab") ist implementiert, dokumentiert — und wurde in
keinem der acht vorherigen Benchmark-Läufe je benutzt. Alle liefen `react`.

Auf SWE-bench verdoppelt es die Basislinie (8/25 gegen 4/25) und halbiert die
Regressionen (3 gegen 6). Auf Polyglot liegt es mit 57/64 ebenfalls vorn.

Die plausible Erklärung, ausdrücklich als Vermutung: Die Regressionen entstehen,
weil der Agent den gemeldeten Fehler repariert und die Nachbarschaft aus dem
Blick verliert. Ein Plan, der vor der ersten Änderung steht, zwingt ihn, den
Umfang einmal zu überblicken. Bewiesen ist das nicht — gemessen ist nur, dass
mit Plan weniger kaputtgeht.

**Empfehlung:** `-s plan` in den Benchmark-Standard, und die Frage stellen, ob
es der bessere Default für den Coding-Agenten überhaupt ist.

## Befund 3 — Der Shell-Timeout: Defekt real, Wirkung null

Terminal-Bench, zweimal identisch konfiguriert, nur der Timeout unterschiedlich:

| | gelöst | Shell-Timeouts |
|---|---|---|
| 60 s (Default für Harbor) | 1/7 | **7** |
| 600 s | 1/7 | **0** |

Der Defekt ist echt — `BENCH_SHELL_TIMEOUT=60` wurde für Exercism-Tests
begründet („dort läuft alles in unter einer Sekunde"), und Terminal-Bench
enthält Compile-Aufgaben; in Runde 2 wurde `build-pov-ray` vierzehnmal
abgeschnitten. Die Behebung ist vollständig (7 → 0). Sie schlägt sich nur
**nicht** in gelösten Aufgaben nieder.

Ein sauberes negatives Ergebnis: Die Timeouts haben Zeit gekostet, waren aber
nicht die bindende Grenze. Der Wert sollte trotzdem benchmark-abhängig sein —
Zeit verbrennen ohne Gegenwert bleibt Verschwendung.

## Befund 4 — Die Messung selbst war der Engpass

Dieselben zehn SWE-Instanzen, dieselbe Konfiguration, zwei Läufe:

| | Runde 2 (`v2-solo`) | Runde 3 (`v3-basis`, erste 10) |
|---|---|---|
| resolved | 4/10 | 1/10 |

Das ist kein Widerspruch in den Daten, das **ist** die Datenlage. Rückwirkend
entwertet es jede Aussage der Form „Arm X ist um eine Aufgabe besser als Y" aus
den Runden 1 und 2 — auch meine eigenen. Polyglot dagegen lieferte über drei
Tage, drei Binaries und zwei Parallelitätsstufen bei gleicher Konfiguration
zweimal exakt 54/64.

Dazu kam ein Stichprobenfehler: Die ersten zehn Instanzen sind astropy und
frühe Django-Tickets. Über 25 Instanzen fällt die Quote von 40 % auf 16 % —
die bisherigen Läufe standen auf dem leichtesten Zehntel des Datensatzes.

**Empfehlung:** Polyglot (64) ist das Arbeitsinstrument, SWE-bench Lite die
Kontrolle. Wer Varianten vergleicht, braucht dort mindestens 25 Instanzen und
sollte Unterschiede unter ~10 Punkten nicht interpretieren.

## Was die Korrekturen aus Runde 1 gebracht haben

Zusammengefasst aus Runde 2 (dieselben 17 Aufgaben je Arm, vor/nach):

- **Frühes Aufgeben beseitigt.** `--verify` war durch **jedes** erfolgreiche
  `run_shell` einlösbar — ein `ls` genügte. Jetzt zählt nur ein erkannter
  Testrunner, und eine Delegation setzt die Prüfpflicht ebenfalls. Wirkung:
  20,7 → 37,6 Schritte je Aufgabe, 95 → 195 Sekunden. `build-pmars` wurde
  gelöst statt nach 17 Schritten mit „make is unavailable" aufgegeben.
- **Testdateien im Diff: 7 → 0**, `patch_failed` 2 → 0. Der Gewinn, den Runde 1
  nur nachträglich per Filter zeigen konnte (3/10 → 4/10), fiel danach im Lauf
  selbst an.
- **Graph und Swarm wurden erst funktionsfähig** — und blieben ohne Vorteil.
  Der Graph verteilte sich vorher auf 13 zufällige Wissensinseln, weil der
  Arbeits-Scope ohne `--session` auf `pid-<id>` zurückfiel und Container-PIDs
  kollidieren. Der Team-Prompt des Swarms wurde nie übergeben. Beides behoben,
  beides ohne Wirkung auf die Ergebnisse. Sie gehören nicht in den Standard.
- **Die Work-Runtime verliert** bei Aufgaben dieser Größe (TB 1/7 gegen 2/7,
  SWE 3/10 gegen 4/10). Jeder Item-Versuch bekommt einen frischen Agenten mit
  leerem Kontext; die Erkundung beginnt in jedem Item von vorn, und
  `max_steps` gilt je Versuch — sechs von zehn Instanzen liefen ins Limit,
  der Einzelagent bei keiner. Das widerlegt ihren Zweck nicht; es zeigt, dass
  eine SWE-bench-Lite-Instanz zu kurz für sie ist.

## Fehler in diesem Bericht, die vorherige Fassungen enthielten

- „Der Tech-Lead ignoriert seinen Team-Prompt" — er hat ihn nie bekommen.
- „`graph_promote`-Compliance ist 0 %" — die Aufforderung stand in einer Datei,
  die nie übergeben wurde.
- „Der Graph ist schreibgeschützt-nutzlos" — Tasks lasen einander sehr wohl,
  nur über eine PID-Kollision statt über den vorgesehenen Weg.
- „Rot-dann-grün ist der große Hebel" — über 40 Instanzen gemessen: die Gruppe
  ohne jeden Testlauf löste am häufigsten (38 % gegen 24 %). Bei n=3/29/8 trägt
  das nichts, aber es trägt die These eben auch nicht.
- „`--no-subagents` schaltet die Delegation ab, Delegationen 0" — es entfernt
  nur `task`. Der Agent wich in Runde 3 wie in Runde 4 auf das `swarm`-Werkzeug
  aus (491 bzw. 309 Aufrufe). Meine Auswertung zählte nur `task` und sah es
  deshalb nicht. Daraufhin stand das Flag einen Tag lang zu Unrecht im Default.

Gemeinsame Ursache der ersten drei: tote Verkabelung, die sich wie eine Zusage
las. Sie ist entfernt. Ursache des vierten: eine Auswertung, die nur nach dem
erwarteten Namen suchte. Wer Delegation misst, muss **jeden** Weg dorthin
zählen — `task` und `swarm_*` — sonst misst er die Umgehung als Abwesenheit.

## Offene Punkte

| # | Vorschlag | Grundlage | Stand |
|---|---|---|---|
| 1 | `-s plan` als Benchmark-Standard | gewinnt in beiden Benchmarks, in zwei Runden reproduziert | erledigt |
| 2 | Prüfen, ob `plan` der bessere Default des **Coding-Agenten** ist | halbierte Regressionen, 67/89 gegen 58/89 | offen — Produktentscheidung |
| 3 | Schlussprüfung „Antwort behauptet Änderung, `git diff` leer" | 5 `empty_patch` in Runde 3 | umgesetzt, Wirkung nicht messbar (2 Auslöser in 178 Läufen) |
| 4 | `BENCH_SHELL_TIMEOUT` benchmark-abhängig | 7 → 0 Timeouts, kein Ergebnisgewinn | erledigt |
| 5 | Graph, Swarm und Work aus dem Standard | drei Runden ohne Vorteil | erledigt |
| 6 | Work-Runtime an einem mehrstündigen Vorhaben messen | dort liegt ihr Zweck | offen — braucht erst eine passende Aufgabe |
| 7 | `--no-subagents` soll auch das `swarm`-Werkzeug entfernen | sonst misst das Flag nicht, was sein Name sagt | offen |
| 8 | Heutigen Stand mit `gpt-5.4-mini` messen | trennt in Runde 5 den Anteil des Modells von dem des Agenten | erledigt (`v10`) |
| 11 | `plan_execute` mit `gpt-5.4-mini` untersuchen | 51/64, vier leere Patches, 895.000 Tokens je Polyglot-Aufgabe | offen |
| 12 | Work-Planer: Vorgaben des Vorhabens wörtlich übernehmen, Umfang nicht verengen | zwei der drei Work-Verluste in `v8` gehen auf eine verengte Item-Beschreibung zurück | versucht (`v11`), kein messbarer Gewinn: `gpt-6.1-terra` 79 → 79 von 89, `gpt-5.4-mini` 70 → 67. `astropy-14182` wird gelöst, `django-11815` nicht. Nicht auf `main`; der Versuch liegt im Branch `versuch/work-planer-wortlaut` |
| 9 | Prüfpflicht bei Hintergrund-Sub-Agenten rollenabhängig machen | Rest von Defekt 1 aus Runde 5 | offen — in den Traces nicht aufgetreten |
| 10 | Test für die Auswerter-Korrektur | bisher nur Gegenprobe mit Gold-Patches | offen |

## Reproduktion

```bash
cd agent_benchmarks
make setup build-agent

# Der beste bekannte Arm:
BENCH_AGENT_FLAGS="-s plan" BENCH_GRAPH=0 BENCH_WORK=0 AGENTKIT_SWARM=0 \
  uv run harbor run -a agentkit_bench.harbor_agent:AgentkitAgent --n-concurrent 4 \
  -d aider-polyglot@1.0 -i "polyglot_python_*" -i "polyglot_rust_*" \
  -o "$BENCH_RESULTS_DIR/polyglot" --job-name v3-plan
uv run python -m agentkit_bench.swebench.run_swebench --limit 25 --workers 4 --run-id v3-plan
uv run python -m agentkit_bench.swebench.eval_local \
  --predictions "$BENCH_RESULTS_DIR/swebench/v3-plan/preds.jsonl" --run-id v3-plan

# zuschauen (Binary mit --features viz):
agentkit viz --trace "$BENCH_RESULTS_DIR" --open
```

`BENCH_AGENT_FLAGS` reicht beliebige agentkit-Flags an die Task-Agenten durch —
der Weg, auf dem `-s plan` und `--no-subagents` überhaupt messbar wurden. Es ist
inzwischen selbst der Default (`config.py::STANDARD_AGENT_FLAGS`), oben steht es
nur der Deutlichkeit halber.

**Vergleichsarme immer gleichzeitig fahren.** Die Streuung zwischen zwei Läufen
derselben Konfiguration ist größer als jeder gemessene Effekt (Runde 3: 4/10
gegen 1/10 auf denselben Instanzen). Runde 4 lief deshalb mit je zwei statt vier
parallelen Instanzen, beide Arme zur selben Zeit, mit demselben Binary.

**Harbor nicht auf `tail` pipen.** Der Plan-Arm brach zweimal still nach 12
bzw. 22 von 64 Trials ab (0 Fehler, `finished_at: null`, keine Meldung);
ohne die Pipe lief derselbe Lauf vollständig durch. Die Skripte schreiben
Harbors Ausgabe deshalb direkt in eine Datei.

## Frühere Läufe (dasselbe Modell)

| Lauf | Terminal-Bench (7) | Polyglot (64) | SWE-bench Lite |
|---|---|---|---|
| Erstlauf 2026-07-21 | 1/7 | 26/64 (40,6 %) | — |
| Smoke 2026-07-22 | 2/7 | 58/64 (90,6 %) | — |
| Volllauf 2026-08-01 | — | 43/64 (67,2 %) | — |
| Runde 1, bester Arm | 2/7 | 55/64 (85,9 %) | 3/10 |
| Runde 2, bester Arm | 2/7 | — | 4/10 |
| Runde 3, bester Arm (`-s plan`) | 1/7 | 57/64 (89,1 %) | 8/25 |
| **Runde 4, bester Arm (`-s plan`)** | — | **59/64 (92,2 %)** | **8/25** |
| Runde 5, bester Arm (`-s plan`) — **`gpt-6.1-terra`, korrigierter Auswerter** | — | 64/64 (100 %) | 18/25 |

Terminal-Bench mit 7 Aufgaben trägt keine Unterschiede und sollte für
Vergleiche nicht mehr herangezogen werden.
