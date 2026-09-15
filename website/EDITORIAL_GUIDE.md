# Redaktionsleitfaden für AgentKit-Wiki und Blog

Dieser Leitfaden beschreibt, wie die öffentliche AgentKit-Dokumentation und der
Blog geschrieben werden. Er hält die Arbeitsweise der gemeinsam überarbeiteten
Website fest. Ziel ist, dass Leser einen technischen Zusammenhang verstehen,
eine passende Funktion auswählen und ein Beispiel selbst nachvollziehen können.

## 1. Geltungsbereich und verpflichtende Lektüre

**Lies diesen Leitfaden vollständig, bevor du Wiki-Inhalte aktualisierst oder
einen Blogbeitrag entwirfst, schreibst oder überarbeitest.** Er gilt auch für
Übersichten, Vorschautexte, Seitentitel, Beschreibungen und Inhaltsverzeichnisse.
Prüfe den fertigen Text anschließend anhand der Checkliste am Ende.

Mit „Wiki“ ist hier die öffentliche Dokumentation unter `website/` gemeint:
Startseite, Funktionsübersicht, Architektur, Designprinzipien und Einstieg.
Für separat beauftragte Wiki-Inhalte gelten dieselben redaktionellen Regeln.
Der Blog liegt unter `website/blog/`. Technische Referenzen und interne
Planungsdokumente behalten ihren jeweiligen Zweck; sie werden nicht bei jeder
Website-Änderung automatisch umgeschrieben.

Die öffentlichen Seiten und Blogbeiträge bleiben **Englisch**, solange der
Auftrag keine andere Sprache verlangt. Dieser Leitfaden und interne Hinweise
sind Deutsch. Namen von APIs, Flags, Dateien und Ereignissen bleiben unverändert.
Verwende im veröffentlichten Text die Projektschreibweise `agentkit`.

Die Leseanweisung ist in der Root-`AGENTS.md` und der Root-`CLAUDE.md` verankert.
Sie ist eine Arbeitsanweisung für Assistenten, die diese Dateien beachten;
ein Markdown-Dokument allein kann das Verhalten beliebiger Werkzeuge nicht
technisch erzwingen. Die redaktionellen Regeln werden nur hier gepflegt.

## 2. Zielgruppe und Erklärhaltung

Schreibe für technisch interessierte Leser, die mit Dateien, Befehlen und
grundlegenden Programmierkonzepten umgehen können, aber AgentKit noch nicht
kennen. Setze Kenntnisse der internen Crates, des Actor-Modells, von MCP oder
der Kontextverwaltung nicht voraus.

Eine Einführung beginnt bei einer nachvollziehbaren Aufgabe: eine Funktion
verstehen, einen Testfehler untersuchen oder eine längere Arbeit wieder aufnehmen.
Erst wenn klar ist, was dabei passiert, bekommt der Leser die internen Namen.
Eine Rust-Implementierungsanalyse darf mehr voraussetzen, muss dieses Vorwissen
aber am Anfang benennen.

Die Leitfrage beim Schreiben lautet: **Was muss der Leser an dieser Stelle
wissen, damit der nächste Schritt verständlich wird?** Beantworte diese Frage
im Text, statt Leser von Begriff zu Begriff zu schicken.

## 3. Schreibstil

### Ton und Wortwahl

- Schreibe sachlich, zugänglich und wie ein Kollege, der einen Ablauf erklärt.
- Verwende konkrete Verben: Der Agent liest eine Datei, das Tool liefert Text,
  der Worker speichert einen Versuch. Benenne jeweils den handelnden Teil.
- Nutze kurze bis mittellange Sätze. Ein längerer Satz ist sinnvoll, wenn er
  einen Zusammenhang verständlich erklärt; Kürze ist kein Selbstzweck.
- Entwickle pro Absatz einen Hauptgedanken. Verbinde Absätze über ihre Inhalte.
- Nutze direkte Ansprache für Anleitungen. Ein zurückhaltendes „we“ passt zu
  einem gemeinsam durchgeführten Beispiel. Erfinde keine persönlichen Erlebnisse,
  Messungen oder Autorenerfahrungen.
