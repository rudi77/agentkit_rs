# TANDEM mit AgentKit – gemeinsam entwickeln (0.6)

TANDEM verbindet einen gemeinsam editierbaren Planschritt mit einem begrenzten
Codebereich. Der Agent diskutiert gewünschtes Verhalten und schlägt einen kleinen
Patch vor. Der echte Code bleibt im VS-Code-Editor. Der Vorschlag schreibt noch
keine Datei; Übernahme und reale Prüfung sind getrennte Aktionen. Seit Version 0.2
kann TANDEM das geöffnete Repository vollständig lesend erkunden und daraus einen
mehrteiligen, belegten und gemeinsam bearbeitbaren Änderungsplan vorschlagen.

Der Arbeitsplatz führt durch drei überschaubare Arbeitsbereiche:

1. **Verstehen:** Aufgabe beschreiben und untersuchen lassen.
2. **Planen:** Einen Plan ansehen, besprechen, bearbeiten und den nächsten Schritt wählen.
3. **Zusammen arbeiten:** Offene Fragen klären oder am ausgewählten Code arbeiten;
   anschließend einen Diff prüfen, übernehmen und real verifizieren.

Es ist jeweils nur ein Arbeitsbereich sichtbar. Analyse, Quellen, Gesprächsverlauf
und technische Hinweise lassen sich bei Bedarf aufklappen. Die zusätzliche
Quellen-Baumansicht ist standardmäßig ausgeblendet und über
`tandem.showSourcesView` weiterhin verfügbar. Bestehende Pläne und Dialoge bleiben
beim Update erhalten.

Version 0.4 ergänzt strukturierte Entscheidungen und Schrittdetails, einen
versionierten Änderungsvertrag, bearbeitbare und während des Laufs sichtbare
Vorschläge, eine grafische Softwarekarte, einen begrenzten Gedanken-Debugger,
gezielte Delegation und lokale Übergabe-/Evaluationsartefakte. Der vollständige
v2-Abnahmeplan und die noch offenen Nachweise stehen in
der lokalen Datei `REQUIREMENTS-v2.md`.

Version 0.6 erweitert den Gedanken-Debugger um `while`, `for`, `do/while`,
`break`, `continue` und lokale Zähler-/Verbundzuweisungen. Neue Simulationen
verwenden Interpreter v2; historische Szenarien ohne Versionsangabe behalten
bei direktem Replay mit `simulate` die frühere v1-Semantik. Beide Versionen
begrenzen Traces auf 500 Schritte und stoppen bei unbekannter Semantik. Die
Oberfläche zeigt die Interpreter-Version an. Die Schleifensemantik wird in 30
Szenarien bei Zuständen, Bedingungen und Rückgaben gegen reale C#-Ausführung
geprüft. Eine gewünschte Startzeile wählt eine erreichte Trace-Position; ihre
Vorbedingungen werden ab Methodenbeginn berechnet.

Im Bereich **Erkenntnis verbindlich festhalten** kannst du jetzt eine aktuelle
Erkenntnis auswählen und ersetzen. Die bisherige Festlegung bleibt als historisch
sichtbar; die neue gilt für den weiteren Dialog und Contract. Das funktioniert
auch mit mehrzeiligen Grenzen und Abnahmekriterien. Eine bereits ersetzte
Erkenntnis kann nicht erneut abgelöst werden; wähle dafür ihren aktuellen Nachfolger.

Version 0.5 ergänzt einen ausführbaren Worker-Übergabeweg. Unter **Zusammen
arbeiten → Dein Arbeitsbereich → An einen Worker übergeben** bereitest du den
gespeicherten Code samt Plan und Contract als Job vor. **Mit AgentKit bearbeiten**
startet den lokalen Worker mit deinem konfigurierten Modellanbieter. Danach prüft
**Ergebnis als Vorschlag prüfen** die aktuelle Datei, Job-Identität, Planrevision,
Contract und erlaubte Änderung. Erst die normale Diff-Übernahme ändert Produktcode.
Ein Worker-Ergebnis erzeugt keinen bestandenen Testbeleg. Stop oder menschliche
Code-/Planänderungen sperren verspätete Ergebnisse.

