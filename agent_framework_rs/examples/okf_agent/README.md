# okf_agent — agentkit als Wissens-Agent statt als Coding-Agent

Der eingebaute System-Prompt von agentkit ist kein neutraler Agenten-Prompt. Er ist eine
**Methode zum Beheben eines Fehlers in einem Code-Repository**, und zwar eine gemessene:

```
Verschaffe dir zuerst einen Überblick über den vorhandenen Code, bevor du ihn änderst…
1. VERSTEHEN  2. NACHWEISEN (dein Check MUSS jetzt fehlschlagen)  3. ÄNDERN  4. ABSICHERN
Fertig bist du, wenn dein Check den Unterschied zeigt.
```

Für SWE-bench ist jede dieser Regeln belegt (siehe die Doc-Comments in `src/coding.rs`).
Für eine Aufgabe, die **kein Bugfix** ist, sind sie falsch — und sie verschwinden nicht,
nur weil man `--skills` dazugibt. Dann stehen zwei Vorgehensvorschriften im selben Text,
und nichts sagt, welche gilt.

Zwei gemessene Folgen aus einem Wiki-Import mit dem Standard-Prompt:

| Beobachtung | Ursache im Prompt |
|---|---|
| Der Agent baute mitten im Wissens-Import ein `check_missing_links.py`, das „zuerst korrekt fehlgeschlagen" ist, und meldete das als Nachweis | Schritt 2 der Bugfix-Methode |
| Ein anderer Lauf lieferte **0** Concepts und begründete das damit, er dürfe nicht selbst lesen — obwohl niemand seinen Sub-Agenten auf das Quellmaterial angesetzt hatte | „lies dafür NICHT selbst viele Dateien: delegiere die Erkundung" |

## Die Lösung braucht keine Änderung an agentkit

`--system` bzw. `system_file` im Profil **ersetzt** den eingebauten Prompt vollständig. Die
Werkzeug-Erklärungen (Shell-Hinweis, `list_skills`/`read_skill`, Sub-Agenten, Leitplanken)
hängt agentkit trotzdem an — der Agent weiß also weiter, was er kann, bekommt aber keine
Bugfix-Methode mehr vorgeschrieben. Damit ist der geladene Skill die einzige
Vorgehensvorschrift, und genau so ist er gemeint.

- [`okf-agent.md`](okf-agent.md) — der Prompt: fünf Schritte für Wissensarbeit statt für
  Bugfixing. Er kodiert die zwei Fehler, die in den Läufen oben tatsächlich passiert sind:
  *lies das Material, auf das dich dein Auftrag verweist, selbst* — und *ein grünes Gate
  prüft Form, nicht Abdeckung; stelle Quellenzahl gegen Conceptzahl*.
- [`profile.json`](profile.json) — bindet den Prompt ein, dazu `max_steps` und ein
  großzügiges `shell_timeout` (ein Import ruft Scripts auf, die Minuten laufen).
- [`Invoke-Okf.ps1`](Invoke-Okf.ps1) — der Wrapper. Pfade sind Parameter, nichts ist fest
  verdrahtet.
