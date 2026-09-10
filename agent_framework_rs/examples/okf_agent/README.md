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
