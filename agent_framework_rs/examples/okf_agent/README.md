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

## Den Auftrag schreiben

Der Prompt sagt dem Agenten, *wie* er arbeitet. Der **Auftrag** sagt ihm, *woran*. Für einen
Wiki-Import gibt es dafür eine Vorlage, gefüllt mit dem, was bei uns tatsächlich schiefging:

```powershell
copy prompts\10_import.md D:\tmp\auftrag.md
notepad D:\tmp\auftrag.md          # Platzhalter <…> ersetzen
.\Invoke-Okf.ps1 … -TaskFile D:\tmp\auftrag.md
```

Fünf Blöcke, jeder mit einem Grund:

| Block | wozu |
|---|---|
| **Ausgangslage** | sagt, ob es ein Merge oder ein Neuaufbau ist, und nennt die aktuellen Gate-Zahlen als Maßstab. Ohne das behandelt der Agent ein volles Bündel wie ein leeres. |
| **Vorgehen** | schickt ihn in den Skill (`list_skills` → `read_skill`), statt ihm den Import selbst zu erklären. Der Skill IST die Methode. |
| **Quelle** | Subtree plus die beiden deterministischen Script-Aufrufe, jeweils in **neue** Scratch-Verzeichnisse. `--force` ist verboten: es hat schon einmal den Export gelöscht, den es gleich lesen wollte. |
| **Harte Regeln** | kein schreibendes git, nichts Bestehendes löschen, fremde Repos nur lesen, Zwischenstände nie ins Bündel. |
| **Abschluss** | beide Gates **wörtlich** zeigen, dazu Quellenbilanz und die Liste angefasster Dateien. |

Zu ersetzen sind `<BESTAND>`, `<SUBTREE>`, `<WIKI-CLONE>`, `<N>`, `<SCRATCH>`, `<NAME>`,
`<SKILLS-REPO>` und `<AUSGESCHLOSSEN>`. `<SKILL_DIR>` bleibt stehen — das setzt agentkit beim
`read_skill` selbst ein. Bleibt versehentlich ein Platzhalter stehen, bricht der Agent ab und
nennt ihn; die Vorlage weist ihn in ihrer zweiten Zeile ausdrücklich dazu an.

**Warum der Abschlussblock nicht optional ist:** Beide Gates prüfen Form, nicht Abdeckung. In
einem unserer Läufe waren sie grün, während nur 5 von 45 Quellseiten im Bündel gelandet waren.
Die Quellenbilanz ist das Einzige, was das sichtbar macht.

### Der zweite Aufruf

Bei einem leeren Bündel legt der `wiki-import`-Skill seinen Split-/Merge-Plan vor und beendet
seinen Zug — *„an import is a bulk change to a knowledge base; the user gets to see its shape
first"*. In einem One-shot gibt es niemanden, der freigibt. Deshalb `-Session`: der zweite
Aufruf läuft im selben Gespräch weiter, und die Freigabe ist einfach der nächste Auftrag.

```powershell
.\Invoke-Okf.ps1 … -Session D:\tmp\s.json -TaskFile D:\tmp\auftrag.md      # Plan
.\Invoke-Okf.ps1 … -Session D:\tmp\s.json -TaskFile prompts\20_freigabe.md # Ausführung
```

Beim Merge in ein volles Bündel hat er dagegen ohne Halt durchgearbeitet — verlass dich nicht
darauf, dass immer angehalten wird.

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