Für einen eigenständigen Worker öffnest du die Jobdatei und verwendest das in der
Oberfläche angezeigte Programm mit seinen Argumenten:

```powershell
& 'D:\Pfad\tandem.exe' --consume-handoff 'D:\Pfad\job.json' --provider auto --workspace 'D:\Pfad\Checkout'
```

Der Prozess gibt genau ein `tandem.factory-result/v1`-JSON auf stdout aus; Fehler
gehen nach stderr mit Exit-Code 1. Speichere das Ergebnis als UTF-8-JSON und wähle
**Externes Ergebnis einlesen**. Die Jobdatei muss bytegleich bleiben, da ihr
SHA-256 das Ergebnis bindet. `--workspace` prüft zusätzlich die gespeicherten
Dateibytes im ausdrücklich angegebenen Checkout; ohne diese Option arbeitet der
Worker auf dem eingebetteten Snapshot. Die Extension prüft beim Import immer
ihren eigenen aktuellen Checkout. Job und Ergebnis enthalten Code und müssen
beim externen Weitergeben entsprechend behandelt werden. Der lokale
Metadatenexport lässt auch Worker-Snapshots und Worker-Antworten aus.

## Starten

Voraussetzungen für den Quellcode-Build: Rust/Cargo, Node.js ab Version 22 und
VS Code ab 1.96. Die Extension läuft als Desktop-/Workspace-Extension unter
Windows, Linux und macOS; die Binary wird jeweils für den aktuellen Host gebaut.
Eine erzeugte VSIX enthält diese plattformspezifische Binary.

Im Verzeichnis `tandem_vscode`:

```powershell
npm ci
npm run build:backend
npm run compile
```

Dieses Verzeichnis in VS Code öffnen und **F5 → TANDEM entwickeln** starten.
Der Entwicklungs-Host öffnet das mitgelieferte Demo-Projekt. Alternativ erzeugt
`npm run package` eine installierbare VSIX. In VS Code über **Extensions: Install
from VSIX** installieren und einen Projektordner öffnen. Ein vertrauenswürdiger
Workspace ist erforderlich.

Die vorhandene lokale AgentKit-Konfiguration wird automatisch verwendet:
`~/.agentkit/config.json`, unter Windows `%USERPROFILE%\.agentkit\config.json`.
`AGENTKIT_HOME` und bestehende Provider-Umgebungsvariablen werden berücksichtigt;
Umgebungsvariablen haben Vorrang. Mit `provider: "azure"` in dieser Datei werden
dieselben Azure-GPT-Zugangsdaten und dasselbe Deployment verwendet wie bei
AgentKit. Die Extension benötigt keinen eigenen API-Key. Projekt-`.env`-Dateien
werden von diesem Adapter nicht automatisch geladen.

`tandem.provider` steht auf `auto`. Ohne Modellkonfiguration erscheint ein Fehler;
es gibt keinen stillen Wechsel auf die Demo. Die Einstellung und ein optionaler
`tandem.binaryPath` gelten für den Rechner, nicht als ausführbarer Projektinhalt.

## Repository-Aufgabe untersuchen

1. Den Projektordner öffnen, beispielsweise den Root von bluStack. Über die
   Befehlspalette **TANDEM: Repository-Aufgabe untersuchen** aufrufen.
2. Unter **Verstehen → Deine Aufgabe** den Fehler, das gewünschte Verhalten und die
   Grenzen beschreiben. **Aufgabe untersuchen** startet den AgentKit-Agenten.
   Eine vorherige Dateiauswahl ist nicht erforderlich. Ticketlinks und ADO-IDs
   werden nicht automatisch importiert; die relevante Beschreibung muss als Text
   eingefügt werden.
3. TANDEM sucht Implementierungen und vorhandene Tests und wechselt zu **Planen**.
   Unter **Quellen und Projektregeln** lassen sich die belegten Stellen öffnen. Jeder Beleg enthält
   Datei, Zeilenbereich, SHA-256 und die Herkunft aus Datei oder ungespeichertem
   Editorstand. Ein gefundener Test ist ausdrücklich noch kein ausgeführter Test.
