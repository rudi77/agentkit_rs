# Phase 3 des Viz-Plans: die Testkampagne nach §30

Bericht zur Handprobe aus `docs/plans/agentkit-viz-plan.md`, Phase 3. Gefahren
mit echtem Modell (`azure:gpt-5.4-mini`) in einem Wegwerf-Klon des Repos, nicht
im Arbeitsverzeichnis.

Der Auftrag wörtlich aus §30 von `docs/plans/agent-work-runtime.md`:

> Analysiere `agentkit_swarm`, identifiziere die drei wichtigsten
> Lifecycle-Risiken, implementiere die wichtigste Verbesserung, ergänze Tests,
> führe ein unabhängiges Review durch, dokumentiere die Entscheidung.

## Aufbau

Sechs Work Items (`discovery` → `analysis` → `implementation` → `test` →
`review` → `documentation`), zwei Rollen (`explorer`, `reviewer`), zwei
Schwarm-Items (Vorlagen `discovery` und `review`), Git-Isolation, eine
`automated_tests`-Policy, Budget 5400 s / 12 Items / 3 Versuche / 60 Schritte.
Gestartet mit `--trace` und `--graph`, beobachtet mit `agentkit viz`.

## Ergebnis des Laufs

```
Lauf beendet: all_items_done
Versuche: 6        Abgeschlossen: W-1 … W-6      (+ W-7 Integration)
Laufzeit: ~7 min   Artefakte: 10                 Trace: 585 Ereignisse
Graph: 28 Entities, 20 Claims                    Git: 2 Item-Commits, 5 gemergt
```

167 Tool-Aufrufe, davon 54 `read_file`, 18 `grep`, 8 `task`, 8 `run_shell`,
6 `swarm_propose`, **0 `swarm_vote`**.

## Die Erfolgskriterien aus §30

| Kriterium | Stand | Beleg |
|---|---|---|
| kein Item verloren | ✅ | 6/6 `completed`, dazu das automatische Integrations-Item |
| kein abgeschlossenes Item wiederholt | ✅ | 6 Versuche auf 6 Items |
| abgelaufene Leases freigegeben | ✅ | „Wiederaufnahme: 1 Item(s) freigegeben" nach dem Abschuss |
| Lauf korrekt fortgesetzt | ✅ | zusätzlich: „Repository stand auf Item-Branch … zurück auf 'main' gewechselt" |
| Claims mit vollständiger Provenance | ✅ | jede Kante trägt `work_item worker-1:explorer-a work:swarm-lifecycle-2/R-1` |
| nur verifizierte Claims promotet | ✅ | 0 promotet — nur ein Item hatte eine Policy, und das hatte keine Claims (siehe Befund 5) |
| Endergebnis mit Artefakten, Tests und Review | ✅ | 10 Artefakte, ein committeter Test, ein Review mit Urteil, `docs/entscheidungen/swarm-lifecycle.md` |

Bedingungen: ≥5 Items ✅ · zwei Rollen ✅ · ein Schwarm ✅ (zwei) · Claims im
Working Graph ✅ · Promotion ⚠️ (siehe Befund 5) · Git-Artefakt ✅ · Review ✅ ·
Prozess mitten in der Arbeit abgeschossen ✅ · nach Neustart fortgesetzt ✅ ·
abgeschlossene Analyse nicht wiederholt ✅ · Evidence Trail ✅.

## Der Betrachter selbst

Beide Zusicherungen des Plans geprüft:

- **Live und nachträglich sind derselbe Codepfad.** Nach dem Lauf eine zweite
  Instanz auf dieselbe Datei gestartet und `/api/runs`, `/api/agents` und
  `/api/swarm` verglichen — Byte für Byte identisch (585 Ereignisse).
- **Gegen die CLI gegengelesen.** `/api/work/<projekt>` gegen
  `agentkit work status --format json`: 7 Items `completed`, 10 Artefakte,
  7 Versuche — in beiden gleich.

