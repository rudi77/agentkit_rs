# TANDEM v2 – Anforderungs- und Abnahmenachweis

Quelle: `C:/Users/rudi/Downloads/TANDEM_Cooperative_Software_Engineering_Konzept_v2.pdf`, Version 2.0, 03.11.2026.

Der Auftrag umfasst den gesamten Katalog F-01 bis F-56, die nichtfunktionalen
Anforderungen in Kapitel 13 sowie die Abläufe und Abnahmen in Kapitel 9 und 14.
MVP-Prioritäten reduzieren diesen Auftrag nicht. Das PDF unterscheidet Ziele,
technische Vorschläge und offene Forschungsfragen. VS Code und vorhandenes
AgentKit mit Azure sind durch den Nutzer vorgegeben. Vorgeschlagene separate
Dienste, DAP, Roslyn und Graphdatenbank sind keine verpflichtenden Technologien.

„Vorhanden“ bezeichnet Implementierungsansätze, keinen vollständigen Nachweis.
Ein Eintrag wird erst nach gezielter Funktionsprüfung als nachgewiesen markiert.
Insbesondere beweist ein bestandener Mini-Patch-Test keinen generischen Simulator,
keine fachlichen Invarianten und keine beliebige Sprachabdeckung.

| ID | Gefordertes Verhalten | Abnahme / noch erforderliche Evidenz |
| --- | --- | --- |
| F-01 | Iterative gemeinsame Entwicklung | Mehrere Dialog-, Plan- und Codezyklen im echten Host |
| F-02 | Vage Idee präzisieren | Rückfrage ohne Codeauswahl und Übernahme neuer Erkenntnis |
| F-03 | Aktuellen Schritt am Code besprechen | Kontextgebundene Frage mit Quellverweis |
| F-04 | Sichtbarer beidseitig editierbarer Plan | Persistenter Plankontext während Codearbeit; Agentenvorschlag überschreibt menschliche Edits nicht |
| F-05 | Ergänzen, streichen, teilen, ordnen, hinterfragen | Alle Operationen einschließlich Zusammenführen/Zurückstellen, ohne Belegverlust |
| F-06 | Bedarf und Nicht-Bedarf beidseitig | Strukturierte Felder, Entscheidungen, Vorschläge und wirksame Invalidierung |
| F-07 | Veraltete Vorschläge erkennen | Code-/Plan-/Contractänderung vor und während eines Laufs |
| F-08 | Risiken und Alternativen | Agentenverhalten mit konkretem Zielkonflikt prüfen |
| F-09 | Transparente Modi | Explore lesend; Pair Vorschlag; Delegate explizite Rechte |
| F-10 | Erkenntnisse dauerhaft | Urheber, Grund, Bezug und Historie über Host-Neustart |
| F-11 | Minimalität auf allen Ebenen | Minimalitätsfelder und konkrete Wiederverwendungsentscheidung |
| F-12 | Kleinste heutige Änderung | Agentenkontext, lokaler Draft und Minimalitätsdiskussion |
| F-13 | Zweck, Scope, Nicht-Ziele, Prüfung vorher | Vorschlagsaktion zeigt vollständigen aktuellen Contract |
| F-14 | Dauerhaft echter Code | Native Editoransicht bei Analyse, Planung, Simulation, Diff |
| F-15 | Konkrete Zeilen live verfolgen | Vorschau/Decorations während Patchentstehung |
| F-16 | Vorschau pausieren, bearbeiten, verwerfen | Editierbarer Draft mit erneuter Scopeprüfung; Stop |
| F-17 | Eigene Edits und Agentenreview | Kein Überschreiben, Analyse menschlicher Änderungen |
| F-18 | Vorschlag, Apply und Verifikation getrennt | Tatsächliche Zustandsübergänge und persistente Evidenz |
| F-19 | Verbundene Viewer | Produkt-, Architektur- und lokaler Kontext mit Codeankern |
| F-20 | Methode und Nachbarn | LSP-Symbole, Aufrufer und ausgehende Aufrufe, fehlende Daten sichtbar |
| F-21 | Produkt, Architektur, Impact, Code | Navigation zwischen Sichten, keine erfundene Vollständigkeit |
| F-22 | Bidirektionaler Fokus | Codeauswahl aktualisiert Viewer; Knoten öffnet Code |
| F-23 | Herkunft von Beziehungen | LSP, importierte Doku und Hypothese unterscheidbar |
| F-24 | Patchauswirkung vorher | Betroffene Zeilen/Symbole/Referenzen und Unsicherheiten |
| F-25 | Simulation ab Methode/Position | Snapshotbindung und explizites unterstütztes Subset |
| F-26 | Eigene Eingaben/Objekte | Validierte Szenarioeingaben, Stubs mit Herkunft |
| F-27 | Schrittweise ohne Produktstart | Deterministischer Interpreter, keine eval-/Produktausführung |
| F-28 | Werte, Bedingungen, Ablauf | Schritt zeigt Codezeile, Zustand, Zweig und Begründung |
| F-29 | Vertiefen und Call Stack | Step Into/Over/Out bekannter Methoden, Unknown an externen Ports |
| F-30 | Zurück, Szenariowechsel, Replay | Reproduzierbarer Trace und getrennte Szenarioversionen |
| F-31 | Unübernommenen Patch simulieren | Draft-Text ohne Apply auswerten |
| F-32 | Gleiches Szenario vergleichen | Baseline/Draft nebeneinander, erste Divergenz |
| F-33 | Ehrliche Evidenz | Ableitung, Annahme, Hypothese, Unknown; unbekannter Effekt stoppt |
| F-34 | Szenario als Testidee | Tracegebundenes Abnahmekriterium, getrennt von realem Test |
| F-35 | Breite sinnvolle Einstiegspunkte | Langfristiges Forschungsziel: kein Vollinterpreter-Versprechen; unterstützte und unbekannte Syntax explizit prüfen |
| F-36 | Echte Build-/Testresultate | Prozess, Exitcode, Codebindung und Seiteneffekte prüfen |
| F-37 | Revisionsgebundene Simulation | Snapshot-/Szenario-/Plan-/Contractversion, Invalidierung |
| F-38 | Keine unberechtigten Writes | Alle Agententools und kontrollierter Apply-Pfad prüfen |
| F-39 | Nachvollziehbare Artefakte | Historische Entscheidungen, Patches und Prüfergebnisse exportieren |
| F-40 | Factory-Artefakte | Versioniertes Übergabeformat mit Intent, Scope, Akzeptanz und Evidenz; kein externer Factorybetrieb vorausgesetzt |
| F-41 | Mensch als aktiver Entwickler | Direkte Edits, Planänderung, Fragen und Entscheidungen jederzeit |
| F-42 | Schnelle Analyse ohne Writes | Read-only Toolregistry und Repository-Erkundung |
| F-43 | Verständliche fachliche Portionen | Schritt teilen/zusammenführen und lokalen Kontext halten |
| F-44 | Keine ungefragten Nebenarbeiten | Gegenproben außerhalb Datei/Region/Operation und vor Apply |
| F-45 | Vollständiger gemeinsamer Contract | Bedarf, Nicht-Ziele, Invarianten, Akzeptanz, Rechte, Version, Baseline |
| F-46 | Scopeabweichung zurückweisen | Absichtlich unzulässiger Patch bleibt unapplied |
| F-47 | Technisch begrenzte Schreibrechte | Keine Shell-/Datei-Schreibtools; keine Umgehung über Modellantwort |
| F-48 | Scopeerweiterung begründen | Strukturierter Konflikt mit Symbol, Grund, Alternative, Risiko und Entscheidung |
| F-49 | Contract ändern/verkleinern/Stop | Lauf abbrechen, alte Rechte/Vorschläge invalidieren |
| F-50 | Keine Änderung empfehlen | Dialog ohne Patch und fachlicher Klärungsabschluss |
| F-51 | Fragen und eigene Edits während Lauf | Race-Tests, Stop, keine verlorenen menschlichen Edits |
| F-52 | Begrenzte Delegation | Explizite Policy, automatischer begrenzter Zyklus mit realer Prüfung |
| F-53 | Erklär-/Unterbrechungsgrad | Kurze/tiefe Erklärung, konstante Schutzregeln |
| F-54 | Delegation bei Abweichung stoppen | Unklarheit/Scopekonflikt/fehlgeschlagene Prüfung eskaliert |
| F-55 | Gemeinsame Versionierung | Plan/Contract/Patch/Szenario/Evidenz über Historie rekonstruieren |
| F-56 | Evaluation | Lokale Messwerte und Befragung zu Verständnis, Last und Scopeabweichungen; keine erfundenen Produktivitätswerte |

