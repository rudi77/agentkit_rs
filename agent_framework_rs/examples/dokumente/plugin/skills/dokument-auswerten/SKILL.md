---
name: dokument-auswerten
description: Ein großes Dokument (PDF, Markdown, Text) vollständig auswerten — in Abschnitte zerlegen, abschnittsweise lesen, jede Aussage mit wörtlicher Fundstelle im Wissensgraphen festhalten, belegten Bericht schreiben. Für Aufträge wie "werte den Vertrag aus", "was steht in diesem Bericht zu X", "fasse das Dokument mit Belegen zusammen".
---

# Dokument auswerten

Du arbeitest mit drei Dingen: `doc_ingest` zerlegt das Dokument, `read_file` liest einen
Abschnitt, `graph_remember` hält fest, was darin steht. Der Graph ist dein Gedächtnis — was
dort steht, musst du nicht im Kopf behalten, wenn das Dokument länger ist als dein Kontext.

## Schritt 1 — Fragen festlegen

Bevor du liest: Was soll herauskommen? Leite aus dem Auftrag eine kurze Liste von **Kriterien**
ab (`laufzeit`, `kündigungsfrist`, `haftungsgrenze`, …). Kurz, klein geschrieben, ein Begriff
je Kriterium. Nennt der Auftrag keine, leitest du sie in Schritt 2 aus der Gliederung ab.

Die Liste steht fest, bevor du Abschnitte liest. Fällt dir beim Lesen ein weiteres Kriterium
auf, nimm es auf — und sag es im Bericht.

## Schritt 2 — zerlegen

```text
doc_ingest(path: "vertrag.pdf")
```

Das legt `.agentkit/docs/vertrag/` an: `index.md` mit der Gliederung, je Abschnitt eine Datei
mit dem Text im Wortlaut. Lies `index.md`. Sie nennt je Abschnitt die Stelle in der Quelle
(Seiten bei PDF, Zeilen bei Markdown) und die Größe.

Ein Abschnitt ist eine Überschrift mit ihrem Text. Bei einem PDF werden die Überschriften
erkannt (größere Schrift, Nummern wie `§ 7` oder `3.2`); heißen die Abschnitte stattdessen
`Seite 1: …`, wurde keine Gliederung gefunden und das Dokument liegt seitenweise vor. Dann
kann ein Gedanke über zwei Abschnitte laufen — lies beim Zitieren den Nachbarn mit.

`doc_ingest` darfst du jederzeit erneut aufrufen. Meldet es „bereits zerlegt und
unverändert", arbeite mit dem vorhandenen Verzeichnis weiter. Meldet es, die Quelle habe sich
geändert, ist das Verzeichnis neu geschrieben: was du vorher daraus gelesen oder festgehalten
hast, stammt aus der alten Fassung — lies `index.md` und die Abschnitte neu. Meldet es, das
Ziel enthalte kein Bundle dieser Quelle, wähle mit `out` ein anderes Verzeichnis. Meldet es
„kein extrahierbarer Text", ist das PDF ein Scan — sag das dem Nutzer und hör auf; hier gibt
es kein OCR.

## Schritt 3 — abschnittsweise lesen und festhalten

Geh die Abschnitte **in der Reihenfolge der Gliederung** durch. Für jeden:

1. `read_file` auf die Abschnittsdatei — **ganz, ohne `von`/`bis`**. Mit Zeilenbereich stehen
   Zeilennummern im Ergebnis, und ein Zitat über mehrere Zeilen passt dann nicht mehr.
2. Für jede Stelle, die ein Kriterium beantwortet:

   ```text
   graph_remember(
     subject:   "Vertrag A",              ← das DOKUMENT, immer derselbe Name
     predicate: "kündigungsfrist",        ← das KRITERIUM
     object:    "3 Monate zum Quartalsende",
     excerpt:   "Die Frist beträgt 3 Monate zum Quartalsende."   ← WÖRTLICH aus dem Abschnitt
   )
   ```

3. Prüfe die Antwort: `"evidence": true` heißt, das Zitat wurde im gelesenen Abschnitt
   gefunden und die Abschnittsdatei steht als Fundstelle an der Aussage. Bei `false` hast du
   umformuliert — kopiere die Stelle wörtlich (mindestens 12 Zeichen) und halte sie neu fest.

Regeln dabei:

- **Das Dokument ist das Subjekt, das Kriterium das Prädikat.** Nie umgekehrt. Nur so lassen
  sich Dokumente später mit `graph_compare` nebeneinanderlegen.
- **Ein Wert je Kriterium.** Hat ein Kriterium mehrere Werte (drei Ansprechpartner), schreib
  sie in EIN Objekt oder wähle genauere Kriterien (`ansprechpartner technik`).
- **`durable` weglassen.** Die Aussagen gelten für diese Auswertung. Nur wenn der Auftrag
  verlangt, dass das Ergebnis spätere Sitzungen erreicht, setzt du `durable: true`.
- Ein Abschnitt ohne Bezug zu einem Kriterium bekommt keine Aussage. Gelesen hast du ihn
  trotzdem — merk ihn dir für die Bilanz.

Suchst du etwas Bestimmtes, hilft `grep` mit `path: ".agentkit/docs/vertrag"`. Ein Treffer
ersetzt das Lesen nicht: zitieren kannst du nur aus einem Abschnitt, den du gelesen hast.

## Schritt 4 — prüfen

1. `graph_neighbors(entity: "Vertrag A")` zeigt alles, was du festgehalten hast.
2. Geh deine Kriterien durch. Zu welchem gibt es keine Aussage? Such gezielt mit `grep`
   danach. Findest du wirklich nichts, halte auch das fest — ohne `excerpt`, mit dem Objekt
   `nicht geregelt`. So ist „geprüft und nicht vorhanden" von „nicht nachgesehen" zu
   unterscheiden.
3. Zähle: Abschnitte laut `index.md` gegen gelesene Abschnitte.

## Schritt 5 — Bericht

- Je Kriterium: der Wert, das wörtliche Zitat, die Fundstelle (Abschnittsdatei und die Seite
  bzw. die Zeilen aus deren Kopf).
- Was nicht geregelt ist.
- Die Bilanz: „42 Abschnitte, 42 gelesen" — oder welche fehlen und warum.

Schreib nichts in den Bericht, was nicht als Aussage mit `"evidence": true` im Graphen steht
oder ausdrücklich als deine eigene Einschätzung gekennzeichnet ist.