- Führe Fachbegriffe bei ihrer ersten Verwendung ein. Erkläre beispielsweise
  ein Artefakt als Ergebnisdatei für spätere Arbeitsschritte und einen Quorumwert
  als die benötigte Zahl von Stimmen.
- Beschreibe Gründe, Folgen und relevante Grenzen einer Entscheidung.
  Ordne Vermutungen ausdrücklich als Vermutungen ein.

### Was vermieden werden soll

- Werbesprache wie „revolutionary“, „seamless“, „powerful“ oder „blazing fast“,
  wenn keine konkrete, belegte Aussage dahintersteht.
- Selbstlob wie „boring on purpose“, „that's the whole story“ oder „honestly,
  that's all there is to it“. Solche Sätze ersetzen keine Erklärung.
- Wiederkehrende Gegensatzformeln wie „not X, but Y“, „X, not Y“ oder
  „This isn't about X. It's about Y.“ Erkläre Unterschiede direkt am Verhalten.
- Ketten aus Abstraktionen wie „watermark-driven, byte-stable, actor-based
  orchestration“ ohne vorher eingeführte Begriffe und ein Beispiel.
- Künstlich abgehackte Sätze, rhetorische Fragen mit sofortiger Kurzantwort und
  immer gleiche Dreiergruppen oder Schlussformeln.
- Unbelegte Absolutheiten: „always safe“, „no failure is possible“, „every crate
  is offline by default“, „the only extension point“.
- Vorsichtshinweise, die nur hypothetische Risiken aufzählen. Erläutere Grenzen
  dort, wo sie die Nutzung oder Interpretation tatsächlich beeinflussen.

Es geht um die Verständlichkeit des gesamten Textes. Das mechanische Ersetzen
einzelner verdächtig klingender Wörter reicht dafür nicht aus.

### Vorher und nachher

**Zu abstrakt:**

> ctxman provides watermark-driven garbage collection with content-addressed
> externalization and byte-stable rendering.

**Erklärend:**

> Reading many files adds more text to the next model request. ctxman can save
> large tool results outside the prompt and leave a reference in their place.
> The agent can retrieve the saved content when it needs it again.

Danach können die Begriffe „externalization“ und die konkreten Schwellenwerte
eingeführt werden, sofern sie für diesen Abschnitt gebraucht werden.

**Zu pauschal:**

> The agent can die; the work state doesn't.

**Nachvollziehbar:**

> The work runtime records completed items and active attempts in a journal.
> After an interruption, it uses those records to determine what remains to
> be done. Changes already made by an interrupted command may still be present.

## 4. Aufbau einer Erklärung

Als Ausgangspunkt eignet sich diese Reihenfolge:

1. **Aufgabe oder Problem:** Was möchte jemand erreichen?
2. **Benötigter Begriff:** Welche Idee muss dafür verstanden werden?
3. **Konkreter Ablauf:** Wer tut was, und welche Information geht wohin?
4. **Beispiel:** Eine kleine Eingabe, ein Befehl, Code oder ein Diagramm.
5. **Ergebnis:** Was entsteht, und was lässt sich daraus schließen?
6. **Folgen und Grenzen:** Wann hilft das, welche Kosten oder Einschränkungen
   sind relevant?
7. **Weiterführung:** Ein gezielter Link zum nächsten Schritt oder zur Quelle.

Diese Reihenfolge ist eine Hilfe, keine obligatorische Folge von sieben
Zwischenüberschriften. Bei kleinen Änderungen genügen wenige Absätze. Schreibe
Übergänge nur dort, wo sie erklären, weshalb das nächste Thema folgt.

## 5. Aufbau der Wiki-Seiten

Jede Seite beantwortet eine eigene Leserfrage. Vermeide, auf allen Seiten
denselben vollständigen Funktionskatalog zu wiederholen.

