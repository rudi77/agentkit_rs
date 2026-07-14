# agentkit installieren (Windows & Linux)

`agentkit` lässt sich als **Executable auf dem Rechner installieren** — wahlweise als
nativer **Rust**-Build oder als **Python**-Paket. Beide stellen denselben Befehl
`agentkit` bereit (One-shot, REPL und — beim Rust-Build mit Feature `tui` — ein
interaktives Terminal-UI).

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
irm https://raw.githubusercontent.com/rudi77/fsod/main/scripts/agentkit_setup.ps1 | iex
```

Danach nur noch die **Azure-Werte eintragen** (siehe [Konfiguration](#konfiguration-agentkitconfigjson)):

```powershell
notepad $env:USERPROFILE\.agentkit\config.json
agentkit config show            # prüft, ob alles gesetzt ist
agentkit "Was ist 17 + 25?"     # neue Shell öffnen, damit der PATH greift
```

Mit Optionen — `iex` reicht keine Parameter durch, deshalb über einen Scriptblock:

```powershell
$s = 'https://raw.githubusercontent.com/rudi77/fsod/main/scripts/agentkit_setup.ps1'
& ([scriptblock]::Create((irm $s))) -NoTui            # schlanke Variante ohne Terminal-UI
& ([scriptblock]::Create((irm $s))) -Version v0.11.0  # bestimmte Version
& ([scriptblock]::Create((irm $s))) -FromSource       # lokal aus dem Quellcode bauen (braucht Rust)
& ([scriptblock]::Create((irm $s))) -Uninstall        # Executable + PATH-Eintrag entfernen
```

| Option | Wirkung |
|---|---|
| `-NoTui` | schlanke Variante **ohne Terminal-UI** — für Skripte/Pipelines/CI (siehe [Varianten](#fertige-binaries-herunterladen-ci-releases)) |
| `-Version v0.11.0` | bestimmter Release-Tag (Default: `latest`) |
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

Die Skripte bauen lokal und legen `agentkit` in den PATH. Variante wählbar:
`rust`, `python` oder `both` (ohne Angabe automatisch erkannt).

**Linux / macOS**

```bash
./scripts/install.sh            # automatisch (rust, falls cargo vorhanden)
./scripts/install.sh rust       # nur Rust
./scripts/install.sh python     # nur Python
./scripts/install.sh both       # beide
./scripts/install.sh rust --no-tui   # Rust ohne Terminal-UI (schlanker)
```

**Windows (PowerShell)**

```powershell
.\scripts\install.ps1 rust
.\scripts\install.ps1 python
.\scripts\install.ps1 both
.\scripts\install.ps1 rust -NoTui
```

> Beim **Rust**-Build richten die Skripte zusätzlich die **Shell-Completion** ein
> (bash/fish unter Linux/macOS in die XDG-User-Verzeichnisse, PowerShell wird an
> `$PROFILE` angehängt). Manuell geht das jederzeit über
> `agentkit completions <bash|zsh|fish|powershell>` — siehe
> [`agent_framework_rs/README.md`](agent_framework_rs/README.md#shell-completions).

---

## Variante A — Rust (nativ, eine kleine Binary)

Voraussetzung: [Rust/Cargo](https://rustup.rs). Empfohlen für eine schlanke, schnelle
Executable ohne Laufzeitabhängigkeiten.

```bash
# Installiert `agentkit` nach ~/.cargo/bin (mit Terminal-UI + PDF-Support)
cargo install --path agent_framework_rs --bin agentkit --features "tui pdf"

# Ohne Terminal-UI (schlanker), PDF-Support behalten
cargo install --path agent_framework_rs --bin agentkit --features pdf
```

> Das Feature `pdf` bringt das `read-pdf`-Kommando und das `read_pdf`-Tool (z. B. für den
> [Accounts-Payable-Demo](agent_framework_rs/examples/accounts_payable/README.md)).

Stelle sicher, dass `~/.cargo/bin` (Windows: `%USERPROFILE%\.cargo\bin`) im PATH liegt —
`rustup` richtet das normalerweise ein.

## Variante B — Python (pipx / pip)

Voraussetzung: Python 3.10+. Liefert denselben `agentkit`-Befehl als Console-Script.

```bash
# Empfohlen: isoliert via pipx
pipx install ./agent_framework

# Alternativ: in die aktuelle Umgebung
pip install ./agent_framework

# Ohne Installation testen
python -m agentkit --demo "Was ist 17 + 25?"
```

## Variante C — Eigenständige Python-Executable (PyInstaller)

Erzeugt **eine Datei**, die ganz ohne Python-Installation läuft (z. B. zum Weitergeben).

```bash
# Linux/macOS
./scripts/build_pyinstaller.sh          # -> agent_framework/dist/agentkit
```

```powershell
# Windows
.\scripts\build_pyinstaller.ps1         # -> agent_framework\dist\agentkit.exe
```

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
| `agentkit-windows-x86_64.exe`     | Windows | `tui pdf` | der interaktive Alltag (inkl. `agentkit --tui`) |
| `agentkit-linux-x86_64`           | Linux   | `tui pdf` | dito |
| `agentkit-cli-windows-x86_64.exe` | Windows | `pdf`     | **Skripte, Pipelines, CI** — ohne `ratatui`, schlanker |
| `agentkit-cli-linux-x86_64`       | Linux   | `pdf`     | dito |

Die `cli`-Variante verhält sich identisch — One-shot, REPL, `--format json`, Exit-Codes,
`read-pdf`, Skills, MCP, Sub-Agenten. Sie enthält nur kein Terminal-UI; `--tui` weist sich
dort mit einem Hinweis ab. Für Automatisierung ist das die richtige Wahl: kleineres
Binary, nichts, was ein UI starten könnte. `pdf` ist bewusst in **beiden** drin — gerade
in Pipelines ist `agentkit read-pdf` das deterministische, tokenfreie Werkzeug (siehe
[Accounts-Payable-Demo](agent_framework_rs/examples/accounts_payable/README.md)).

> Der Python-Teil (PyInstaller) wird **nicht mehr released** — die Python-Variante bleibt
> als Paket bestehen (siehe Variante B/C), wandert aber nicht mehr in die Release-Assets.

Herunterladen, ausführbar machen (`chmod +x` unter Linux) und in ein PATH-Verzeichnis
legen — oder unter Windows einfach das [Setup-Skript](#schnellster-weg-windows-ein-befehl)
nehmen (`-NoTui` wählt die schlanke Variante).

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
globale Konfiguration angefasst werden muss.

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

Die **Python-CLI** kennt `~/.agentkit/config.json` nicht; sie lädt eine `.env`-Datei, falls
`python-dotenv` installiert ist (siehe [`agent_framework/.env.example`](agent_framework/.env.example)).
