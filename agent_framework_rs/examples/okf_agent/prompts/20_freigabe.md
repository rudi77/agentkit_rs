Notfall-Auftrag: `10_import.md` ist auf EINEN Zug ausgelegt und braucht diese Datei im
Normalfall nicht. Sie ist für den Fall, dass der Agent trotzdem angehalten hat — dann gibst
du damit in DERSELBEN Session frei, statt neu zu starten.

Freigegeben. Führe deinen vorgelegten Split-/Merge-Plan jetzt aus, ohne erneut anzuhalten.

Erzeuge die Concepts im Bündel, verlinke sie von `index.md` aus erreichbar, trage den Import
in `.okf/log.md` ein und erzeuge den Migrationsreport nach <SCRATCH>.

Danach beide Gates ausführen und deren Ausgabe WÖRTLICH zeigen:

    uv run "<SKILL_DIR>/../validate/scripts/okf_validate.py" .okf --strict
    uv run "<SKILL_DIR>/../blumatix/scripts/blumatix_validate.py" .okf

Zum Abschluss die Quellenbilanz: wie viele der Quellseiten stecken in einem Concept, welche
sind ausgelassen und warum.

Es gelten unverändert die harten Regeln aus dem ersten Auftrag.