| Seitentyp | Leserfrage | Aufbau |
|---|---|---|
| Startseite | Was ist AgentKit, und wo beginne ich? | Konkrete Tätigkeit erklären, einen kleinen Einstieg zeigen, passende Vertiefungen anbieten. |
| Einstieg | Wie bekomme ich einen ersten Lauf zum Funktionieren? | Voraussetzungen, Installation, Demo, Modellkonfiguration, erste Datei, Ergebnis, typische Fehler, optionale Erweiterungen. |
| Funktionsübersicht | Welche Fähigkeit brauche ich für meine Aufgabe? | Nach Aufgaben gliedern; pro Funktion Zweck, kleines Beispiel, Aktivierung und wichtige Grenzen erklären. |
| Architektur | Wie läuft eine Anfrage durch das System? | Modell-/Tool-Ablauf zuerst, dann Ereignisse und Integration; Crate-Namen und Abhängigkeiten anschließend als Orientierung. |
| Designprinzipien | Warum ist es so implementiert? | Entscheidung an einem Beispiel erklären, Gründe und Tradeoffs nennen, Beitrag zur Wartbarkeit beschreiben. |

Längere Seiten erhalten ein Inhaltsverzeichnis mit funktionierenden Ankern.
Überschriften sollen Inhalt oder Tätigkeit benennen, etwa „Registering tools“
oder „Manage a growing conversation“. Bestehende Anker bleiben auch dann
erreichbar, wenn Überschriften neu formuliert werden.

Ein Einstieg führt zunächst einen einfachen Weg vollständig durch. Erweiterungen
wie Graph, Swarm und Work folgen erst danach. Eine minimale Anleitung muss nicht
jede Option zeigen; die vollständige Referenz wird passend verlinkt.

## 6. Aufbau eines Blogbeitrags

Ein Blogbeitrag entwickelt einen technischen Zusammenhang ausführlicher als die
Funktionsübersicht. Er hat eine klare Fragestellung und ein durchgehendes Beispiel.

### Empfohlenes Gerüst

1. **Präziser Titel:** Beschreibt, was Leser verstehen werden. Keine künstlichen
   Zeitversprechen wie „everything in five minutes“.
2. **Metadaten:** Tatsächliches Veröffentlichungsdatum, bei inhaltlicher
   Überarbeitung zusätzlich Änderungsdatum; relevante Version, wenn geprüft.
3. **Einstieg:** Eine konkrete Situation und der Zusammenhang, den der Artikel
   daran erklärt. Umfang und vorausgesetztes Wissen kurz benennen.
4. **Orientierung:** Bei längeren Artikeln ein Inhaltsverzeichnis.
5. **Grundmechanismus:** Den einfachsten Ablauf vollständig erklären.
6. **Durchgeführtes Beispiel:** Eingabe, Verarbeitung und Ergebnis zusammenhalten.
7. **Vertiefung:** Erst danach Implementierungsdetails, Varianten und verwandte
   Komponenten einführen. Immer auf das erklärte Beispiel zurückbeziehen.
8. **Einordnung:** Nutzen, Aufwand, Grenzen und offene Fragen konkret benennen.
9. **Abschluss:** Die gewonnenen Erkenntnisse knapp zusammenführen und passende
   nächste Schritte verlinken. Keine neuen Themen und keine pauschalen Versprechen.

Der vorhandene Artikel
[`Following a task through the agent loop`](blog/anatomy-of-an-agent-loop.html)
zeigt dieses Vorgehen: Tool-Anfrage, Rückgabe des Ergebnisses, ausführbarer
Rust-Code, Übertragung auf Dateiänderungen und anschließend Kontext sowie
Zusammenarbeit. Er ist ein redaktionelles Beispiel, keine unveränderliche
Vorlage für jeden künftigen Artikel.

Ein Beitrag über Kontextverwaltung braucht nicht erneut alle Crates vorzustellen.
Verlinke bereits erklärte Grundlagen. Versprich Folgeartikel nur, wenn sie
tatsächlich geplant und Teil des Auftrags sind. Lesezeiten nur verwenden, wenn
sie aus dem aktuellen Text plausibel ermittelt wurden; sonst weglassen.

## 7. Code, Befehle, Tabellen und Diagramme

