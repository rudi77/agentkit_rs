Freigegeben. Führe deinen vorgelegten Split-/Merge-Plan jetzt aus.

Erzeuge die Concepts im Bündel, verlinke sie von `index.md` aus erreichbar, trage den Import
in `.okf/log.md` ein und erzeuge den Migrationsreport nach <SCRATCH>.

Danach beide Gates ausführen und deren Ausgabe WÖRTLICH zeigen:

    uv run "<SKILL_DIR>/../validate/scripts/okf_validate.py" .okf --strict
    uv run "<SKILL_DIR>/../blumatix/scripts/blumatix_validate.py" .okf

Zum Abschluss die Quellenbilanz: wie viele der Quellseiten stecken in einem Concept, welche
sind ausgelassen und warum.

Es gelten unverändert die harten Regeln aus dem ersten Auftrag.
