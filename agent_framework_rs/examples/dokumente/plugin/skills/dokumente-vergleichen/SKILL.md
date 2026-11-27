---
name: dokumente-vergleichen
description: Zwei oder mehr Dokumente inhaltlich vergleichen (Angebote, Verträge, Vertrag gegen Vorlage, Richtlinien) — gemeinsames Kriterienschema, je Dokument dieselben Aussagen mit Fundstelle, Gegenüberstellung mit Abweichungen und Lücken. Für Aufträge wie "vergleiche die beiden Angebote", "worin unterscheiden sich die Verträge", "was fehlt in B gegenüber A".
---

# Dokumente vergleichen

Ein Vergleich ist nur so gut wie sein Schema: Du stellst jedem Dokument **dieselben Fragen**
und legst die Antworten nebeneinander. Das Nebeneinanderlegen macht `graph_compare` — du
sorgst dafür, dass die Antworten vergleichbar aufgeschrieben sind.

Die Arbeit je Dokument beschreibt der Skill `dokument-auswerten`. **Lade ihn mit
`read_skill` und befolge seine Schritte 2 bis 4 für jedes Dokument.** Hier steht nur, was
beim Vergleichen dazukommt.

## Schritt 1 — das Schema, EINMAL und vorab

Lege die Kriterien fest, bevor du das erste Dokument auswertest:

- Nennt der Auftrag Kriterien, sind es diese.
- Sonst: zerlege alle Dokumente (`doc_ingest`), lies ihre `index.md` und leite aus den
  Gliederungen eine gemeinsame Liste ab. Ein Thema, das nur EIN Dokument hat, gehört auf die
  Liste — genau solche Lücken soll der Vergleich zeigen.

Schreib die Liste auf (im ersten Absatz deiner Antwort oder in einer Datei) und halte dich
daran. **Jedes Kriterium heißt in jedem Dokument gleich**: `kündigungsfrist` und
`kuendigungsfristen` sind für den Vergleich zwei verschiedene Fragen.

## Schritt 2 — jedes Dokument gegen das Schema auswerten

Dokument für Dokument, mit `dokument-auswerten`. Achte auf:

- **Ein fester Name je Dokument** als Subjekt (`Angebot Müller`, `Angebot Schmidt`), von der
  ersten bis zur letzten Aussage.
- **Werte vergleichbar schreiben.** Verglichen werden die Objekte als Text. `3 Monate` in
  einem und `drei Monate` im anderen Dokument gelten als abweichend. Schreib Zahlen als
  Ziffern, Beträge mit Währung (`5.000 EUR`), Fristen in derselben Einheit. Das wörtliche
  Zitat steht im `excerpt` — das Objekt darf normalisiert sein.
- **Jedes Kriterium in jedem Dokument prüfen.** Findest du nach gezielter Suche (`grep`)
  nichts, halte `nicht geregelt` fest. Eine bloße Lücke heißt sonst nur: nicht nachgesehen.

## Schritt 3 — gegenüberstellen

```text
graph_compare(entities: ["Angebot Müller", "Angebot Schmidt"])
```

Je Kriterium eine Zeile pro Dokument, markiert als

- `gleich` — alle sagen dasselbe,
- `abweichend` — mindestens eines sagt etwas anderes,
- `Lücke` — zu mindestens einem gibt es keine Aussage.

Jede `Lücke` ist ein offener Punkt für DICH: Entweder du hast das Kriterium in dem Dokument
noch nicht geprüft (dann jetzt), oder du hast es unter anderem Namen festgehalten (dann mit
`graph_retract` zurückziehen und richtig benannt neu festhalten). Wiederhole `graph_compare`,
bis keine ungeprüfte Lücke bleibt.

Jedes `abweichend` liest du nach: Ist es ein echter Unterschied oder nur eine andere
Schreibweise? Bei einer Schreibweise korrigierst du die Aussage; bei einem echten Unterschied
holst du mit `graph_evidence(claim_id)` die Fundstellen beider Seiten.

## Schritt 4 — Bericht

1. Das Schema.
2. Eine Tabelle: Zeilen = Kriterien, Spalten = Dokumente, dazu die Bewertung.
3. Zu jedem `abweichend` und jedem `nicht geregelt`: die wörtlichen Zitate mit Fundstelle
   (Abschnittsdatei, Seite bzw. Zeilen) — und, getrennt und als solche gekennzeichnet, deine
   Einschätzung, was der Unterschied bedeutet.
4. Die Bilanz je Dokument: Abschnitte gesamt, gelesen, ausgelassen.

Der Vergleich stellt fest, WAS abweicht. Ob eine Abweichung gut oder schlecht ist, sagst du
nur, wenn der Auftrag einen Maßstab nennt.