## Befunde

Fünf, alle als Issue im GitHub-Projekt erfasst. Drei davon hätte kein Test
gefunden — sie fielen auf, weil ein echter Lauf im Betrachter mitgelesen wurde.

1. **[#32] Schwarm-Work-Item verwarf ein bereits abgeliefertes Ergebnis.**
   Ein Mitglied liefert mit `work_submit` ab und beendet seinen Zug, ohne
   `swarm_propose`; der Schwarm läuft in den Leerlauf, der Executor meldete
   „(keine Antwort)", und `runner::run_attempt` prüft diesen Sentinel VOR der
   Submission. Der Versuch galt als gescheitert, das Item lief in seine
   Versuchsgrenze und blockierte alle abhängigen Items. **Behoben** und im
   nächsten echten Lauf bestätigt: derselbe Schwarm endete wieder mit `idle`
   und zwei Vorschlägen ohne Stimmen — und wurde korrekt als erledigt verbucht.
2. **[#33] Work-Schwarm erbte `max_idle = 300 s`** statt einer eigenen Grenze.
   Fünf Minuten Stillstand je Versuch. **Behoben** (60 s).
3. **[#34] „runtime" erschien als Agent** im Ereignisstrom — die Initialaufgabe
   kommt von der Laufzeit, nicht von einem Agenten. **Behoben.**
4. **[#35] Die Schwarm-Vorlagen erreichen nie Konsens.** Beide Mitglieder
   schlagen vor, keiner stimmt ab: 6 `swarm_propose`, 0 `swarm_vote` im ganzen
   Lauf, alle sechs Vorschläge „offen". Bei zwei Mitgliedern und Quorum 1 ist
   der Konsensweg damit per Konstruktion tot; dass die Items trotzdem fertig
   werden, liegt allein an Befund 1. **Offen** — die Entscheidung gehört zum
   Schwarm-Entwurf, nicht in einen Schnellschuss.
5. **[#36] Akzeptanzkriterien prüfen nichts.** Ein Item meldete, es habe
   Aussagen „promotet" — über ein Tool, das es gar nicht gibt und das im Trace
   nie aufgerufen wurde. Das Item galt als erledigt. Die Laufzeit hat sich
   dabei korrekt verhalten (Promotion hängt an der Verifikations-Policy), aber
   wer Zusammenfassungen liest statt Artefakte, hält den Lauf für erfolgreicher,
   als er war. **Offen** (Doku-Vorschlag im Issue).

## Was der Lauf über das Werkzeug sagt

Die Erwartung des Plans — „dass dabei etwas gefunden wird, ist die Erwartung,
nicht der Ausnahmefall" — hat sich bestätigt. Entscheidend war dabei nicht die
Existenz des Trace, sondern zwei Ansichten:

- Die **Agenten-Liste** zeigte sofort, dass in einem Schwarm ein Mitglied 87
  und das andere 44 Ereignisse hatte — und im ersten Lauf eines gar keine.
- Die **Abstimmungsansicht** zeigte sechs Vorschläge mit leerer
  Zustimmungs-Spalte. Diese Zahl steht in keinem Log und in keinem
  `SwarmResult`-Text; sie ist genau der Befund, für den Phase 4 gebaut wurde.

## Eigene Fehler in der Durchführung

Der Vollständigkeit halber, weil sie Laufzeit gekostet haben und nicht dem
Produkt anzulasten sind:

- Ein `cargo test --no-default-features` zwischen zwei Läufen baute das Binary
  ohne `openai` neu — der zweite Lauf lief unbemerkt im Demo-Modus, bis der
  Trace es zeigte („Demo-Modus (kein Netz)"). Vor einem Lauf gehört ein
  ausdrücklicher Build mit den gewünschten Features und eine Modellprüfung.
- Die Item-Beschreibung nannte ein Tool (`work_promote`), das es nicht gibt.
