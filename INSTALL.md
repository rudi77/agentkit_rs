# agentkit installieren (Windows & Linux)

`agentkit` lässt sich als **Executable auf dem Rechner installieren** — als nativer
**Rust**-Build (One-shot, REPL und — mit Feature `tui` — ein interaktives
Terminal-UI).

> Ohne API-Key läuft ein eingebauter, netzfreier **Demo-Modus** — die Executable ist
> also sofort nach der Installation nutzbar. Für ein echtes Modell setzt du
> `OPENAI_API_KEY` (optional `OPENAI_MODEL`) oder die `AZURE_OPENAI_*`-Variablen.

```bash
agentkit "Was ist 17 + 25?"     # One-shot: Auftrag ausführen, Antwort streamen
agentkit --repl                 # interaktiver Zeilen-REPL (Gedächtnis bleibt erhalten)
agentkit --tui                  # Terminal-UI (nur Rust-Build mit Feature `tui`)
agentkit --demo "3 + 4"         # Demo-Modus erzwingen (kein Netz/Key nötig)
agentkit --help
```

---

## Schnellster Weg (Windows): ein Befehl

Kein Rust, kein Klon, kein Admin — das Setup-Skript lädt die fertige Executable aus dem
GitHub-Release, legt sie nach `%LOCALAPPDATA%\Programs\agentkit\bin`, nimmt dieses
Verzeichnis in den **Benutzer-PATH** auf und erzeugt die Konfiguration unter
`%USERPROFILE%\.agentkit\config.json`:

```powershell
irm https://raw.githubusercontent.com/rudi77/agentkit_rs/main/scripts/agentkit_setup.ps1 | iex
```

