---
name: fehlersuche
description: Einen Fehler systematisch eingrenzen und beheben — reproduzieren, Ursache finden, gezielt korrigieren, mit Test absichern. Für Aufträge wie "das stürzt ab", "Test X schlägt fehl", "warum kommt hier Y raus", "finde den Bug".
---

# Fehler suchen

Rate nicht. Jeder Schritt erzeugt einen Beleg, der die nächste Vermutung trägt oder
widerlegt.

## Schritt 1 — reproduzieren

- Finde den kleinsten Aufruf, der den Fehler zeigt: einen bestehenden Test, einen Befehl,
  ein kurzes Skript. Führe ihn aus (`run_shell`) und halte die **genaue** Fehlermeldung fest.
- Lässt sich der Fehler nicht reproduzieren, sag das und frag nach dem, was fehlt (Eingabe,
  Umgebung, Version), statt auf Verdacht zu ändern.

## Schritt 2 — eingrenzen

- Folge der Fehlermeldung: Stacktrace, Zeilennummer, `grep` nach der Meldung im Code.
- Prüfe deine Annahmen an der Stelle, an der das Ergebnis zum ersten Mal falsch ist — nicht
  dort, wo es auffällt. Lies den Code mit `read_file`, nicht aus dem Gedächtnis.
- Hilft das nicht weiter: halbiere. Was ist der letzte Zustand, der noch stimmt? Ein
  `git_log` auf die betroffene Datei zeigt, was sich kürzlich geändert hat.

## Schritt 3 — die Ursache benennen

Bevor du etwas änderst, schreib in einem Satz auf: **„Der Fehler entsteht, weil …"** Kannst
du den Satz nicht zu Ende schreiben, bist du noch in Schritt 2.

## Schritt 4 — beheben

- Ändere die Ursache, nicht das Symptom. Ein `try/except` um die Absturzstelle oder ein
  Sonderfall für genau die Eingabe aus der Meldung ist keine Behebung.
- So klein wie möglich: kein Umbau nebenbei.
- **Einen fehlschlagenden Test nie anpassen, damit er grün wird** — es sei denn, der Test
  selbst ist nachweislich falsch; dann sag das ausdrücklich.

## Schritt 5 — absichern

1. Der Reproduktionsfall aus Schritt 1 läuft jetzt richtig.
2. Gibt es keinen Test, der den Fehler gefangen hätte, schreib einen.
3. Lass die übrigen Tests des betroffenen Bereichs laufen.

## Bericht

Ursache (der Satz aus Schritt 3), die Änderung mit Fundstelle, und was du zum Beleg
ausgeführt hast — mit dem Ergebnis, auch wenn etwas rot blieb.
