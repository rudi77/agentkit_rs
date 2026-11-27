# dokumente — große Dokumente auswerten und vergleichen

agentkit als Dokumenten-Analyst: ein Vertrag mit 80 Seiten wird Abschnitt für Abschnitt
ausgewertet, zwei Angebote werden Kriterium für Kriterium nebeneinandergelegt — und jede
Aussage im Ergebnis trägt die Stelle, an der sie steht.

Alles läuft lokal: keine Vektordatenbank, keine Embeddings, kein externer Dienst außer dem
Modell selbst. Unterstützt sind **PDF** (mit Text-Ebene, kein OCR), **Markdown** und **Text**.

## Die drei Bausteine

| Baustein | Was er tut | Wo |
|---|---|---|
| `doc_ingest` | zerlegt ein Dokument deterministisch in Abschnitte — eine Datei je Überschrift, Text im Wortlaut, Fundstelle im Frontmatter, Gliederung in `index.md` | Agent-Kern (`src/documents.rs`) |
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

Die Executable braucht die Features `pdf` und `graph` (beide in den Release-Binaries). Der
Dokumentmodus kommt als **Plugin** [`plugin/`](plugin) — einmal installieren, dann ist er in
jedem Frontend da:

```bash
agentkit plugins install dokumente          # aus dem Release (Asset agentkit-plugins.json)
agentkit plugins install ./plugin           # oder aus diesem Ordner

# eine gezielte Frage — liest nur die betroffenen Abschnitte
agentkit run dokumente -w /pfad/zu/den/dokumenten "Was steht in vertrag.pdf zur Kündigung?"

# ein Dokument vollständig auswerten
agentkit run dokumente -w /pfad/zu/den/dokumenten \
  "Werte vertrag.pdf vollständig aus: Laufzeit, Kündigungsfrist, Haftungsgrenze, Gerichtsstand."

# zwei Dokumente vergleichen
agentkit run dokumente -w /pfad/zu/den/dokumenten \
  "Vergleiche angebot_mueller.pdf und angebot_schmidt.pdf nach Preis, Lieferzeit, \
   Zahlungsziel und Gewährleistung."

agentkit run dokumente --tui                # im TUI
agentkit run dokumente acp                  # für einen ACP-Client (Zed, Omnigent, …)
```

Was das Plugin enthält:

- den Befehl [`commands/dokumente.md`](plugin/commands/dokumente.md) — sein Text ist der
  System-Prompt und **ersetzt** den eingebauten Coding-Prompt. Der beschreibt eine Methode
  zum Beheben von Fehlern in Code und ist für diese Aufgabe falsch (dieselbe Begründung wie in
  [`../okf_agent`](../okf_agent)). Der Prompt kennt zwei Wege: eine **gezielte Frage**
  beantwortet er über Gliederung und `grep` aus den betroffenen Abschnitten, mit Seite bzw.
  Zeilen als Fundstelle; eine **vollständige Auswertung** oder einen **Vergleich** macht er
  nur auf ausdrücklichen Auftrag, nach den Skills. Eine Chat-Frage zu einem langen
  Tarifvertrag kostet so ein paar Abschnitte statt aller.
- die Skills [`dokument-auswerten`](plugin/skills/dokument-auswerten/SKILL.md) und
  [`dokumente-vergleichen`](plugin/skills/dokumente-vergleichen/SKILL.md) — die
  Vorgehensvorschriften für Weg B.

Zum Ausprobieren liegen zwei Verträge in [`beispiel/`](beispiel): gleicher Aufbau, andere
Kündigungsfrist und Haftungsgrenze, § 2 läuft über einen Seitenumbruch.

```bash
agentkit run dokumente -w beispiel \
  "Vergleiche vertrag_a.pdf und vertrag_b.pdf nach Laufzeit, Kündigungsfrist, Haftung und Gerichtsstand."
```

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
      001-anfang.md              Text vor der ersten Überschrift
      002-1-vertragsgegenstand.md
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
title: "§ 2 Laufzeit und Kündigung"
description: "Abschnitt 3 von 5 aus vertrag_a.pdf (Seiten 1-2)"
resource: "vertrag_a.pdf"
agentkit:
  order: 3
  pages: "1-2"
---

§ 2 Laufzeit und Kündigung

Der Vertrag läuft 24 Monate. Die Kündigungsfrist beträgt 3 Monate zum Quartalsende. …
```

Das Verzeichnis ist ein OKF-Bundle (Open Knowledge Format v0.2), dasselbe Format wie der
Wissensgraph.

Ersetzt du eine Quelle unter gleichem Namen, zerlegt `doc_ingest` sie beim nächsten Aufruf neu:
es entfernt die alten Abschnittsdateien (nur `index.md` und `NNN-<titel>.md`; eigene Dateien
daneben bleiben) und meldet dem Agenten, dass früher Gelesenes nicht mehr gilt. Ist die Quelle
unverändert, geschieht nichts. Aussagen, die eine frühere Auswertung im Graphen festgehalten
hat, bleiben dabei stehen — sie verweisen dann auf die alte Fassung.

## Grenzen

- **Kein OCR.** Ein gescanntes PDF ohne Text-Ebene lehnt `doc_ingest` ab.
- **PDF-Überschriften werden erkannt, nicht ausgelesen** — an größerer Schrift oder einer
  Gliederungsnummer (`§ 7`, `3.2 Titel`, `Artikel 5`). Eine fette Überschrift in Textgröße
  ohne Nummer wird nicht erkannt; ihr Text hängt dann am Abschnitt davor. Findet sich gar
  keine Gliederung, bleibt es bei einem Abschnitt je Seite. Ein Blick in `index.md` zeigt
  sofort, was erkannt wurde.
- **Kopf- und Fußzeilen bleiben im Text.** Sie gelten nicht als Überschrift, stehen aber an
  jedem Seitenumbruch mitten im Abschnitt.
- **Die Textextraktion ist nicht bei jedem PDF sauber.** Je nach Erzeuger und Schrift setzt
  sie einzelne Leerzeichen falsch (`K ündigung`, `2 4 Monate`). Das betrifft `read_pdf`
  genauso und stört `grep` wie das wörtliche Zitieren.
- **Tabellen** kommen aus einem PDF als Fließtext, Spalten können durcheinandergeraten.
- **Verglichen werden Texte, nicht Bedeutungen.** `3 Monate` und `ein Quartal` gelten als
  abweichend. Die Skills weisen den Agenten an, Werte einheitlich zu schreiben und jede
  Abweichung nachzulesen.
- **Die Belegprüfung beweist ein Zitat, keine Deutung.** Dass der Satz im Dokument steht, ist
  geprüft; dass er das Kriterium beantwortet, hat ein Modell entschieden.
