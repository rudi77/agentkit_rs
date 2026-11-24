# dokumente — große Dokumente auswerten und vergleichen

agentkit als Dokumenten-Analyst: ein Vertrag mit 80 Seiten wird Abschnitt für Abschnitt
ausgewertet, zwei Angebote werden Kriterium für Kriterium nebeneinandergelegt — und jede
Aussage im Ergebnis trägt die Stelle, an der sie steht.

Alles läuft lokal: keine Vektordatenbank, keine Embeddings, kein externer Dienst außer dem
Modell selbst. Unterstützt sind **PDF** (mit Text-Ebene, kein OCR), **Markdown** und **Text**.

## Die drei Bausteine

| Baustein | Was er tut | Wo |
|---|---|---|
| `doc_ingest` | zerlegt ein Dokument deterministisch in Abschnitte — eine Datei je Seite bzw. Überschrift, Text im Wortlaut, Fundstelle im Frontmatter, Gliederung in `index.md` | Agent-Kern (`src/documents.rs`) |
| `graph_remember` | hält eine Aussage als `Dokument — Kriterium — Wert` fest; ein wörtliches Zitat aus einem gelesenen Abschnitt macht die Abschnittsdatei zur Fundstelle | Wissensgraph |
| `graph_compare` | legt die Aussagen mehrerer Dokumente je Kriterium nebeneinander: `gleich`, `abweichend`, `Lücke` | Wissensgraph |

Dazwischen arbeiten die vorhandenen Datei-Tools: `read_file` liest einen Abschnitt, `grep`
sucht über alle.

Warum das ohne Vektorsuche auskommt: Auswerten und Vergleichen heißt, ein Dokument
**vollständig** durchzugehen — nicht, die fünf ähnlichsten Stellen zu finden. Die Arbeit
verteilt sich deshalb so, dass ein Modell nur tut, was Urteil verlangt:

- **Zerlegen** macht Code. Derselbe Input ergibt dieselben Dateien.
- **Lesen und Einordnen** macht das Modell, Abschnitt für Abschnitt.
- **Belegen** prüft die Laufzeit: ein Zitat zählt nur, wenn es wörtlich in einem
  Tool-Ergebnis dieses Laufs steht.
- **Gegenüberstellen** macht wieder Code. Eine Lücke steht in der Antwort, statt zu fehlen.

## Benutzen

Die Executable braucht die Features `pdf` und `graph` (beide in den Release-Binaries).

```bash
# ein Dokument auswerten
agentkit --profile profile.json --skills skills -w /pfad/zu/den/dokumenten \
  "Werte vertrag.pdf aus: Laufzeit, Kündigungsfrist, Haftungsgrenze, Gerichtsstand."

# zwei Dokumente vergleichen
agentkit --profile profile.json --skills skills -w /pfad/zu/den/dokumenten \
  "Vergleiche angebot_mueller.pdf und angebot_schmidt.pdf nach Preis, Lieferzeit, \
   Zahlungsziel und Gewährleistung."
```

- [`profile.json`](profile.json) bindet [`dokument-agent.md`](dokument-agent.md) als
  System-Prompt ein. Er **ersetzt** den eingebauten Coding-Prompt — der beschreibt eine
  Methode zum Beheben von Fehlern in Code und ist für diese Aufgabe falsch (dieselbe
  Begründung wie in [`../okf_agent`](../okf_agent)).
- [`skills/dokument-auswerten`](skills/dokument-auswerten/SKILL.md) und
  [`skills/dokumente-vergleichen`](skills/dokumente-vergleichen/SKILL.md) sind die
  Vorgehensvorschriften.

Nennt der Auftrag die Kriterien, hält sich der Agent daran. Nennt er keine, leitet der Agent
sie aus den Gliederungen ab und nennt sie im Bericht — bei einem Vergleich lohnt es sich, sie
vorzugeben.

## Was dabei entsteht

```text
<workspace>/
  vertrag.pdf
  .agentkit/
    docs/vertrag/              ← doc_ingest
      index.md                   Gliederung
      001-seite-1.md             Text der Seite, wörtlich
      …
    graph/                     ← der Wissensgraph des Workspace
```

Die Aussagen einer Auswertung liegen im Sitzungs-Scope des Graphen: sie gelten für diesen
Lauf und gehören nicht zu dem Wissen, das spätere Sitzungen automatisch vorgelegt bekommen.
Von den Sitzungs-Scopes bleiben die 20 jüngsten — der **Bericht** ist also das Ergebnis, das
du aufhebst, nicht der Graph.

Dauerhaft wird eine Aussage nur, wenn der Agent sie mit `durable: true` festhält oder die
Sitzung mit `--consolidate` läuft. Wer das will, gibt der Auswertung einen eigenen Graphen
(`--graph DIR`), damit Vertragsklauseln nicht im Projektwissen eines Code-Repos auftauchen.

Ein Abschnitt sieht so aus:

```markdown
---
type: Dokumentabschnitt
title: "Seite 12"
description: "Abschnitt 12 von 80 aus vertrag.pdf (Seite 12)"
resource: "vertrag.pdf"
agentkit:
  order: 12
  pages: "12"
---

§ 7 Kündigung
Die Frist beträgt 3 Monate zum Quartalsende. …
```

Das Verzeichnis ist ein OKF-Bundle (Open Knowledge Format v0.2), dasselbe Format wie der
Wissensgraph.

## Grenzen

- **Kein OCR.** Ein gescanntes PDF ohne Text-Ebene lehnt `doc_ingest` ab.
- **PDF-Abschnitte sind Seiten.** Ein PDF trägt keine verlässliche Gliederung; die Seite ist
  die Fundstelle, die ein Mensch nachschlagen kann. Eine Klausel über einen Seitenumbruch
  steht in zwei Abschnitten.
- **Tabellen** kommen aus einem PDF als Fließtext, Spalten können durcheinandergeraten.
- **Verglichen werden Texte, nicht Bedeutungen.** `3 Monate` und `ein Quartal` gelten als
  abweichend. Die Skills weisen den Agenten an, Werte einheitlich zu schreiben und jede
  Abweichung nachzulesen.
- **Die Belegprüfung beweist ein Zitat, keine Deutung.** Dass der Satz im Dokument steht, ist
  geprüft; dass er das Kriterium beantwortet, hat ein Modell entschieden.