## Weitere verbindliche Prüfungen

- Kapitel 9 A–F: je ein vollständiger Nutzerablauf im echten Host.
- Kapitel 14.4: echter Editor, lebender Plan, Ausschluss einer Zusatzklasse,
  gebundener Draft, abgelehnte Scopeverletzung, Diff/Apply/Tests,
  menschliche Edits, Entscheidungsevidenz.
- Kapitel 14.5: unterstützter Einstieg, Parameter/Stubs, Werteherkunft,
  Unknown-Stopp, mindestens ein Vergleich gegen echte Referenzausführung,
  Simulation eindeutig von realer Verifikation getrennt.
- Kapitel 13: Modell-/Datenfluss und Log-Policy sichtbar; Tastatur und Fokus,
  Screenreader und Kontrast; reaktive Navigation und gemessene lokale Latenz;
  keine Scopeumgehung durch Shell; explizite Berechtigung für reale Prüfungen;
  natives Undo und Konflikterkennung.

## Aktueller Arbeitsstand

Der Entwicklungsstand 0.6 besitzt Read-only-Erkundung, einen lebenden Plan mit
strukturierten Schrittdetails, Entscheidungen und Historien, revisionsgebundene
Contracts, regionsbegrenzte Patches, laufende und bearbeitbare native Diffs,
direkte menschliche Edits samt lesender Begutachtung, Softwarekarte,
Subset-Simulation, begrenzte Delegation, Übergabe und lokale Pilotevaluation.

