---
name: postgres-logs
description: PostgreSQL-Serverlogs lesen — Schweregrade, typisches Rauschen (Checkpoints, autovacuum), echte Alarme (Deadlocks, Verbindungslimit, langsame Queries, Korruption).
---

# PostgreSQL-Serverlogs

## Aufbau

```
2026-07-11 04:12:40.123 UTC [12345] LOG:  Nachricht
                                     ^^^  ^^^-- Schweregrad
                                     Prozess-ID (verbindet zusammengehörige Zeilen!)
```

Die **Prozess-ID in eckigen Klammern** ist der Schlüssel: Zeilen mit derselben PID gehören zu
derselben Sitzung. `ERROR` + `STATEMENT` mit gleicher PID sind **ein** Befund, nicht zwei.

## Schweregrade

`DEBUG` < `INFO` < `NOTICE` < `WARNING` < `ERROR` < `FATAL` < `PANIC`

- **`ERROR`** — eine Anweisung ist gescheitert. Die Sitzung lebt weiter.
- **`FATAL`** — die *Verbindung* wurde beendet. Häufig Anmeldung/Limit.
- **`PANIC`** — der *Server* geht runter. Immer kritisch.

## Was Rauschen ist (nicht melden)

- **`checkpoint starting/complete`** — Routine. Interessant erst, wenn `checkpoints are occurring
  too frequently` dabeisteht (dann: `max_wal_size` zu klein).
- **`automatic vacuum` / `automatic analyze`** — der Autovacuum macht seine Arbeit. Gut.
- **`ERROR: duplicate key value violates unique constraint`** — meist die Anwendung, die einen
  Konflikt korrekt abfängt (Upsert). Nur melden, wenn es plötzlich **massenhaft** auftritt.
- **`LOG: connection received` / `disconnection`** — normaler Verkehr.

## Was ein echter Alarm ist

| Muster | Warum |
|---|---|
| **`FATAL: sorry, too many clients already`** | Das Verbindungslimit ist erreicht — die Anwendung bekommt **keine** Verbindung mehr. Sieht für sie aus wie „Datenbank weg". Sehr oft die wahre Ursache scheinbar unerklärlicher App-Fehler. |
| **`deadlock detected`** | Zwei Transaktionen blockieren sich. Nenne die beteiligten Anweisungen (`STATEMENT:` mit denselben PIDs). |
| **`PANIC`** / `database system is shut down` | Der Server ist unten. |
| **`FATAL: password authentication failed for user "…"`** in Serie | Anmeldeversuche. Menge + Quelle nennen — das kann ein Angriff sein oder ein falsch konfigurierter Client. Beides muss jemand wissen. |
| **`duration: NNNN.NNN ms`** mit großen Werten | Langsame Abfragen. Nenne Dauer und Anweisung. Ein Sprung von 20 ms auf 8000 ms ist ein Befund, auch wenn nichts „fehlschlägt". |
| **`could not write to file` / `invalid page in block`** | **Speicher-/Korruptionsproblem.** Immer kritisch, immer sofort. |
| **`canceling statement due to statement timeout`** gehäuft | Die Datenbank kommt nicht hinterher. |

## Vorgehen

1. Zeilen mit gleicher **PID** zusammenziehen (`ERROR` + `STATEMENT` + `DETAIL` = ein Befund).
2. Gleichartige Fehler gruppieren: **Anzahl + Zeitfenster** statt hundert Einzelzeilen.
3. Beim Verbindungslimit (`too many clients`) ausdrücklich darauf hinweisen, dass sich das
   **in der Anwendung als Ausfall zeigt** — dort sucht sonst jemand an der falschen Stelle.
