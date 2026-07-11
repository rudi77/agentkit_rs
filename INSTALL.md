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

## Schnellster Weg: Install-Skript

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
[`.github/workflows/release.yml`](.github/workflows/release.yml) automatisch
Executables für **Windows & Linux** (Rust- und Python-Variante) und hängt sie an den
GitHub-Release. So veröffentlichst du einen Release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Danach liegen unter **Releases** u. a. diese Dateien:

| Datei | Plattform | Variante |
|---|---|---|
| `agentkit-rust-linux-x86_64`       | Linux   | Rust (mit TUI) |
| `agentkit-rust-windows-x86_64.exe` | Windows | Rust (mit TUI) |
| `agentkit-python-linux-x86_64`     | Linux   | Python (PyInstaller) |
| `agentkit-python-windows-x86_64.exe` | Windows | Python (PyInstaller) |

Herunterladen, ausführbar machen (`chmod +x` unter Linux) und in ein PATH-Verzeichnis
legen — fertig.

---

## Konfiguration (echtes Modell)

| Variable | Bedeutung |
|---|---|
| `OPENAI_API_KEY`            | aktiviert den OpenAI-Pfad |
| `OPENAI_MODEL`              | Modellname (Default `gpt-4o-mini`) |
| `AZURE_OPENAI_API_KEY`      | aktiviert den Azure-Pfad |
| `AZURE_OPENAI_ENDPOINT`     | Azure-Endpoint |
| `AZURE_OPENAI_DEPLOYMENT`   | Azure-Deployment-Name |
| `AZURE_OPENAI_API_VERSION`  | optional (Default `2024-10-21`) |

Die Python-CLI lädt zusätzlich automatisch eine `.env`-Datei, falls `python-dotenv`
installiert ist (siehe [`agent_framework/.env.example`](agent_framework/.env.example)).