4. Den vorgeschlagenen Änderungsplan prüfen und mit **Diesen Plan verwenden** als
   gemeinsames Artefakt übernehmen. **Plan bearbeiten** öffnet Ziel, Nicht-Ziele,
   offene Fragen und Schritt-Details. Schritte können ergänzt, entfernt oder nach
   oben verschoben werden; Änderungen mit **Änderungen speichern** festhalten.
5. Einen Schritt auswählen und **An diesem Schritt arbeiten** anklicken. Das übernimmt dessen Ziel und Akzeptanzkriterium
   in den bisherigen Code-Schritt. Das erteilt keine Schreibrechte: für einen
   Patch muss weiterhin ein konkreter Codebereich ausgewählt werden.

Bei einem Klärungsschritt kannst du unmittelbar unter **Dein Beitrag zu diesem
Schritt** eine Entscheidung oder Rückfrage eingeben und **Besprechen** wählen.
Dafür ist keine Codeauswahl nötig. TANDEM berücksichtigt die ursprüngliche Aufgabe
und den bearbeiteten Plan und schlägt bei Bedarf einen aktualisierten Plan vor.
Der menschliche Plan wird nicht automatisch ersetzt. Bei einem neuen Vorschlag
kannst du auch **Meinen Plan behalten** wählen.

Für einen Code-Schritt **Am Code arbeiten** öffnen, eine passende Datei anklicken,
den erlaubten Bereich im Editor markieren und **Markierten Code verwenden** wählen.
Anschließend führt **Kleine Codeänderung vorschlagen** zum Patch. **Diff im Editor
prüfen**, **Änderung übernehmen** und **Reale Prüfung ausführen** erscheinen im
jeweils passenden Zustand.

Die Erkundung verfügt nur über `list_files`, `search_code`, `read_file`,
`find_references`, `read_project_instructions` und `propose_change_plan`.
`find_references` findet textuelle Symbolvorkommen, keinen bewiesenen semantischen
Aufrufgraph. CodeGraph oder ein Sprachserver werden von diesem Agenten nicht
gestartet. Der vorhandene Sprachdienst kann weiterhin den fokussierten Editor-
Bereich und dessen Referenzen für die bisherige Codeansicht liefern.

`AGENTS.md` und `CLAUDE.md` werden am Root und entlang gelesener Dateipfade geladen.
Explizit verlinkte `AGENTS.md`, `CLAUDE.md`, `CLAUDE_BEHAVIORAL.md` und
`CODING_GUIDELINES.md` innerhalb des Repositorys werden verfolgt. Die Regeltexte
gehen als Projektvorgaben an das Modell. Beliebige prosebasierte Fachregeln sind
damit noch kein formaler Policy-Beweis. Die Lesewerkzeuge selbst erzwingen
Repository-Grenze, Pfadsperren und Budgets unabhängig vom Modell.

Gitignore und eine optionale **`.tandemignore` im Projektroot** begrenzen den
Suchraum. `.tandemignore` verwendet Gitignore-Muster. Zusätzlich sind unter anderem
`.env*`, `appsettings*.json`, Geheimnis-/Credential-Dateinamen, private Schlüssel,
Logs, Datenbanken, PDFs, `.git`, `.codegraph` und Build-Verzeichnisse gesperrt.
Diese festen Sperren können nicht mit einer Negationsregel aufgehoben werden.
Auch direkte Tool-Anfragen und offene, ungespeicherte Geheimnisdateien bleiben
gesperrt. Symlinks und Windows-Verzeichnisverknüpfungen werden nicht verfolgt.
Das ist ein definierter Datei-/Pfadschutz, kein vollständiger Scanner für
versehentlich in gewöhnlichem Quellcode abgelegte Geheimnisse oder Personendaten.

Eine Erkundung ist auf einen geöffneten Projektordner, 20.000 lesbare Dateien,
UTF-8-Dateien unter 512 KB, 240 Zeilen pro Leseaufruf und ein begrenztes Ausgabebudget
beschränkt. Suchergebnisse melden abgeschnittene Ergebnisse und übersprungene
Dateien. Bei mehreren Workspace-Ordnern wird der betreffende Ordner ausgewählt.

