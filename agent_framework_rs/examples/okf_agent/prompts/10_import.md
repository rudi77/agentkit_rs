Importiere einen Teil eines Azure-DevOps-Wikis in das OKF-Bündel in deinem Arbeitsverzeichnis
und führe den Import in DIESEM EINEN ZUG vollständig zu Ende.

Enthält dieser Auftrag noch Platzhalter in spitzen Klammern (`<…>`), brich sofort ab und sag,
welche. Eine halb ausgefüllte Vorlage darfst du nicht raten.

## Kein Zwischenstopp

Der `wiki-import`-Skill verlangt, den Split-/Merge-Plan vorzulegen und auf Freigabe zu warten.
Diese Freigabe ist hiermit **erteilt**. Beende deinen Zug NICHT, um einen Plan vorzulegen, um
Bestätigung zu bitten oder anzukündigen, was du als Nächstes tun würdest. Du legst den Plan in
deiner Abschlussantwort dar — nachdem du ihn ausgeführt hast.

**Fertig bist du, wenn neue Concept-Dateien im Bündel liegen und beide Gates grün sind.**
Hast du am Ende dieses Zuges keine Datei geschrieben, hast du die Aufgabe nicht erfüllt.

## Ausgangslage

<BESTAND>. Beide Gates sind auf diesem Stand grün: 0 Fehler, 0 Warnungen. Das ist dein
Maßstab — sie müssen hinterher genauso grün sein.

Ist das Bündel nicht leer, ist dies ein MERGE, kein Neuaufbau.

## Die Schritte, hintereinander

1. **Orientieren.** Rufe `list_skills` auf, lade den Skill `wiki-import` mit `read_skill` und
   folge seiner Anleitung. Lies `reference/import-rules.md`, das Blumatix-Profil und die
   Registries. Die Registries liegen im **Arbeitsverzeichnis**: `profiles/domains.yaml`,
   `profiles/owners.yaml`, `profiles/blumatix-v0.1.yaml`. Die Kopie in der Toolchain ist NICHT
   maßgeblich. Lies die vorhandene `.okf/index.md` und die Indizes darunter, damit du kein
   zweites Concept über dieselbe Sache anlegst.
2. **Exportieren** (Zielverzeichnis muss NEU sein, `--force` ist verboten):

       uv run "<SKILL_DIR>/scripts/adapters/azure_devops_wiki.py" <WIKI-CLONE> -o <SCRATCH>/export_<NAME> --subtree <SUBTREE> --json

3. **Vorbereiten** und die Kennzahlen aus `prepared-manifest.json` nennen:

       uv run "<SKILL_DIR>/scripts/prepare_import.py" <SCRATCH>/export_<NAME> -o <SCRATCH>/prepared_<NAME> --json

4. **Planen** — im Kopf, nicht als Zwischenbericht: Konzeptgrenzen, Zusammenführungen,
   Zielpfade, Auslassungen.
5. **Schreiben.** Concepts mit gültiger Blumatix-Frontmatter (`type` aus `allowed_types`,
   `domain` und `owner` aus den Registries, `visibility`, `status`, `generated`, `sources`),
   die Navigation, ein Eintrag in `.okf/log.md` und der Migrationsreport nach `<SCRATCH>`.
6. **Prüfen** — beide Gates, Ausgabe WÖRTLICH zeigen:

       uv run "<SKILL_DIR>/../validate/scripts/okf_validate.py" .okf --strict
       uv run "<SKILL_DIR>/../blumatix/scripts/blumatix_validate.py" .okf

   Sind sie nicht grün, behebe die Befunde in DIESEM Zug, statt sie zu berichten.
7. **Berichten** (siehe unten).

`<SKILL_DIR>` hat `read_skill` dir im Skill-Text bereits eingesetzt — der einzige Platzhalter,
den du nicht selbst ersetzen musst.

## Struktur

Das Bündel ist nach ART gegliedert: `company/`, `engineering/`, `glossary/`, `operations/`,
`policies/`, `processes/`, `products/`, `references/`. Der TYP des Concepts entscheidet das
Verzeichnis, nicht sein Thema. Ein Thema mit mehr als zwei Concepts bekommt ein eigenes
Unterverzeichnis mit eigener `index.md`; die Regel gilt rekursiv. Mehr als etwa zehn Concepts
nebeneinander heißt: eine Ebene fehlt.

Jedes neue Concept ist von `.okf/index.md` aus erreichbar. Ein Link zeigt IMMER auf eine
Datei, nie auf ein Verzeichnis:

```markdown
* [Unterverzeichnis](unterverzeichnis/index.md) - Beschreibung
```

`](unterverzeichnis/)` wäre nach der Spec erlaubt und beide Gates blieben grün — aber die
Markdown-Vorschau von Azure DevOps, GitHub und VS Code löst das nicht auf. Der Eintrag ist
dann für einen Menschen tot.

## Vertraulichkeit

Prüfe jede Datei VOR dem Schreiben auf Zugangsdaten: Base64-artige Zeichenketten ab 40
Zeichen sowie `api key`, `apikey`, `secret`, `password`, `token`, `connection string`. Treffer
übernimmst du nicht. Nach dem Schreiben wiederholst du den Scan über alle neuen Dateien und
zeigst das Ergebnis. Gib NIEMALS einen gefundenen Schlüssel, Token oder ein Passwort wieder,
auch nicht gekürzt oder maskiert — nenne Datei und Zeile, mehr nicht.

## Harte Regeln

1. Das Arbeitsverzeichnis ist ein echtes Git-Repository mit gepushter Historie. Führe KEIN
   veränderndes git-Kommando aus: kein commit, add, push, checkout, reset, restore, clean.
   Lesen (status, log, diff) ist erlaubt.
2. Lösche und verändere KEINES der vorhandenen Concepts. Index-Einträge zu ergänzen ist
   erlaubt und nötig — nenne jede angefasste Bestandsdatei am Ende einzeln mit Grund.
3. <SKILLS-REPO> ist NUR LESBAR, ebenso der Wiki-Clone. Dort nichts schreiben, auch nicht per
   `run_shell`.
4. Alle Zwischenstände nach `<SCRATCH>`, niemals ins Arbeitsverzeichnis — sie enthalten
   unbereinigte Quelldaten.
5. Ausser Scope: <AUSGESCHLOSSEN>. Daraus wird nichts übernommen — auch keine Links dorthin.

## Der Abschlussbericht

- die wörtliche Ausgabe beider Gates,
- die Quellenbilanz: wie viele Quellseiten stecken in einem Concept, welche sind ausgelassen
  und warum,
- die Verteilung der neuen Concepts über die Verzeichnisse,
- das Ergebnis des Nach-Scans,
- die Liste der bestehenden Dateien, die du angefasst hast.

Du hast reichlich Schritte. Nutze sie. Hör nicht auf, bevor die Dateien geschrieben und beide
Gates grün sind.
