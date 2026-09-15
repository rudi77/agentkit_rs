Notfall-Auftrag: `10_import.md` ist auf EINEN Zug ausgelegt und braucht diese Datei im
Normalfall nicht. Sie ist für den Fall, dass der Agent trotzdem angehalten hat — dann gibst
du damit in DERSELBEN Session frei, statt neu zu starten.

Freigegeben. Führe deinen vorgelegten Split-/Merge-Plan jetzt aus, ohne erneut anzuhalten.

Erzeuge die Concepts im Bündel, verlinke sie von `index.md` aus erreichbar und trage den
Import in `.okf/log.md` ein.

Danach alle DREI Gates ausführen und deren Ausgabe WÖRTLICH zeigen:

    uv run "<SKILL_DIR>/../validate/scripts/okf_validate.py" .okf --strict
    uv run "<SKILL_DIR>/../blumatix/scripts/blumatix_validate.py" .okf --strict
    uv run "<SKILL_DIR>/scripts/migration_report.py" --prepared <SCRATCH>/prepared_<NAME> --bundle .okf --max-unmapped <MAX-UNMAPPED> -o <SCRATCH>/migration-report.json

Der Migrationsreport ist das dritte Gate, kein Bericht: ohne `--max-unmapped` endet er auch
dann mit 0, wenn die Hälfte der Quellseiten nirgends gelandet ist. Das `-o` zeigt nach
`<SCRATCH>`, nicht ins Arbeitsverzeichnis.

Zum Abschluss die Quellenbilanz: `unmapped_pages` als Zahl, dazu jede dieser Seiten einzeln
mit dem Grund, aus dem sie kein Concept geworden ist.

Es gelten unverändert die harten Regeln aus dem ersten Auftrag.