Vor einer Planübernahme werden Dateikatalog und Quell-Hashes erneut geprüft.
Editoränderungen, Dateiänderungen und ein Host-Neustart markieren alte Analysen
und Pläne als veraltet. Der menschlich bearbeitete Plan bleibt dabei erhalten;
ein neuer Agentenvorschlag wird nicht automatisch darübergeschrieben.
**Quellenstand prüfen** validiert den bestehenden Stand ohne Modellaufruf.
Nur wenn die Quellen verändert wurden, ist **Analyse aktualisieren** erforderlich.

## Einen Code-Schritt gemeinsam bearbeiten

1. Einen erlaubten Codebereich markieren. Ohne Markierung versucht TANDEM,
   über den installierten Sprachdienst das umgebende Symbol zu bestimmen.
2. **TANDEM: Auswahl oder Methode als Schritt verwenden** ausführen.
3. Unter **Ziel und Grenzen dieses Schritts anpassen** die Vorgaben bei Bedarf bearbeiten.
4. Mit **Besprechen** Fragen klären oder mit **Kleine Codeänderung vorschlagen** einen Patch anfordern.
5. **Diff im Editor prüfen** öffnet den nativen Diff-Editor. Anschließend übernehmen oder
   verwerfen. Die Übernahme ist ein normaler Editor-Edit mit Undo und bleibt
   zunächst ungespeichert.
6. Für eine reale Prüfung `tandem.verificationProgram` und
   `tandem.verificationArgs` festlegen, etwa `dotnet` und
   `["test", "tests/Example.Tests.csproj"]`. **Reale Prüfung ausführen** speichert die
   aktuelle Datei und startet das Programm ohne Shell-Auswertung im Projektordner.

Menschliche Edits und Contract-Änderungen machen einen offenen Vorschlag ungültig.
Eigene Edits lassen sich zuerst mit **Meine Änderungen besprechen** lesend
begutachten. Für einen neuen Patch danach den erlaubten Bereich erneut wählen.
Planvorschläge des Agents können separat übernommen werden.
Ein gestoppter Lauf kann keinen verspäteten Patch mehr in die Oberfläche eintragen.

Plan, Repository-Analyse, Quellen, Dialog, Vorschlag, Prüfstatus und Entscheidungsverlauf liegen als
`step.json` im Workspace-Speicher der Extension unter dem VS-Code-Benutzerdaten-
verzeichnis. Die Datei enthält Code-Snapshots. Nach einem Host-Neustart werden
offene Vorschläge und alte Prüfergebnisse als veraltet gekennzeichnet.

## Softwarekarte und Arbeitsplatz

**Softwarekarte** oder **TANDEM: Grafische Softwarekarte öffnen** öffnet einen
zusätzlichen Editorbereich neben dem echten Code. Vier Sichten bleiben verbunden:

- **Produkt & Plan:** gemeinsames Ziel, Schritte und verknüpfte Dateien.
- **Architektur:** importierte Beziehungen oder ein ausdrücklich unvollständiger
  Ausschnitt gelesener Dateien. Dateigruppierung ist keine Aufrufanalyse.
- **Code-Nachbarn:** Aufrufer, aufgerufene Methoden und Referenzen aus dem
  verfügbaren Sprachdienst. Testkandidaten anhand von Dateinamen sind kein
  Abdeckungsnachweis.
- **Patchauswirkung:** tatsächlicher Änderungsumfang und mögliche Nachbarn,
  sofern die im Patchbereich liegende Methode fokussiert wurde.

Knoten öffnen Code in dessen bisheriger Spalte. Herkunft, Quellanker und
Unsicherheiten stehen im Inspektor. Durchgezogene Beziehungen aus dem
Sprachdienst, dokumentierte Beziehungen, gestrichelte Hypothesen und bloße
Dateigruppierung sind beschriftet. Die Karte folgt auf Wunsch dem Codefokus;
Suche, Zoom, direkter Nachbarausschnitt und eine tastaturbedienbare Listenansicht
begrenzen die Informationsmenge. Ohne Sprachdienst bleiben fehlende Daten sichtbar.

