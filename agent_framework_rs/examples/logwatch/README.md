# logwatch — agentkit als lernender Log-Filter

Ein `grep` findet Auffälligkeiten. Es findet sie **jeden Tag wieder** — dieselben. Deshalb
schaut nach zwei Wochen niemand mehr hin.

Ein Wachhund, der jede Nacht dieselbe Katze anbellt, wird ignoriert. Und ein ignorierter
Wachhund ist nutzlos.

`logwatch` ist ein agentkit-Filter, der **nur meldet, was neu ist** — weil er sich merkt, was er
schon gesagt hat. Zwei agentkit-Fähigkeiten, die ein klassischer Unix-Filter nicht hat:

| | |
|---|---|
| **`--skills ./skills`** | Fachwissen je Logtyp (IIS, PostgreSQL, Event-Log), **on demand geladen**. Im Kontext liegt permanent nur der schlanke Index; die ausführliche Anleitung holt der Agent erst mit `read_skill()`, wenn er sie braucht — *progressive disclosure*. |
| **`--memory state/known.jsonl`** | Ein **Langzeitgedächtnis** (JSONL), das den Prozess überlebt. Über `recall` fragt der Agent: kenne ich das schon? |

Ein klassischer Filter ist zustandslos — er kann gar nicht wissen, was er gestern gesagt hat.
Genau das ist hier der Unterschied.

## Der Beweis

```powershell
.\Invoke-LogWatch.ps1 -Demo
```

Vier Läufe, und der zweite ist der entscheidende:

```
Lauf 1 (Tag 1)             2 neu   — er sieht die Probleme zum ersten Mal.
Lauf 2 (Tag 1 nochmal)     0 neu   — STILL. Er weiß, dass er es schon gesagt hat.
Lauf 3 (Tag 2)             2 neu   — nur das wirklich Neue (Bekanntes blieb still).
Lauf 4 (PostgreSQL)        3 neu   — anderer Skill, anderes Fachwissen.
```

**Lauf 2 bekommt exakt dieselben Zeilen wie Lauf 1** — und meldet nichts. Nicht, weil ein
Vergleich der Eingabedaten das verhindert (die Zeilen sind ja identisch neu für ihn), sondern
weil er im Gedächtnis nachsieht und feststellt: *das habe ich bereits berichtet.*

**Lauf 3** ist die Nagelprobe fürs Urteilsvermögen. Tag 2 enthält:

- dieselbe kaputte Route `500.19 /api/v2/orders` — nur häufiger, zu anderen Zeiten, von anderen
  Client-IPs → **bleibt still.** Es ist dieselbe kaputte Sache.
- denselben Traversal-Versuch auf `/api/v2/files` — aber er bekommt jetzt **`200` statt `404`**
  → **wird gemeldet**, mit hohem Schweregrad. Nicht das Muster ist neu, sondern **der Ausgang**.
  Aus „abgewehrt" wurde „erfolgreich".
- eine **neue** 503-Serie auf `/api/v2/checkout` → **wird gemeldet.**

Das ist der Unterschied zwischen „Muster gesehen" und „verstanden, was sich geändert hat".

## Der Aufbau: drei Stufen, jede mit genau einer Aufgabe

```
neue Zeilen seit letztem Lauf   ← PowerShell rechnet den Offset aus (wie `tail -f`).
        │                          Deterministisch. Kein Modell.
        ▼
┌──────────────────────────────────────────────────┐
│ Stufe 1 — ANALYSE     --skills, KEIN Gedächtnis  │   „Was steht in diesen Zeilen?"
│ Lädt den passenden Skill, findet Auffälligkeiten,│   Weiß nichts von früher — und soll
│ vergibt je eine STABILE Signatur.                │   auch nichts davon wissen.
└───────────────────────┬──────────────────────────┘
                        │  befunde[] (JSON)
                        ▼
┌──────────────────────────────────────────────────┐
│ Stufe 2 — ABGLEICH    --memory, sieht kein Log   │   „Neu — oder schon gemeldet?"
│ Fragt je Signatur per `recall` das Gedächtnis.   │   Kennt die Logzeilen nicht und
│ Erkennt Verschlechterungen von Bekanntem.        │   braucht sie nicht.
└───────────────────────┬──────────────────────────┘
                        │  neu[] / bereits_bekannt[]
                        ▼
   Stufe 3 — MERKEN     PowerShell schreibt die NEUEN Befunde ins Gedächtnis (JSONL).
                        Deterministisch. Kein Modell.
```

