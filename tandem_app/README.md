# TANDEM AgentKit-Adapter

Lokaler Rust-Prozess für `../tandem_vscode`. Er verwendet den AgentKit-Loop mit
einer eigenen Registry aus Snapshot-Lesen, Planvorschlag und begrenztem
Patch-Vorschlag. Der zusätzliche Repository-Agent erkundet den vom Host gebundenen
Projektordner lesend und schlägt einen belegten, mehrteiligen Änderungsplan vor.
Er schreibt keine Produktdateien und führt keine Shell aus.

```powershell
cargo test --manifest-path tandem_app/Cargo.toml
cargo build --manifest-path tandem_app/Cargo.toml --features openai
```

Der Standard-Build ist offline. Das Feature `openai` aktiviert die bestehenden
AgentKit-Provider einschließlich Azure und Anthropic. `tandem --provider auto`
verwendet die vorhandene globale AgentKit-Konfiguration. `--provider demo` wählt
ausdrücklich den geskripteten Offline-Lauf.

`--workspace ABSOLUTER_PFAD` bindet die Lesewerkzeuge an einen Projektordner.
Der Agent kann diese Wurzel nicht durch Tool-Argumente ersetzen. Ignore-Regeln,
Pfadsperren und Verknüpfungsprüfung liegen in `src/repository.rs`.

Transport: JSON-RPC 2.0 mit einer Nachricht pro Zeile auf stdin/stdout. `ping`
liefert Version und Anbieterwahl, `run` nimmt `RunRequest` aus `src/lib.rs` an,
`explore` den `ExploreRequest` aus `src/exploration.rs`. `check_path` und
`check_paths` prüfen Pfade vor Übermittlung offener Editorstände;
`validate_exploration` prüft Quellen und Suchkatalog vor einer Planübernahme.
`workspace_snapshot` bindet für reale Prüfungen die Inhalte sämtlicher Dateien
des erlaubten Suchraums, einschließlich Änderungen anderer Quelldateien. Es
liefert ausschließlich Pfade und Hashes, keinen zusätzlichen Modellkontext.
Der Snapshot bricht bei verändertem Katalog, Lesefehlern oder mehr als 1 GB ab.
Ignore-, Pfad- und Link-Sperren gelten weiterhin; ein Betriebssystem-Sandboxbeweis
ist das nicht.
`cancel` setzt den kooperativen Stop-Knopf. Pro Prozess läuft höchstens ein Auftrag.
Währenddessen kommen `event`-Benachrichtigungen mit `run_id` und AgentKit-Event.
Die Extension beendet beim Stoppen zusätzlich den Kindprozess, damit ein
blockierender Netzaufruf nicht auf das nächste Cancel-Polling warten muss.

`propose_scope_change` legt Grund, Dateien/Symbole, kleinere Alternative und Risiko
einer nötigen Erweiterung vor. Der Lauf verwirft vorhandene Patches und darf
danach keine weiteren Patches erzeugen. Die Extension führt die Entscheidung
zurück zum Entwickler; die Tool-Antwort erweitert keine Rechte.

Der Adapter ist eine neue Integration, kein Port. AgentKit und seine bestehenden
Frontends bleiben unverändert. Architektur, Bedienung und Grenzen des ersten
Prototyps sind in `../tandem_vscode/README.md` beschrieben.