Im Explorer bleibt zusätzlich der native Baum **TANDEM · Softwarekarte** verfügbar.
Über **Layout** kann der Plan mit dem nativen VS-Code-Dialog in die rechte
Seitenleiste verschoben werden. Code, Baum und Plan lassen sich so gleichzeitig
anzeigen; der Gedanken-Debugger liegt im unteren Panel.

Eine optionale, ausdrücklich importierte `.tandem/architecture.json` benennt echte
Codeanker und die Herkunft ihrer Beziehungen:

```json
{
  "nodes": [
    {"id": "entry", "label": "Prediction", "path": "src/Prediction.cs", "line": 12},
    {"id": "port", "label": "Plan-Port", "path": "src/IPlanPort.cs", "line": 4}
  ],
  "edges": [
    {"from": "entry", "to": "port", "kind": "documented", "reason": "Bestätigter Lesevertrag"}
  ]
}
```

**TANDEM: Architektur aus .tandem/architecture.json importieren** prüft relative
Pfade, vorhandene Dateien und Zeilen. `hypothesis` markiert eine unbelegte Beziehung.
Dateiänderungen und Reload machen den Import sichtbar veraltet. Importierte
Dokumentation wird dadurch nicht zu einer symbolisch bewiesenen Beziehung.

## Gedanken-Debugger

Eine C#-Methode als Arbeitsbereich wählen und **Verhalten im Gedanken-Debugger
untersuchen** öffnen. Parameter werden erkannt; einfache Werte können direkt
bearbeitet werden. Objektwerte und Stubs sind explizite JSON-Szenariowerte. Ein
Stub benötigt einen Rückgabewert und einen Grund, etwa:

```json
{"_plans.GetPublished(12)": {"value": {"Version": 12}, "reason": "Gewählter veröffentlichter Teststand"}}
```

Der lokale Interpreter unterstützt ein ausgewiesenes Subset: ganze `int`-Werte,
`bool`, Strings, lesende Szenarioobjekte, lokale Zuweisungen, if/else, Returns,
modellierte Exceptions und eindeutig auflösbare lokale Methoden mit Blockrumpf.
Externe Aufrufe benötigen dokumentierte Stubs. Schleifen, Konstruktorsemantik,
Reflection, dynamische Bindung, allgemeine Asynchronität und Nebenläufigkeit
werden nicht erfunden; der Trace endet mit **Unbekannt**. Nicht modellierte
Überläufe liefern ebenfalls keinen behaupteten C#-Wert.

Vorwärts, zurück, Step Into/Over/Out und Weiter bis Zeile navigieren einen
reproduzierbaren Trace. Quellzeile, Call Stack, Werte, Herkunft und Zweigentscheidung
bleiben sichtbar. **Mit Patch vergleichen** verwendet dieselben Eingaben gegen
Baseline und nicht übernommenen Vorschlag. Szenario-, Code- und Planversion sind
gebunden; Änderungen machen alte Traces ungültig. Eine Beobachtung kann als
Testidee in den aktuellen Schritt übernommen werden. Das ist noch kein realer Test.

## Gezielte Delegation

Im Änderungsvertrag ausdrücklich **Delegate** wählen. Ein klarer risikoarmer
Schritt benötigt ein konkretes Abnahmekriterium, geschützte öffentliche
Signaturen und keine neuen Typen/Methoden. Das Zeilenbudget zählt entfernte plus
neue Zeilen. Die Übernahme- und Prüfberechtigung wird separat vor dem Start gewählt.

Ohne automatische Übernahme endet der Lauf bei einem geprüften Vorschlag.
Mit automatischer Übernahme wird nur der aktuelle Contract angewendet und das
zum Start festgelegte Prüfprogramm ausgeführt. Scope-Konflikte, geänderte
Voraussetzungen, überschrittenes Budget und fehlgeschlagene oder veraltete
Prüfungen führen zurück zur gemeinsamen Entscheidung. Ein erfolgreicher Lauf
markiert reale Verifikation, keine automatische fachliche Abnahme.

## Entscheidungen, Aufgaben und Übergabe

Bedarf, Nicht-Ziele, Annahmen, Risiken und Testideen lassen sich mit Urheber,
Begründung und Schrittbezug festhalten. Änderungen invalidieren widersprechende
Vorschläge und Belege. **Neue Aufgabe** archiviert den bisherigen lokalen Ablauf;
**Gespeicherte Aufgabe fortsetzen** erhält Entscheidungen und Historien und
verlangt eine neue Prüfung alter Codebindungen.