### Warum drei Stufen? Weil zwei Aufgaben in einem Agenten schiefgingen

Der erste Entwurf war **ein** Agent, der beides tat: analysieren *und* Buch führen (`recall`
vor dem Melden, `remember` danach). Er scheiterte reproduzierbar — und zwar auf lehrreiche Weise:

> **`recall` sieht innerhalb eines Laufs sofort, was `remember` gerade geschrieben hat.**

agentkits `LongTermMemory` hält die Einträge in einem geteilten Speicher; ein `remember` ist für
das nächste `recall` **desselben Laufs** sichtbar. Der Agent merkte sich pro Befund erst den
Eintrag, fragte dann ab, fand seinen eigenen frischen Eintrag — und schloss: *„kenne ich schon"*.
Ergebnis: Im allerersten Lauf, mit **leerem** Gedächtnis, meldete er alles als „bereits bekannt".

Die Lehre ist nicht „das Modell ist dumm", sondern **ein Werkzeug, eine Aufgabe**. Analysieren
und Buch führen sind zwei Jobs. Getrennt macht jeder Agent seinen Job zuverlässig, und das
Schreiben ist ohnehin deterministisch — es gehört gar nicht in ein Modell.

Deshalb gilt hier: **Der Agent urteilt (`recall`), die Pipeline führt Buch.** Wer das Beispiel
abwandelt, sollte diese Trennung nicht aufheben.

### Die Signatur — der Dreh- und Angelpunkt

Jeder Befund bekommt eine **stabile Signatur**: `HTTP 500.19 /api/v2/orders`. Stabil heißt:
**ohne** Zeitstempel, **ohne** Anzahl, **ohne** Client-IP. Dieselbe kaputte Sache muss morgen
dieselbe Signatur bekommen — sonst funktioniert das Wiedererkennen nicht.

Der **Ausgang** (`abgewehrt (404)` / `erfolgreich (200)`) steht bewusst **nicht** in der Signatur,
sondern in einem eigenen Feld. Denn er kann sich ändern — und genau diese Änderung ist die
Nachricht.

Die Signaturwörter landen als **Tags** im Gedächtnis. agentkits `recall` bewertet über
Stichwort-Überlappung von Text *und* Tags (`src/memory.rs`) — die Signatur ist damit der
Schlüssel, unter dem der Befund wiedergefunden wird.

## Ausführen

Voraussetzung: agentkit gebaut (`cargo build --release` in `agent_framework_rs`) und
LLM-Credentials in einer `.env` (`AZURE_OPENAI_*` oder `OPENAI_API_KEY`).

```powershell
.\Invoke-LogWatch.ps1 -Demo         # die volle Beweiskette (setzt das Gedächtnis zurück)

# Echte Logs — beim zweiten Aufruf werden nur die HINZUGEKOMMENEN Zeilen betrachtet:
.\Invoke-LogWatch.ps1 -Path C:\inetpub\logs\LogFiles\W3SVC1\u_ex260711.log
.\Invoke-LogWatch.ps1 -Path C:\pg\log\postgresql-2026-07-11.log

.\Invoke-LogWatch.ps1 -Replay       # Datei von vorn lesen, Gedächtnis BEHALTEN
.\Invoke-LogWatch.ps1 -Fresh        # Gedächtnis UND Offsets verwerfen
```

Als **geplanter Task** (alle 15 Minuten): `Exit 0` = nichts Neues, `Exit 1` = neue Befunde. Der
Task meldet sich also nur, wenn es wirklich etwas zu sagen gibt.

Neue Zeilen werden über einen **Zeilen-Offset** je Datei bestimmt (`state/offsets.json`) — wie
`tail -f`. Schrumpft eine Datei (Logrotation), wird sie von vorn gelesen.

## Einen neuen Logtyp beibringen

Ein Ordner, eine Datei, **kein Code**:

```
skills/nginx-logs/SKILL.md
---
name: nginx-logs
description: nginx-Zugriffslogs lesen — Feldaufbau, typisches Rauschen, echte Alarme.
---
# nginx-Zugriffslogs
## Was Rauschen ist …
## Was ein echter Alarm ist …
```

