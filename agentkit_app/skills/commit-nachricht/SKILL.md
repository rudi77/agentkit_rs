---
name: commit-nachricht
description: Eine Commit-Nachricht zu den aktuellen Änderungen schreiben, im Stil des Repos. Für Aufträge wie "schreib eine Commit-Nachricht", "was soll ich committen", "fasse die Änderungen für den Commit zusammen".
---

# Commit-Nachricht schreiben

Du schreibst die Nachricht, du committest nicht — es sei denn, der Auftrag verlangt es
ausdrücklich.

## Schritt 1 — was sich geändert hat

1. `git_status` — welche Dateien geändert, neu, gelöscht sind.
2. `git_diff` — der Inhalt. Bei vielen Dateien zuerst die Liste, dann gezielt die Dateien,
   die das Verhalten ändern; reine Formatierung erkennst du am Diff.
3. Gehört nicht alles zusammen (eine Fehlerbehebung und ein unabhängiger Umbau), sag das
   und schlag getrennte Commits vor, statt eine Nachricht über beides zu schreiben.

## Schritt 2 — den Stil des Repos lernen

`git_log` mit den letzten 10–20 Commits. Übernimm, was du dort siehst:

- **Sprache** der Nachrichten (nicht die des Codes).
- **Form der ersten Zeile**: Imperativ („Füge … hinzu") oder Beschreibung, mit oder ohne
  Präfix (`fix:`, `feat(scope):`), Länge (meist unter 72 Zeichen).
- **Körper**: ob es einen gibt, wie lang, ob Aufzählungen üblich sind.
- **Trailer**: `Co-Authored-By:`, Ticket-Nummern.

## Schritt 3 — schreiben

- **Erste Zeile: was der Commit bewirkt**, nicht welche Dateien er anfasst. „Login bricht bei
  abgelaufenem Token nicht mehr ab" statt „auth.rs geändert".
- **Körper: warum.** Was war vorher falsch oder fehlte, und warum ist diese Lösung die
  richtige. Das Was steht im Diff, das Warum nirgends sonst.
- Nenne Folgen, die ein Reviewer kennen muss: geänderte Schnittstellen, Migrationen, ein
  Verhalten, das sich für Nutzer ändert.
- Nichts erfinden: kein „getestet", wenn du keinen Test hast laufen sehen.

Gib die Nachricht in einem Codeblock aus, damit sie sich kopieren lässt.