**Versionierte Übergabe öffnen** erzeugt lokal `tandem.handoff/v1`. Standardmäßig
werden Metadaten exportiert; Code-Snapshots und Dialog sind eine ausdrückliche
Option. Der Export enthält keine Ausführungsberechtigung. Ein späterer Factory-
Empfänger muss Baseline und eigene Rechte prüfen. Ein externer Factorybetrieb
ist damit noch nicht angebunden.

Eine optionale Piloteinschätzung erfasst kognitive Last, Verständnis,
Unterbrechungskosten und eine eigene Erklärung. Gemessene lokale Ereignisse und
subjektive Einschätzungen bleiben getrennt. Daraus wird kein behaupteter
Produktivitätsgewinn abgeleitet.

## Offline-Demo

In den **Benutzereinstellungen** `tandem.provider` auf `demo` setzen und den Ordner
`demo` als Projekt öffnen. In `Normalize.cs` die Methode mit `return input;`
markieren, als Arbeitsbereich wählen und eine kleine Codeänderung anfordern.
Die geskriptete Demo verwendet den echten AgentKit-Tool-Loop und schlägt
`return input.Trim();` vor. Sie ist keine allgemeine KI und benötigt keinen Key.

Für die reale Demo-Prüfung:

```json
{
  "tandem.verificationProgram": "node",
  "tandem.verificationArgs": ["verify.cjs"]
}
```

Das Demo-Prüfprogramm ist ausdrücklich ein **Quelltextcheck**, kein ausgeführter
C#-Verhaltenstest. Es schlägt vor dem Patch fehl und nach dem gespeicherten Patch
erfolgreich an. Ein Exit-Code 0 belegt nur das konfigurierte Prüfprogramm.

## Nachweise und Grenzen

```powershell
cargo test --manifest-path ../tandem_app/Cargo.toml
npm test
npm run test:ui
npm run test:v2
npm run test:azure
```

Die Rust-Tests prüfen Scope-Verstöße, mehrdeutige Ersetzungen, unveränderten
Umgebungscode, Explore-Rechte, Abbruch und den echten AgentKit-Loop mit FakeLlm.
Die Extension-Tests laufen in 15 echten VS-Code-Hosts mit eigenen Testprojekten
und isolierten Einstellungen. Sie prüfen native Diffs, veraltete Contracts,
menschliche Edits einschließlich Unicode, kontrolliertes Apply, Undo, echte
Prüfprozesse, Wiederladen, den JavaScript-Handshake der echten Webview und
Unterbrechungen während eines laufenden Modellaufrufs. Ein eigener Prüf-Slice
deckt BOM-Dateien, doppelte Prüfstarts und Quelltext-Nebenwirkungen ab. Die
Repository-Slices prüfen das Laden der Regeln, Quellen und Editorstände,
Geheimnissperren, gemeinsame Planbearbeitung, Navigation, Invalidierung und
Wiederladen. Alle
Modellaufrufe dieser Tests sind offline. `test:azure` prüft zusätzlich die globale
Konfiguration und Azure-SSE gegen einen lokalen HTTP-Doppelgänger; dafür muss die
Binary zuvor mit `npm run build:backend` gebaut sein.
`TANDEM_VSCODE_EXECUTABLE` kann eine vorhandene VS-Code-Executable auswählen;
unter Windows wird die übliche Benutzerinstallation verwendet. Andernfalls lädt
der Testrunner VS Code herunter.

Der zusätzliche Browsertest lädt dieselben HTML-, CSS- und JavaScript-Dateien wie
die Webview. Er prüft den geführten Ablauf, Rückfragen, erhaltene Eingaben,
Markdown ohne ausführbares HTML und Breiten von 320 bis 1250 Pixeln. Screenshots
liegen unter `test-results/ui`. Playwright nutzt unter Windows eine vorhandene
Chrome-Installation; andernfalls bei Bedarf `npx playwright install chromium`
ausführen oder mit `TANDEM_CHROMIUM` eine Browser-Executable angeben. Der Test
verwendet ausschließlich einen lokalen UI-Doppelgänger, keine Modellaufrufe.

