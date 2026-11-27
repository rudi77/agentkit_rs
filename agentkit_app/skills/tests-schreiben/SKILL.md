---
name: tests-schreiben
description: Tests für vorhandenen oder neuen Code schreiben, wie das Projekt sie schon schreibt — ein Verhalten je Test, lesbar als Spezifikation, ohne Netz und ohne Zufall. Für Aufträge wie "schreib Tests für X", "deck das mit Tests ab", "füge einen Regressionstest hinzu".
---

# Tests schreiben

## Schritt 1 — wie das Projekt testet

Bevor du einen Test schreibst, finde zwei, drei bestehende in der Nähe (`glob_files`,
`grep` nach dem Test-Framework oder nach dem Namen der Funktion). Übernimm:

- Ort und Benennung der Testdateien und -funktionen (auch die Sprache der Namen),
- das Framework und seine Hilfsfunktionen (Fixtures, Fakes, temporäre Verzeichnisse),
- wie Ein- und Ausgaben aufgebaut sind.

Führ die vorhandenen Tests einmal aus (`run_shell`), damit du weißt, wie sie laufen und ob
sie vorher grün sind.

## Schritt 2 — was zu testen ist

Leite die Fälle aus dem **Verhalten** ab, nicht aus den Codezeilen:

- der Normalfall,
- die Ränder: leer, eins, Maximum, Grenzwert genau,
- die Fehlerfälle: ungültige Eingabe, fehlende Datei, was der Code als Fehler zurückgibt,
- bei einem Bug: genau der Fall, der ihn ausgelöst hat (Regressionstest).

## Schritt 3 — schreiben

- **Ein Verhalten je Test**, und der Name sagt welches.
- Der Test ist **deterministisch**: kein Netz, keine Uhrzeit, kein Zufall ohne festen
  Startwert, keine Abhängigkeit von der Reihenfolge. Temporäre Dateien in einem eigenen
  Verzeichnis, das der Test wieder entfernt.
- Prüfe das Ergebnis genau (`== erwartet`), nicht nur „kein Fehler".
- Fehlermeldungen der Zusicherung zeigen den tatsächlichen Wert.

## Schritt 4 — prüfen, dass der Test etwas prüft

- Lass ihn laufen: er ist grün.
- Bei einem Regressionstest: er muss **ohne** die Behebung rot sein. Wenn du das nicht
  zeigen kannst, sag es.
- **Bestehende Tests änderst oder löschst du nicht**, um einen neuen grün zu bekommen. Steht
  ein bestehender Test im Widerspruch zum gewünschten Verhalten, halt an und sag es.

## Bericht

Welche Fälle du abgedeckt hast und welche bewusst nicht, und die Zusammenfassungszeile des
Testlaufs im Wortlaut.