### Beispiele müssen nachvollziehbar sein

- Voraussetzungen nennen: installierte Programme, Modellzugang, Features,
  Arbeitsverzeichnis und benötigte Eingabedateien.
- Bash und PowerShell getrennt kennzeichnen, sobald die Syntax unterschiedlich
  ist. Ein Windows-Pfad gehört nicht unkommentiert in ein allgemeines Beispiel.
- Platzhalter ausdrücklich benennen und erklären, womit sie ersetzt werden.
- Vollständige Beispiele mit Manifest, Imports und Startbefehl bereitstellen
  oder diese direkt verlinken. Auszüge und Pseudocode als solche kennzeichnen.
- Nach dem Beispiel erwartetes Ergebnis und Bedeutung erklären. Ausgabe nur
  als tatsächlich beobachtet bezeichnen, wenn das Beispiel ausgeführt wurde.
- JSON muss ohne erklärende Kommentare kopierbar sein; kommentierte Formate
  ausdrücklich als JSONC oder schematische Darstellung kennzeichnen.
- Demo und Fake-Modell vom echten Modell unterscheiden. Ein geskripteter
  Tool-Aufruf belegt das Verhalten der Runtime für diese Sequenz, keine
  Fähigkeit eines realen Modells, das richtige Tool auszuwählen.
- Prüfungen passend zum Beispiel durchführen. Keine echten Modellaufrufe oder
  kostenpflichtigen Aktionen allein für eine unnötige Dokumentationsprüfung.

### Visualisierungen gezielt einsetzen

Ein Diagramm erklärt zum Beispiel einen Nachrichtenfluss oder eine Abhängigkeit.
Beschrifte Pfeile: „requests“, „returns“ oder „depends on“. Erläutere in einer
Bildunterschrift, was dargestellt und was vereinfacht wurde. Ein Diagramm mit
falscher Pfeilrichtung ist irreführender als ein klarer Absatz.

Tabellen eignen sich für echte Vergleiche: Situation, passende Funktion,
zusätzlicher Nutzen. Listen eignen sich für Schritte oder kurze Referenzen.
Der erklärende Zusammenhang bleibt Fließtext. Die Website unterstützt einfache
`<pre>`-Diagramme; führe für einen Textbeitrag keine neue Build-Abhängigkeit ein.

## 8. Sachliche Genauigkeit und Quellen

Prüfe Aussagen gegen die aktuelle Implementierung und ihre Konfiguration.
Ältere Wiki-Seiten, Kommentare und Planungsdokumente können veraltet sein.
Nutze CodeGraph zur Code-Orientierung, wenn das Repository indexiert ist.
Unterscheide implementiertes Verhalten, geplante Funktionen und Vermutungen.

Besonders prüfbedürftig sind:

- Standardwerte gegenüber harten Obergrenzen und vom Nutzer gesetzten Werten;
- Offline-Testbarkeit gegenüber Standardbuilds und Release-Features;
- direkte, optionale und indirekte Abhängigkeiten;
- Dateitool-Grenzen gegenüber Shell- und MCP-Prozessen;
- Wiederaufnahme gespeicherten Zustands gegenüber Rücknahme externer Effekte;
- Abstimmung, Selbstprüfung und unabhängige Verifikation;
- verlustfrei gespeicherte Originale gegenüber möglicherweise verlustbehafteter
  Zusammenfassung.

Performance-Aussagen nennen Messaufbau, Vergleich, Version beziehungsweise
Stand und die Grenzen der Aussage. Framework-Overhead mit einem Fake-Modell
belegt weder die Latenz noch die Erfolgsquote einer realen Coding-Aufgabe.
Übernimm keine alten Zahlen ungeprüft in einen neuen Beitrag.

Verlinke die passende Quelldatei, Referenz oder Messung unmittelbar am
zugehörigen Abschnitt. Verwende für lokale Website-Seiten relative Links.
Aktuelle Anleitungen können auf `main` zeigen; eine historische Analyse sollte
bei Bedarf einen konkreten Commit oder Tag referenzieren.

## 9. Redaktionelle Referenzen