Die Patch-Funktion unterstützt weiterhin **einen aktiven Code-Schritt, eine Datei
und einen erlaubten Textbereich**. Der gemeinsame Änderungsplan kann mehrere
Schritte und Dateien beschreiben; er führt sie noch nicht automatisch aus.
Die Software-Karte zeigt den Fokus und bis zu 20 vom Sprachdienst
gelieferte Referenzen. Eine vollständige Architektur- oder Auswirkungsanalyse ist
damit nicht verbunden. Ohne Sprachdienst bleibt die manuelle Bereichsauswahl nutzbar.

Bei einem Code-Schritt bekommt der Rust-Adapter den Snapshot der ausgewählten Datei
und den kurzen Dialog. Bei der Repository-Erkundung liest er die freigegebenen
Projektregeln und aufgabenbezogene Code-/Testausschnitte selbstständig. Diese Daten
gehen an den konfigurierten Modellanbieter.
Er hat keine Shell-, MCP-, Datei-Schreib- oder sonstigen Coding-Tools. Seine
Patch-Vorschläge werden zusätzlich in der Extension geprüft. Der Contract schützt
Datei und Textbereich; fachliche Nicht-Ziele sind noch keine semantisch bewiesenen
Invarianten und benötigen menschliche Prüfung und geeignete Tests.

Das bewusst gestartete Prüfprogramm ist ein eigener Ausführungspfad und läuft mit
den Benutzerrechten, **ohne Betriebssystem-Sandbox**. Andere Erweiterungen und
externe Programme werden von TANDEM nicht gesperrt. Der Prüfbeleg bindet die ausgewählte Datei, Plan-/Contract-Version und die
Inhalte des gesamten erlaubten Repository-Suchraums vor und nach der Ausführung.
Auch Änderungen anderer erlaubter Dateien, neue Dateien und gelöschte Dateien
invalidieren den Beleg. Gitignore, .tandemignore, Pfad-/Link-Sperren sowie ein
1-GB-Inhaltsbudget begrenzen die Abdeckung. Unvollständige Snapshots werden nicht
als bestandene Prüfung ausgegeben. Ausgeschlossene Dateien und externe
Seiteneffekte bleiben außerhalb dieses Nachweises. Andere ungespeicherte
Quelldateien müssen bewusst vom Entwickler gespeichert werden.

Der Gedanken-Debugger ist ein lokaler Subset-Interpreter, kein vollständiger
Roslyn-/CLR-Interpreter. Breitere Sprachabdeckung, mehrere gleichzeitig
ausführende Schritte, dateiübergreifende Patches und eine produktive Factory-
Orchestrierung über den eigenständigen Worker-Vertrag hinaus bleiben Ausbaustufen. `test:v2` vergleicht Werte, Zweige und
Aufrufreihenfolge mit tatsächlich ausgeführten lokalen C#-Referenzen und benötigt
dafür das .NET-10-SDK; die Extension selbst benötigt dieses SDK nicht.
Der Prototyp ist kein Port eines bestehenden TANDEM-Programms; AgentKit wird über
seine unveränderte öffentliche Bibliotheks-API eingebunden.

## Lesende Abnahme außerhalb des Editors

Nach `npm run build:backend` kann derselbe Erkundungs-Pfad auch explizit als
JSON-RPC-Smoke-Test gestartet werden:

```powershell
node scripts/explore-repository.cjs --workspace D:\Pfad\zum\Repo --task "Fehlerbeschreibung und gewünschtes Verhalten; nur untersuchen" --output test-results/analysis.json
```

Dabei wird das gewählte Repository nur gelesen. Das Ergebnis enthält Quellen,
Regelpfade, Änderungsplan und offene Fragen. Nur die angegebene Ausgabedatei wird
geschrieben. Ohne `--provider demo` verwendet dieser manuelle Nachweis das echte
konfigurierte Modell und verursacht dessen normale Modellaufrufe. Die explizite
Offline-Demo zeigt lediglich Dateisuche und Regelzugriff; sie erzeugt keinen
allgemeinen aufgabenspezifischen Repository-Plan.