### Geprüfte Implementierungswege

| Bereich | Autoritative Nachweise | Grenze des Nachweises |
| --- | --- | --- |
| F-01–08, F-10–13, F-41–43 | `src/collaboration.ts`, Rust-Planwerkzeuge; `scripts/test-v2.cjs`, Host-Slices `repository`, `guidance`, `cooperation-v2` | Offline-Doppelgänger prüfen Werkzeuge und Zustände; kein allgemeiner Beweis der Qualität beliebiger Modellantworten |
| F-14–18, F-44–49, F-51 | Native Editor-/Diff-/Apply-Wege, `src/contract.ts`, `tandem_app/tests/contracts.rs`, `scope_escalation.rs`; Host-Slices `flow`, `interruption`, `contract-host`, `cooperation-v2` | Harte Datei-/Regiongrenze; ergänzende C#-Strukturprüfung ist kein vollständiger semantischer API-Beweis |
| F-19–24 | `src/code-map.ts`, `src/software-graph.ts`, `src/graph-view.ts`, `media/graph.js`; `scripts/test-graph.cjs`, Host-Slices `code-map-host`, `graph-host` | Sprachdienst-Ausschnitt und ausdrücklich importierte Beziehungen; keine automatisch vollständige Architektur oder bewiesene Verhaltensauswirkung |
| F-25–34, F-37 | `src/simulation.ts`, Debugger-Panel; `scripts/test-v2.cjs`, `test-simulation-loops.cjs`, `test-simulation-ui.cjs`, Host-Slices `simulation-host`, `simulation-loops-host`, `simulation-invalidation-host` | C#-Subset; vier bisherige Szenarien sowie 30 Schleifenfälle mit Zuständen, Zweigen und Rückgaben gegen reale .NET-Referenz geprüft. Unbekanntes und Budgetüberschreitung stoppen. Neue Traces binden Interpreter v2; direkte menschliche Edits invalidieren auch ohne Repository-Plan |
| F-06, F-10, F-41 | `src/collaboration.ts`, Erkenntnisformular; `scripts/test-decisions.cjs`, `test-decision-ui.cjs` | Aktuelle Erkenntnis einschließlich mehrzeiliger Grenzen/Abnahme ersetzbar; alter Eintrag bleibt historisch. Kein erneutes Ersetzen eines bereits abgelösten Eintrags |
| F-09, F-38, F-47, F-52–54 | Werkzeugprofile, explicit Delegate-Policy, Host-Slice `delegation-host`, Scope-Eskalationstest | Ein begrenzter Schritt; Prüfprogramm läuft mit ausdrücklich gewählten Benutzerrechten, nicht in einer Betriebssystem-Sandbox |
| F-36, F-55 | `workspace_snapshot`, `src/verification.ts`; Host-Slices `verification`, `verification-workspace-host` | Vollständige Inhalte des erlaubten Suchraums bis 1 GB, Bindung vor/nach realem Prozess sowie an Plan/Contract. Ausgeschlossene Dateien und externe Effekte nicht erfasst |
| F-39, F-56 | Aufgabenarchive, `src/artifact.ts`, Host-Slice `cooperation-v2`, reine Artefakt-/Evaluationsprüfungen | Versionierter lokaler Übergabevertrag und messbare Ereignisse/Selbsteinschätzung; keine abgeschlossene Nutzerstudie |
| F-40 | `tandem_app/src/factory.rs`, CLI `--consume-handoff`, RPC `factory`, `src/factory.ts`, Host-Slices `factory-host`, `factory-interruption-host`, `scripts/test-factory.cjs`, `test-factory-ui.cjs` | Tatsächlich ausführbarer eigenständiger Worker-Consumer und Ergebnisimport mit lokaler Revalidierung. Kein angeschlossener produktiver externer Orchestrierungsdienst; kein Worker-Schreibzugriff oder Testbeleg |
| Kapitel 13, Interaktion | Browserprüfungen für 320–1320 px, Tastatur-Navigation, sichere DOM-Erzeugung, Herkunftslegende mit mindestens 4,5:1 Textkontrast im hellen Thema | Noch keine umfassende manuelle Screenreader- und Entwickler-Usability-Abnahme über alle Views |

