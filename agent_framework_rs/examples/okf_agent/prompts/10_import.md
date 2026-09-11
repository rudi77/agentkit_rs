Importiere einen Teil eines Azure-DevOps-Wikis in das OKF-Bündel in deinem Arbeitsverzeichnis.

Enthält dieser Auftrag noch Platzhalter in spitzen Klammern (`<…>`), brich sofort ab und sag,
welche. Eine halb ausgefüllte Vorlage darfst du nicht raten.

AUSGANGSLAGE — lies das genau: <BESTAND>. Beide Gates sind auf diesem Stand grün: 0 Fehler,
0 Warnungen. Das ist dein Maßstab — sie müssen hinterher genauso grün sein.

Ist das Bündel nicht leer, ist dies ein MERGE, kein Neuaufbau: lies zuerst die vorhandene
`.okf/index.md` und die Indizes darunter, damit du weißt, was schon da ist, und kein zweites
Concept über dieselbe Sache anlegst.

VORGEHEN: Rufe list_skills auf, lade den Skill "wiki-import" mit read_skill und folge seiner
Anleitung EXAKT, inklusive Step 0.

QUELLE: Subtree <SUBTREE> des Wiki-Clones <WIKI-CLONE> (<N> Seiten).
Stufe 0 — Export erzeugen (Zielverzeichnis muss NEU sein, benutze NIEMALS `--force`):

    uv run "<SKILL_DIR>/scripts/adapters/azure_devops_wiki.py" <WIKI-CLONE> -o <SCRATCH>/export_<NAME> --subtree <SUBTREE> --json

Stufe 1 — vorbereiten:

    uv run "<SKILL_DIR>/scripts/prepare_import.py" <SCRATCH>/export_<NAME> -o <SCRATCH>/prepared_<NAME> --json

`<SKILL_DIR>` hat read_skill dir im Skill-Text bereits eingesetzt — das ist der einzige
Platzhalter, den du NICHT selbst ersetzen musst.

HARTE REGELN:
1. Das Arbeitsverzeichnis ist ein echtes Git-Repository mit gepushter Historie. Führe KEIN
   veränderndes git-Kommando aus: kein commit, add, push, checkout, reset, restore, clean.
   Lesen (status, log, diff) ist erlaubt.
2. Lösche und verändere KEINES der vorhandenen Concepts. Verlangt der Merge doch eine Änderung
   an einer bestehenden Datei (etwa einen Index-Eintrag oder einen Querverweis), ist das
   erlaubt — aber nenne jede solche Änderung am Ende einzeln mit Grund.
3. <SKILLS-REPO> ist NUR LESBAR, ebenso der Wiki-Clone. Dort nichts schreiben, auch nicht per
   run_shell.
4. Alle Zwischenstände nach <SCRATCH>, niemals ins Arbeitsverzeichnis — sie enthalten
   unbereinigte Quelldaten.
5. Ausser Scope: <AUSGESCHLOSSEN>.

ABSCHLUSS — beide Gates ausführen und deren Ausgabe WÖRTLICH zeigen:

    uv run "<SKILL_DIR>/../validate/scripts/okf_validate.py" .okf --strict
    uv run "<SKILL_DIR>/../blumatix/scripts/blumatix_validate.py" .okf

Dazu die Quellenbilanz (wie viele der Quellseiten stecken in einem Concept, welche sind
ausgelassen und warum), die Verteilung der NEUEN Concepts über die Verzeichnisse, und die
Liste der bestehenden Dateien, die du angefasst hast.