Dazu in `Get-LogType` (in `modules/logwatch-helpers.ps1`) eine Zeile, die den Typ erkennt.
Fertig — der Agent findet den Skill über `list_skills` selbst.

Das ist der Punkt von *progressive disclosure*: Drei Skills liegen im Ordner, aber nur der
**eine** passende landet im Kontext. Bei dreißig Skills wäre es genauso.

## Ehrlichkeit über die Grenzen

- **Die Anzahlen des Modells sind Schätzungen.** Es sagt „50×", wo die Datei 47 Zeilen hat. Für
  ein Alarmsignal reicht das; wer **exakte** Zahlen braucht, zählt sie deterministisch in
  PowerShell und reicht sie dem Agenten mit — dasselbe Prinzip wie sonst überall hier: Fakten
  ausrechnen, Urteil dem Modell.
- **`recall` arbeitet mit Stichwort-Überlappung**, nicht mit Embeddings (bewusst — siehe
  `src/memory.rs`). Es findet, was Wörter teilt. Deshalb sind stabile, wortreiche Signaturen
  wichtig; ein kryptisches `E500-A17` würde nicht wiedergefunden.
- **Der Filter kann übervorsichtig werden.** Ein als Rauschen gemerkter Eintrag wird nie wieder
  gemeldet. Das Gedächtnis ist eine schlichte JSONL-Datei — man kann und soll hineinsehen und
  Zeilen löschen, wenn er etwas zu Unrecht stummgeschaltet hat.

## Tests

```powershell
pwsh -File .\tests\Test-LogWatch.ps1     # 17 Prüfungen, offline, ohne LLM
```

Geprüft wird, was ohne Modell prüfbar ist: die Offset-Verwaltung (inkl. Logrotation), die
Logtyp-Erkennung und das **Gedächtnis-Format** — schreibt die Pipeline JSONL, das agentkits
`LongTermMemory` auch wirklich lesen kann (`text` + `tags` je Zeile)?

> Diese Tests haben einen echten Bug gefunden: `$x = if (…) { @('eins') }` **entpackt** in
> PowerShell das einelementige Array zu einem String — `.neu[0]` lieferte dann das erste
> *Zeichen* statt der ersten *Zeile*. Ausgerechnet im häufigsten Fall des Dauerbetriebs: genau
> eine neue Logzeile. Deshalb der Kommentar an `Get-NewLines`.

## Dateien

```
logwatch/
  Invoke-LogWatch.ps1           # Orchestrator (Offset → Analyse → Abgleich → Merken)
  modules/logwatch-helpers.ps1  # Offsets, Logtyp-Erkennung, Gedächtnis-Schreiben (JSONL)
  prompts/
    10_analyze.md               # Stufe 1: --skills, KEIN Gedächtnis
    20_dedup.md                 # Stufe 2: --memory (nur recall), sieht das Log nicht
  skills/
    iis-logs/SKILL.md           # je Logtyp: was ist Rauschen, was ist Alarm
    postgres-logs/SKILL.md
    windows-eventlog/SKILL.md
  fixtures/                     # Tag 1, Tag 2 (mit Verschlechterung), PostgreSQL
  tools/Build-Fixtures.ps1      # erzeugt die Beispiel-Logs
  tests/Test-LogWatch.ps1       # offline, kein LLM
  state/                        # known.jsonl (Gedächtnis) + offsets.json (generiert)
```

## Bezug zum Rest

- [`win_triage`](../win_triage/README.md) zeigt Fan-out über parallele Agenten und `--dry-run`
  als Sicherheitsnetz — auf Windows-Event-Logs.
- Das [Accounts-Payable-Demo](../accounts_payable/README.md) zeigt dieselbe Komposition über
  Dokumente; die [interaktive Variante](../accounts_payable_interactive/README.md) lässt einen
  Orchestrator-Agenten dazulernen — dort über einen **Wissensgraphen in Dateien**, hier über
  agentkits **eingebautes Langzeitgedächtnis**.
- `--skills`, `--memory` und die Exit-Codes sind allgemeine agentkit-Fähigkeiten — siehe
  [Benutzerhandbuch](../../docs/USER_MANUAL.md).