- [`prompts/10_import.md`](prompts/10_import.md) und
  [`prompts/20_freigabe.md`](prompts/20_freigabe.md) — die Auftragsvorlagen (siehe
  [Den Auftrag schreiben](#den-auftrag-schreiben)).

## Benutzen

Du brauchst ein [okf-skills](https://github.com/rudi77/okf-skills)-Checkout und `uv`
(die Skills bringen Python-Scripts mit).

```powershell
.\Invoke-Okf.ps1 -Bundle D:\repos\Wissen -Skills D:\src\okf-skills `
    -ReadAlso D:\repos\Alt.wiki,D:\tmp\import -Yes `
    -Task "Importiere das Wiki unter D:/repos/Alt.wiki mit dem wiki-import Skill."
```

Ohne Wrapper, damit sichtbar ist, was es tut:

```bash
agentkit --profile profile.json \
  -w /pfad/zum/buendel \
  --skills /pfad/zu/okf-skills/skills \
  --agents /pfad/zu/okf-skills/agents \
  --allow-read /pfad/zu/okf-skills \
  --allow-read /pfad/zur/quelle \
  "Dokumentiere X in unserer Wissensbasis."
```

Am Ende meldet der Wrapper Dauer und Exit-Code und **reicht den Exit-Code durch** — ein
Import läuft Minuten bis Stunden, und in einer Pipeline ist ein Wrapper ohne Exit-Code
nutzlos:

```text
OK  Fertig in 00:18:42 - Exit 0
```

agentkits Codes gelten unverändert: `0` ok, `1` Laufzeitfehler, `2` API/Netz,
`3` Kontext/Prompt, `4` `--format` nicht erfüllbar.

Ein Protokoll bekommst du mit `-LogFile`, **nicht** mit `2>&1 | Tee-Object`:

```powershell
.\Invoke-Okf.ps1 … -LogFile D:\tmp\run.log
```

Der Umweg über die Pipeline sieht naheliegend aus, taugt hier aber nicht. agentkit schreibt
Status und Werkzeug-Trace auf stderr (so ist der Unix-Filter-Vertrag gebaut: stdout trägt nur
das Ergebnis). PowerShell macht aus jeder dieser Zeilen einen `ErrorRecord` — der Lauf bricht
dann an der ersten Statuszeile ab, und was im Protokoll landet, steht in UTF-16 mit vier
Zeilen Dekoration je Ausgabezeile. `-LogFile` packt die Zeile aus und schreibt UTF-8.
Nebenbei stellt der Wrapper die Konsole auf UTF-8, sonst wird aus `»` ein `┬╗`.

## Den Auftrag schreiben

Der Prompt sagt dem Agenten, *wie* er arbeitet. Der **Auftrag** sagt ihm, *woran*. Für einen
Wiki-Import gibt es dafür eine Vorlage, gefüllt mit dem, was bei uns tatsächlich schiefging:

```powershell
copy prompts\10_import.md D:\tmp\auftrag.md
notepad D:\tmp\auftrag.md          # Platzhalter <…> ersetzen
.\Invoke-Okf.ps1 … -TaskFile D:\tmp\auftrag.md
```

Die Vorlage ist **einzügig**: ein Aufruf, und am Ende liegen die Concepts im Bündel. Sie
beschreibt dafür sieben Schritte, die der Agent hintereinander abarbeitet — orientieren,
exportieren, vorbereiten, planen, schreiben, prüfen, berichten — und drei Rahmenblöcke:

| Block | wozu |
|---|---|
| **Kein Zwischenstopp** | überstimmt den Freigabe-Halt des Skills und bindet „fertig" an ein Artefakt: neue Dateien im Bündel, beide Gates grün. Steht ganz oben, weil eine spätere Anweisung eine frühere nicht schlägt. |
| **Ausgangslage** | sagt, ob es ein Merge oder ein Neuaufbau ist, und nennt die aktuellen Gate-Zahlen als Maßstab. Ohne das behandelt der Agent ein volles Bündel wie ein leeres. |
| **Struktur** | Gliederung nach Typ, Unterverzeichnisse mit eigener `index.md`, Links immer auf eine Datei. |
| **Vertraulichkeit** | Credential-Scan vor und nach dem Schreiben, und das Verbot, Fundwerte wiederzugeben. |
| **Harte Regeln** | kein schreibendes git, nichts Bestehendes löschen, fremde Repos nur lesen, Zwischenstände nie ins Bündel. `--force` ist verboten: es hat schon einmal den Export gelöscht, den es gleich lesen wollte. |
| **Abschlussbericht** | beide Gates **wörtlich**, dazu Quellenbilanz, Verteilung und die Liste angefasster Dateien. |

Zu ersetzen sind `<BESTAND>`, `<SUBTREE>`, `<WIKI-CLONE>`, `<SCRATCH>`, `<NAME>`,
`<SKILLS-REPO>` und `<AUSGESCHLOSSEN>`. `<SKILL_DIR>` bleibt stehen — das setzt agentkit beim
`read_skill` selbst ein. Bleibt versehentlich ein Platzhalter stehen, bricht der Agent ab und
nennt ihn; die Vorlage weist ihn in ihrer zweiten Zeile ausdrücklich dazu an.

**Warum der Abschlussblock nicht optional ist:** Beide Gates prüfen Form, nicht Abdeckung. In
einem unserer Läufe waren sie grün, während nur 5 von 45 Quellseiten im Bündel gelandet waren.
Die Quellenbilanz ist das Einzige, was das sichtbar macht.

### Warum die Vorlage „kein Zwischenstopp" ganz oben sagt

Der `wiki-import`-Skill verlangt, den Split-/Merge-Plan vorzulegen und den Zug zu beenden —
*„an import is a bulk change to a knowledge base; the user gets to see its shape first"*. Das
ist als Human-in-the-Loop richtig gedacht, aber in einem One-shot wartet er auf jemanden, der
nicht da ist: der Lauf endet mit einer Ankündigung statt mit Dateien.

Die Vorlage erteilt die Freigabe deshalb vorab, und zwar **im ersten Abschnitt**. Weiter unten
würde es nicht wirken: eine spätere Anweisung schlägt eine frühere, konkretere nicht. Dazu ein
überprüfbares Fertig-Kriterium — *„fertig bist du, wenn neue Concept-Dateien im Bündel liegen
und beide Gates grün sind"* —, damit „fertig" an einem Artefakt hängt und nicht am Gefühl des
Modells.

Garantiert ist es trotzdem nicht: der Skill sagt das Gegenteil, und welche Anweisung stärker
wiegt, entscheidet sich zur Laufzeit. Hält der Agent doch an, gibst du mit
[`prompts/20_freigabe.md`](prompts/20_freigabe.md) in **derselben** `-Session` frei — das
kostet eine Runde statt eines Neustarts:

```powershell
.\Invoke-Okf.ps1 … -Session D:\tmp\s.json -TaskFile prompts\20_freigabe.md
```

Willst du den Plan ausdrücklich vorab sehen, streich den Abschnitt „Kein Zwischenstopp" aus
deiner Kopie — dann ist der zweistufige Ablauf wieder der Normalfall.

## Warum `--allow-read` dazugehört

Die Skills verweisen auf **eigene** Referenzdateien (`reference/import-rules.md`,
`../blumatix/reference/profile.md`, `profiles/*.yaml`). Die liegen außerhalb des Bündels
und fielen sonst in die Sandbox. `--allow-read` öffnet sie **nur lesend**; geschrieben wird
weiterhin ausschließlich in `-w`. Das ist enger als die naheliegende Alternative, `-w` über
einen gemeinsamen Elternordner zu ziehen — die würde auch die *Schreib*-Sandbox aufweiten.

## Ein Fallstrick, der Geld kostet

Ein Blumatix-Bündel braucht sein **eigenes** `profiles/`-Verzeichnis. Der Validator nimmt
bewusst *nicht* das der Toolchain, sobald das Bündel außerhalb des okf-skills-Checkouts
liegt: Domains und Owner sind Firmendaten, und ein Tippfehler, der nur zufällig in der
Toolchain existiert, würde still durchgehen. Ohne lokales Profil bricht das Gate mit
`BLU010` ab. Ein neues Repository setzt du deshalb zuerst auf:

```bash
uv run /pfad/zu/okf-skills/skills/blumatix/scripts/blumatix_init.py /pfad/zum/buendel
```

Das legt `profiles/`, die Kind-Verzeichnisse (`products/`, `engineering/`, `operations/`, …),
`index.md` und `log.md` an. Der Agent füllt sie danach — er muss die Struktur nicht erfinden.
