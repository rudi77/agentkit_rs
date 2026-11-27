---
name: diff-review
description: Die aktuellen Änderungen (oder einen Commit) auf Fehler prüfen, bevor sie committet oder gemergt werden — Korrektheit zuerst, mit Fundstelle und konkretem Fehlerfall. Für Aufträge wie "prüf meine Änderungen", "review den Diff", "ist das so richtig".
---

# Diff prüfen

Ziel sind **Fehler, die jemand trifft** — nicht Geschmack. Ein Befund ohne konkreten
Fehlerfall ist keiner.

## Schritt 1 — Umfang

- Ungespeicherte Änderungen: `git_status`, dann `git_diff`.
- Ein Commit: `git_show` mit dem Commit.
- Lies zu jeder geänderten Stelle den **umgebenden Code** mit `read_file` (den Aufrufer, die
  Funktion drumherum). Ein Diff allein zeigt nicht, ob eine Annahme woanders verletzt wird.
- Liegen Projektregeln bei (`CLAUDE.md`, `AGENTS.md`, `CONTRIBUTING.md`, Coding-Guidelines),
  lies sie — ein Verstoß dagegen ist ein Befund.

## Schritt 2 — prüfen, in dieser Reihenfolge

1. **Korrektheit**: falsche Bedingung, vertauschte Argumente, Off-by-one, nicht behandelter
   Fehlerfall, `unwrap`/Null an einer Stelle, die leer sein kann, geänderte Semantik, die ein
   Aufrufer nicht mitbekommt (`grep` nach den Aufrufern).
2. **Daten und Sicherheit**: ungeprüfte Eingaben in Pfaden, Shell-Befehlen, SQL;
   Geheimnisse im Code; Dateien, die gelöscht oder überschrieben werden könnten.
3. **Tests**: Deckt ein Test das neue Verhalten ab? Wurde ein bestehender Test abgeschwächt,
   damit er grün wird?
4. **Erst danach** Lesbarkeit und Vereinfachung — knapp, und als solche gekennzeichnet.

## Schritt 3 — belegen

Zu jedem Befund, bevor du ihn meldest:

- **Fundstelle** `datei:zeile`.
- **Fehlerfall**: welche Eingabe oder welcher Zustand zu welchem falschen Ergebnis führt.
- Lässt er sich mit einem kurzen Befehl oder Test zeigen, tu das (`run_shell`, nach Freigabe).
  Kannst du ihn nicht belegen, sag „möglich" statt „Fehler".

## Bericht

Befunde nach Schwere sortiert, je Befund: Fundstelle, Problem in einem Satz, Fehlerfall,
Vorschlag. Danach, getrennt: was du geprüft und für richtig befunden hast. Keine Befunde ist ein
gültiges Ergebnis — erfinde keine, um etwas zu liefern.