Die vom Nutzer genannten Artikel dienen als Orientierung für die didaktische
Vorgehensweise:

- [The Big LLM Architecture Comparison](https://magazine.sebastianraschka.com/p/the-big-llm-architecture-comparison)
- [The State of LLM Reasoning Model Inference](https://magazine.sebastianraschka.com/p/state-of-llm-reasoning-and-inference-scaling)

Relevant sind der schrittweise Aufbau, früh erklärte Grundlagen, konkrete
Beispiele, vergleichbare Gegenüberstellungen und die Einordnung von Ergebnissen.
Formulierungen und Beispiele für AgentKit werden eigenständig geschrieben.
Die Artikel sind keine Belege für AgentKit-Funktionen. Ihre erneute Lektüre ist
bei Routineänderungen nicht erforderlich; dieser Leitfaden enthält die dauerhaft
anzuwendenden Regeln.

## 10. Arbeitsablauf und Abnahme

### Vor dem Schreiben

1. Diesen Leitfaden und `website/README.md` lesen.
2. Betroffene Seiten samt Links und Vorschautexten lesen.
3. Leserfrage, benötigtes Vorwissen und ein tragendes Beispiel festlegen.
4. Technische Aussagen, Optionen und Zahlen am aktuellen Stand prüfen.

### Beim Überarbeiten

Den Inhalt bei Bedarf neu ordnen, statt nur Sätze zu glätten. Fachliche Details
erhalten oder gezielt in eine verlinkte Referenz verschieben. Umfang nach der
Leserfrage bemessen; weder notwendige Zwischenschritte kürzen noch jeden Absatz
zu einem vollständigen Grundlagenkurs ausbauen.

Bei Blogänderungen `<title>`, Meta-Beschreibung, sichtbaren Titel und Vorschautexte
in `blog/index.html` und `index.html` abgleichen. Veröffentlichungsdatum erhalten;
bei substanzieller Überarbeitung das tatsächliche Änderungsdatum ergänzen.
Vorhandene URLs und Abschnittsanker erhalten. Bei einer unumgänglichen
Umbenennung Weiterleitungen beziehungsweise kompatible Anker vorsehen.

### Checkliste vor der Abgabe

- [ ] Der Text beantwortet eine konkrete Leserfrage.
- [ ] Fachbegriffe werden eingeführt, bevor sie zum Verständnis nötig sind.
- [ ] Absätze erklären Abläufe und Gründe; Merkmalslisten ersetzen sie nicht.
- [ ] Der Ton ist natürlich und präzise, ohne Werbeformeln und Gegensatz-Slogans.
- [ ] Beispiele nennen Voraussetzungen, Eingaben, Startbefehl und Ergebnis.
- [ ] Fakten, Versionsangaben und Zahlen wurden geprüft; Unsicherheit ist benannt.
- [ ] Quellenlinks führen zur passenden Referenz oder Implementierung.
- [ ] Titel, Beschreibung, Inhaltsverzeichnis und Vorschautexte passen zusammen.
- [ ] Lokale Links und Fragmente funktionieren; bestehende Anker bleiben erhalten.
- [ ] HTML-Struktur ist gültig; jede Seite hat genau eine Hauptüberschrift.
- [ ] Betroffene Seiten wurden in breiter und schmaler Ansicht angesehen;
  lange Befehle und Tabellen erzeugen keinen seitlichen Seitenüberlauf.
- [ ] Kopierbare Beispiele wurden angemessen geprüft; nicht ausgeführte Beispiele
  werden nicht als getestet ausgegeben.
- [ ] Der Abschlussbericht nennt Änderungen, Prüfungen und verbleibende Grenzen.

Für reine Rechtschreibkorrekturen reichen gezielte Prüfungen. Neue ausführbare
Beispiele und größere Umbauten benötigen entsprechend gründlichere Kontrolle.
Die technische Anleitung für Vorschau und Veröffentlichung steht in
[`README.md`](README.md). Eine Textüberarbeitung ist keine automatische
Aufforderung zum Veröffentlichen; beachte den jeweiligen Auftrag.