### Weiterhin offene Gesamt-Abnahme

- F-35 bleibt ein langfristiges Produktziel. Die aktuelle Simulation unterstützt
  nicht beliebige C#-Sprachkonstrukte oder breite andere Sprachfamilien. Ein
  Unknown-Stopp ist ehrlich, aber kein Beleg, dass diese Semantik implementiert ist.
- F-40 besitzt nun einen eigenständigen ausführbaren Worker-Consumer und einen
  Rückweg in den normalen Vorschlags-/Diff-Prozess. Die im PDF geforderte spätere
  Delegation über gemeinsame Artefakte ist damit praktisch ausführbar. Die
  Betriebsabnahme eines konkreten externen Factorydienstes wird dadurch nicht
  behauptet; ein bestimmter Dienst ist im PDF nicht vorgeschrieben.
- F-56 und M0 sind als lokale Mess-/Evaluationsfunktionen vorbereitet. Ein echter
  vergleichender Pilot mit Entwicklern und Aussagen zu Netto-Geschwindigkeit,
  Verständnis oder kognitiver Last steht aus.
- Die zusammenhängenden fachlichen Abläufe A–F benötigen eine zusätzliche
  systematische Abnahme über die einzelnen technischen Slices hinaus.
- Eine vollständige PDF-v2-Abnahme ist deshalb ausdrücklich noch nicht erreicht.
  Die Anforderungen werden nicht auf die vorhandenen grünen Tests reduziert.