Danach nur noch die **Azure-Werte eintragen** (siehe [Konfiguration](#konfiguration-agentkitconfigjson)):

```powershell
notepad $env:USERPROFILE\.agentkit\config.json
agentkit config show            # prüft, ob alles gesetzt ist
agentkit "Was ist 17 + 25?"     # neue Shell öffnen, damit der PATH greift
```

Mit Optionen — `iex` reicht keine Parameter durch, deshalb über einen Scriptblock:

```powershell
$s = 'https://raw.githubusercontent.com/rudi77/agentkit_rs/main/scripts/agentkit_setup.ps1'
& ([scriptblock]::Create((irm $s))) -NoTui            # schlanke Variante ohne Terminal-UI
& ([scriptblock]::Create((irm $s))) -Version v0.11.0  # bestimmte Version
& ([scriptblock]::Create((irm $s))) -FromSource       # lokal aus dem Quellcode bauen (braucht Rust)
& ([scriptblock]::Create((irm $s))) -Uninstall        # Executable + PATH-Eintrag entfernen
```

| Option | Wirkung |
|---|---|
| `-NoTui` | schlanke Variante **ohne Terminal-UI** — für Skripte/Pipelines/CI (siehe [Varianten](#fertige-binaries-herunterladen-ci-releases)) |
| `-Version v0.11.0` | bestimmter Release-Tag (Default: `latest`) |
| `-WithExamples` | Beispiele (accounts_payable, pr_review inkl. ADO-Reviewer-Skript, coding_swarm, …) aus dem Release nach `<InstallDir>\examples` entpacken (ab v0.13.0) |
| `-InstallDir DIR` | anderes Zielverzeichnis (Default: `%LOCALAPPDATA%\Programs\agentkit`) |
| `-NoPath` | PATH unangetastet lassen |
| `-NoCompletions` | keine PowerShell-Vervollständigung an `$PROFILE` anhängen |
| `-FromSource` | statt Download lokal mit `cargo` bauen (respektiert `-NoTui`) |
| `-Uninstall` | Executable + PATH-Eintrag entfernen (Konfiguration bleibt) |

> Angefasst wird genau eine Sache dauerhaft: die **PATH-Variable des Benutzers** — das ist
> unter Windows das, was „in den PATH aufnehmen“ heißt. Kein Admin, kein Installer, keine
> Uninstall-Einträge; `-Uninstall` räumt es wieder weg.

---

## Aus dem Quellcode bauen: Install-Skript

Die Skripte bauen lokal (via `cargo install`) und legen `agentkit` in den PATH.

**Linux / macOS**

```bash
./scripts/install.sh            # mit Terminal-UI + PDF
./scripts/install.sh --no-tui   # ohne Terminal-UI (schlanker)
```

**Windows (PowerShell)**

```powershell
.\scripts\install.ps1
.\scripts\install.ps1 -NoTui
```

> Die Skripte richten zusätzlich die **Shell-Completion** ein
> (bash/fish unter Linux/macOS in die XDG-User-Verzeichnisse, PowerShell wird an
> `$PROFILE` angehängt). Manuell geht das jederzeit über
> `agentkit completions <bash|zsh|fish|powershell>` — siehe
> [`agent_framework_rs/README.md`](agent_framework_rs/README.md#shell-completions).

---

## Aus dem Quellcode bauen: cargo install

Voraussetzung: [Rust/Cargo](https://rustup.rs). Ergebnis ist eine schlanke, schnelle
Executable ohne Laufzeitabhängigkeiten.

```bash
# Installiert `agentkit` nach ~/.cargo/bin — derselbe Feature-Satz wie der Release
cargo install --path agentkit_app --bin agentkit --features "tui pdf ctxman tiktoken graph work"

# Ohne Terminal-UI (schlanker), sonst identisch
cargo install --path agentkit_app --bin agentkit --features "pdf ctxman tiktoken graph work"
```

> Das sind exakt die Feature-Sätze der beiden Release-Varianten (siehe Tabelle unten);
> `scripts/install.ps1` bzw. `scripts/install.sh` bauen mit genau denselben.
> `pdf` bringt das `read-pdf`-Kommando und das `read_pdf`-Tool (z. B. für den
> [Accounts-Payable-Demo](agent_framework_rs/examples/accounts_payable/README.md)),
> `tiktoken` die exakte Token-Zählung für `ctxman`.

Stelle sicher, dass `~/.cargo/bin` (Windows: `%USERPROFILE%\.cargo\bin`) im PATH liegt —
`rustup` richtet das normalerweise ein.

---

## Fertige Binaries herunterladen (CI-Releases)

Bei jedem Versions-Tag (`v*`) baut der Workflow
[`.github/workflows/release.yml`](.github/workflows/release.yml) die **Rust**-Executables
für Windows & Linux und hängt sie an den GitHub-Release:

```bash
git tag v0.11.0
git push origin v0.11.0
```

Pro Plattform gibt es **zwei Varianten** — derselbe Agent-Kern, nur ein anderer
Feature-Satz:

| Datei | Plattform | Features | Wofür |
|---|---|---|---|
| `agentkit-windows-x86_64.exe`     | Windows | `tui pdf ctxman tiktoken graph work` | der interaktive Alltag (inkl. `agentkit --tui`) |
| `agentkit-linux-x86_64`           | Linux   | `tui pdf ctxman tiktoken graph work` | dito |
| `agentkit-cli-windows-x86_64.exe` | Windows | `pdf ctxman tiktoken graph work` | **Skripte, Pipelines, CI** — ohne `ratatui`, schlanker |
| `agentkit-cli-linux-x86_64`       | Linux   | `pdf ctxman tiktoken graph work` | dito |

Seit v0.13.1 enthalten alle Varianten das volle **Context-Management** (`ctxman`):
`agentkit --ctx <dir>` aktiviert Watermark-GC, verlustfreie Auslagerung großer
Tool-Ergebnisse (`expand_context_ref`) und Snapshot-Resume über Prozessgrenzen.

Ebenso in allen Varianten: der **Wissensgraph** (`graph`). `agentkit --graph <dir>`
gibt dem Agenten ein Gedächtnis über Sessions hinweg — `graph_search`,
`graph_remember`, `graph_promote` und Provenance zu jeder gespeicherten Aussage;
`--graph-readonly` lässt ihn nur lesen. Dynamisch erzeugte Schwarm-Mitglieder
teilen sich denselben Graphen.

Und ebenfalls in allen Varianten: die **Arbeits-Runtime** (`work`). `agentkit work
create --title … --objective …` legt ein Vorhaben an, `agentkit work run <id>`
arbeitet es ab, `agentkit work status <id>` zeigt den Stand, `agentkit work resume
<id>` macht nach einem Absturz oder Ctrl-C dort weiter, wo es stand. Gedacht für
Vorhaben, die länger laufen als ein einzelner Agent-Lauf: der Arbeitszustand liegt
in einem Journal unter `.agentkit/work/<projekt-id>/`, nicht im Prozess.

Die `cli`-Variante verhält sich identisch — One-shot, REPL, `--format json`, Exit-Codes,
`read-pdf`, Skills, MCP, Sub-Agenten. Sie enthält nur kein Terminal-UI; `--tui` weist sich
dort mit einem Hinweis ab. Für Automatisierung ist das die richtige Wahl: kleineres
Binary, nichts, was ein UI starten könnte. `pdf` ist bewusst in **beiden** drin — gerade
in Pipelines ist `agentkit read-pdf` das deterministische, tokenfreie Werkzeug (siehe
[Accounts-Payable-Demo](agent_framework_rs/examples/accounts_payable/README.md)).

Herunterladen, ausführbar machen (`chmod +x` unter Linux) und in ein PATH-Verzeichnis
legen — oder unter Windows einfach das [Setup-Skript](#schnellster-weg-windows-ein-befehl)
nehmen (`-NoTui` wählt die schlanke Variante).

Zusätzlich liegt ab v0.13.0 **`agentkit-examples.zip`** im Release: die kompletten
Beispiele (accounts_payable, `pr_review` inkl. ADO-Reviewer-Skript, coding_swarm,
logwatch, win_triage) zum Entpacken neben die Binary — unter Windows per
`-WithExamples` des Setup-Skripts, unter Linux:

```bash
curl -fsSL -o /tmp/agentkit-examples.zip \
  https://github.com/rudi77/agentkit_rs/releases/latest/download/agentkit-examples.zip
unzip /tmp/agentkit-examples.zip -d ~/agentkit   # -> ~/agentkit/examples/…
```

---

## Konfiguration: `~/.agentkit/config.json`

Der Rust-`agentkit` liest seine Zugangsdaten aus einer JSON-Datei im Benutzerverzeichnis —
`%USERPROFILE%\.agentkit\config.json` (Linux/macOS: `~/.agentkit/config.json`). Das
Setup-Skript legt sie an; von Hand geht es mit `agentkit config init`.

```jsonc
{
  "provider": "auto",                  // auto | azure | openai | demo
  "azure": {
    "endpoint": "https://<DEINE-RESSOURCE>.openai.azure.com",
    "api_key": "<DEIN-AZURE-API-KEY>",
    "deployment": "<DEIN-DEPLOYMENT-NAME>",
    "api_version": "2024-10-21"
  },
  "openai": { "api_key": "", "model": "gpt-4o-mini" },
  "env": {}                            // beliebige weitere Umgebungsvariablen
}
```

Nur die drei Azure-Werte müssen eingetragen werden. **Platzhalter in spitzen Klammern
werden ignoriert** — eine unausgefüllte Datei führt zum netzfreien Demo-Modus, nicht zu
einem 401 vom Endpunkt.

```powershell
agentkit config path     # wo liegt die Datei?
agentkit config init     # Vorlage anlegen (überschreibt nichts)
agentkit config show     # welche Werte sind wirksam? (Keys maskiert; Exit 3 = kein Anbieter)
```

### Rangfolge

Die Datei ist die *unterste* Ebene — Projekte können sie überschreiben:

```text
echte Umgebungsvariable  >  .env im Arbeitsverzeichnis  >  ~/.agentkit/config.json
```

So bleibt eine Projekt-`.env` (z. B. mit einem anderen Deployment) wirksam, ohne dass die
globale Konfiguration angefasst werden muss. Eine kommentierte Vorlage liegt unter
[`agent_framework_rs/.env.example`](agent_framework_rs/.env.example) — kopieren nach
`.env` und ausfüllen (für die Benchmarks gibt es eine eigene:
[`agent_benchmarks/.env.example`](agent_benchmarks/.env.example)).

### Die zugrunde liegenden Variablen

`config.json` wird auf genau diese Umgebungsvariablen abgebildet — wer sie direkt setzt
(CI, Container, Python-CLI), braucht die Datei nicht:

| Variable | Bedeutung |
|---|---|
| `AZURE_OPENAI_API_KEY`      | aktiviert den Azure-Pfad |
| `AZURE_OPENAI_ENDPOINT`     | Azure-Endpoint |
| `AZURE_OPENAI_DEPLOYMENT`   | Azure-Deployment-Name |
| `AZURE_OPENAI_API_VERSION`  | optional (Default `2024-10-21`) |
| `OPENAI_API_KEY`            | aktiviert den OpenAI-Pfad |
| `OPENAI_MODEL`              | Modellname (Default `gpt-4o-mini`) |
