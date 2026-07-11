# Accounts Payable mit agentkit — ein Beispiel, zwei Betriebsarten

Ein praxisnahes Beispiel, wie man mit **agentkit** einen kleinen, aber vollständigen
**Eingangsrechnungs-Prozess** (Accounts Payable) für deutsche Kleinunternehmer und Freelancer
baut — nach dem Unix-Prinzip *„ein Werkzeug, eine Aufgabe, zusammensteckbar"*. Dasselbe Beispiel
läuft in **zwei Modi**, die sich **dieselben Bausteine** teilen:

- **Batch** — eine deterministische Pipeline über `.\inbox`: ein Werkzeug bzw. ein Agent pro
  Stufe, volle Compliance-Artefakte je Rechnung. Nicht-interaktiv, ideal für Automatisierung/CI.
- **Interaktiv (TUI/REPL)** — ein **Orchestrator-Agent** (*Frau Berger, die Leiterin der
  Buchhaltung*) führt dasselbe Fach-Team, **ruft dieselben Compliance-Werkzeuge**, **redet mit
  dir** (Human-in-the-Loop), **fragt bei Unklarheiten nach** und baut dabei einen **lernenden
  Company Knowledge Graph** auf.

> ⚠️ **Kein Steuer- oder Rechtsrat.** Ein Demo, das Komposition, Orchestrierung, HITL und einen
> lernenden Wissensgraph zeigt — nicht für den Produktiveinsatz gedacht.

## Warum das gerade wichtig ist

Seit **01.01.2025** gilt in Deutschland die **E-Rechnungspflicht** im B2B: strukturierte
Rechnungen (**XRechnung**, **ZUGFeRD/Factur-X** nach **EN 16931**) müssen empfangen und
verarbeitet werden. Das Beispiel deckt beides ab — klassische Papier-/PDF-Rechnungen **und**
E-Rechnungen — und ergänzt **GoBD-Ablage**, **DATEV-Export** und **Dublettenprüfung**.

## Die gemeinsamen Bausteine

Beide Modi nutzen dieselben Teile — das ist der Kern der Fusion:

| Baustein | Ort | Aufgabe |
|---|---|---|
| **Fach-Logik** | `roles/` (interaktiv) · `prompts/` (Batch) | Extraktion (§14) · Validierung · Buchung (SKR03) — dieselbe Logik in zwei Formen |
| **Compliance-Werkzeuge** | `tools/*.ps1` | `xcheck` (EN 16931) · `check-duplicate` (Dublette) · `datev-export` (EXTF) · `gobd-manifest` (SHA-256) |
| **Deterministische Helfer** | `modules/ap-helpers.ps1` | eine Quelle für Format-Erkennung, xcheck-Aufruf, GoBD, DATEV, Register |
| **Beispiel-Rechnungen** | `inbox/` | alle vier Formate + zwei Text-Rechnungen (bekannter/unbekannter Lieferant) |

Deterministische Schritte (PDF-Text, EN-16931-Konformität, DATEV, GoBD, Dublette) sind **reine
Werkzeuge** — kein LLM-Raten, wo strukturierte Daten oder Rechenregeln genügen. Die LLM-Schritte
sind spezialisierte Agenten, konfiguriert allein über ihren System-Prompt.

## Was jeder Modus kann

|  | **Batch** | **Interaktiv** |
|---|:---:|:---:|
| Vier Eingangsformate (PDF · XRechnung · ZUGFeRD · Text) | ✅ | ✅ |
| E-Rechnungsprüfung EN 16931 (xcheck) | ✅ | ✅ |
| Validierung §14 + Arithmetik | ✅ | ✅ |
| SKR03-Buchungsvorschlag | ✅ | ✅ |
| DATEV-EXTF-Export (je Rechnung + Sammelstapel) | ✅ | ✅ |
| GoBD-Ablage (schreibgeschützt + SHA-256-Manifest) | ✅ | ✅ |
| Dublettenprüfung (Register) | ✅ | ✅ |
| **Orchestrator-Agent** koordiniert den Prozess | — | ✅ |
| **Human-in-the-Loop** (`ask_user`, Rückfrage mitten in der Aufgabe) | — | ✅ |
| **Lernender Wissensgraph** (OKF: Lieferanten, Kontierung) | — | ✅ |
| Voll deterministisch, nicht-interaktiv (CI) | ✅ | — |

Der interaktive Orchestrator ist damit ein **Superset** der Batch-Fähigkeiten — er umschließt die
deterministischen Bausteine mit Orchestrierung, Rückfrage und Gedächtnis. Die Batch-Pipeline
bleibt die **deterministische Referenz** mit garantierter Artefaktstruktur.

## Voraussetzungen

1. **agentkit mit TUI + PDF** (Features `tui pdf`):

   ```powershell
   # im Repo-Ordner agent_framework_rs
   cargo build --release --bin agentkit --features "tui pdf"
   ```

2. **LLM-Credentials.** Das Skript lädt automatisch eine `.env` (neben dem Skript oder
   `agent_framework_rs\.env`) und wählt den Provider per `auto`:
   - **Azure:** `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_DEPLOYMENT`
   - **OpenAI:** `OPENAI_API_KEY` (optional `OPENAI_MODEL`)

3. **xcheck-API** (optional, für die E-Rechnungs-Prüfung). Separates Repo `rudi77/xcheck`.
   Lokaler Start (Postgres + KoSIT via Docker, API per `dotnet run`):

   ```bash
   cd xcheck
   docker compose -f docker-compose.dev.yml up -d --build
   export ConnectionStrings__Postgres="Host=localhost;Database=invoiceport;Username=invoiceuser;Password=dev"
   export Kosit__BaseUrl="http://localhost:8080"  Stripe__WebhookSecret="whsec_localtest"
   dotnet run --project src/InvoicePort.Api --urls http://localhost:5080 &
   CREDITS=1000 ./scripts/seed-tenant.sh          # legt Tenant + API-Key an (inv_port_… wird ausgegeben)
   ```

   Den Key per `-XCheckApiKey` bzw. Env `XCHECK_API_KEY` an das Skript geben. Ohne xcheck wird die
   E-Rechnungs-Prüfung sauber übersprungen (der Rest läuft weiter).

## Ausführen

Ein Launcher, ein `-Mode`:

```powershell
# Beispielrechnungen erzeugen (Papier-PDF, XRechnung-XML, ZUGFeRD-PDF):
.\tools\Build-Samples.ps1        # oder .\tools\New-InboxBatch.ps1 für 10 gemischte Rechnungen

# --- Interaktiv (Default): Orchestrator im TUI ---
.\Invoke-Ap.ps1 -Mode Interactive
.\Invoke-Ap.ps1 -Mode Interactive -Fresh    # Gelerntes verwerfen, neu aus dem Seed

# --- Batch: deterministische Pipeline über die Inbox ---
.\Invoke-Ap.ps1 -Mode Batch
.\Invoke-Ap.ps1 -Mode Batch -XCheckUrl 'http://localhost:5080' -XCheckApiKey 'inv_port_…'

# --- Repl: wie Interactive, aber scriptbar (stdin) ---
.\Invoke-Ap.ps1 -Mode Repl
```

### Interaktiv — was passiert

Im TUI z. B. eintippen:

```
Verarbeite die Eingangsrechnung inbox/rechnung_meier.txt und melde mir das Ergebnis.
```

Der Orchestrator extrahiert, prüft E-Rechnung/Dublette über die `tools/`, sucht den Lieferanten
im Graph — und da **Bürobedarf Meier** unbekannt ist, **fragt er dich** nach Kostenstelle, Konto
und Freigabe-Verantwortlicher (`ask_user`). Danach legt er die OKF-Entitäten an, bucht, exportiert
DATEV, archiviert GoBD-konform und berichtet. **Beim zweiten Lauf** desselben Lieferanten fragt er
**nicht** mehr — er hat gelernt. Der Seed enthält bereits den **bekannten** Lieferanten
*Tischlerei Thomas Berg* → `inbox/rechnung_berg.txt` zeigt den „kein-Nachfragen"-Fall.

Shell-Aufrufe an die Compliance-Werkzeuge werden per `--yes` automatisch freigegeben; die
menschlichen Entscheidungen laufen bewusst über `ask_user`. Mit `-ApproveShell` bestätigst du jede
Shell-Ausführung einzeln.

### Batch — was entsteht

Pro Rechnung ein Ordner `out\<name>\` mit allen Stufen (`00_source` … `07_report.md` +
`manifest.json`), plus `out\datev_buchungsstapel.csv` und `out\_register.json`. Eine
Zusammenfassungstabelle zeigt je Rechnung Format, EN-16931-Status, Gesamtstatus und Buchbarkeit.

| # | Stufe | Werkzeug | LLM? |
|---|-------|----------|------|
| 00 | **Ingest** | Format erkennen, Original GoBD-konform ablegen | nein |
| 01 | **Inhalt** | `agentkit read-pdf` bzw. XML direkt | nein |
| 02 | **E-Rechnung** | `tools/xcheck.ps1` (EN 16931 / KoSIT) | nein |
| 03 | **Extraktion** | agentkit-Agent (§14-Merkmale) | ja |
| 04 | **Validierung** | agentkit-Agent (Arithmetik + §14 + EN + Dublette) | ja |
| 05 | **Buchung** | agentkit-Agent (SKR03) | ja |
| 06 | **DATEV** | `tools/datev-export.ps1` (EXTF-CSV) | nein |
| 07 | **Report** | agentkit-Agent (Markdown) | ja |
| — | **GoBD** | SHA-256-Manifest über alle Artefakte | nein |

## Der Company Knowledge Graph (OKF)

`knowledge/` ist die Wissensbasis im **[Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf)**:
je Entität **eine Markdown-Datei** mit YAML-Frontmatter und `[[links]]`; der Graph ist netz-, nicht
baumförmig. Typen: `lieferant`, `kostenstelle`, `person`, `rechnung` (Details in
[`knowledge/index.md`](knowledge/index.md)). Der interaktive Orchestrator liest und **erweitert**
ihn — so lernt die Buchhaltung dazu.

## Das `ask_user`-Werkzeug (Human-in-the-Loop)

Damit ein Agent **mitten in der Aufgabe** eine Rückfrage stellen kann, hat agentkit das Werkzeug
**`ask_user`** (nur der Orchestrator; Sub-Agenten melden Unklarheiten an ihn zurück). Es wirkt im
**REPL** (Antwort über stdin) und im **TUI** (Eingabedialog). In einer nicht-interaktiven Pipe
liefert es eine Sentinel-Antwort, damit nichts blockiert. `--repl` macht die Session **scriptbar**
(Kommandos und Antworten von stdin) — praktisch für Automatisierung und Tests.

## Tests

```powershell
# Deterministische Offline-Tests der Helfer (DATEV, GoBD, Dublette) — kein Netz, kein LLM:
pwsh -File .\tests\Test-ApHelpers.ps1

# Deterministische Tests der komponierbaren Compliance-Werkzeuge (tools/*.ps1):
pwsh -File .\tests\Test-ComplianceTools.ps1

# Scriptbarer End-to-End-Test des HITL-Lernpfads (echtes Modell nötig; nutzt --repl):
pwsh -File .\tests\Test-Interactive.ps1

# Live-Integrationstest gegen die xcheck-API (übersprungen, wenn nicht konfiguriert):
$env:XCHECK_URL='http://localhost:5080'; $env:XCHECK_API_KEY='inv_port_…'
pwsh -File .\tests\Test-XCheckIntegration.ps1
```

## Dateien

```
accounts_payable/
  Invoke-Ap.ps1                # EIN Launcher: -Mode Batch | Interactive | Repl
  orchestrator.md              # System-Prompt der „Leiterin der Buchhaltung" (interaktiv)
  roles/                       # Fach-Rollen: extractor, validator, booker (delegierbar)
  prompts/                     # dieselbe Fach-Logik als Batch-Pipe-Stufen
  modules/ap-helpers.ps1       # deterministische Helfer (eine Quelle für beide Modi)
  tools/
    xcheck.ps1                 # EN-16931-Prüfung (komponierbares Kommando)
    check-duplicate.ps1        # Dublettenprüfung + Register
    datev-export.ps1           # DATEV-EXTF-Buchungsstapel
    gobd-manifest.ps1          # GoBD-Ablage + SHA-256-Manifest
    Build-Samples.ps1 · New-InboxBatch.ps1 · New-SampleInvoicePdf.ps1 · zugferd-embed.xml
  knowledge/                   # OKF-Wissensgraph (Seed): index + Lieferant/Kostenstelle/Person
  inbox/                       # Beispielrechnungen (alle Formate + Text-Rechnungen für HITL/Lernen)
  tests/                       # Test-ApHelpers · Test-ComplianceTools · Test-Interactive · Test-XCheckIntegration
  out/                         # Batch-Ergebnisse (generiert, nicht eingecheckt)
  workspace/                   # interaktiver Arbeitsordner (generiert) — hier wächst der Graph
```

## Warum das ein gutes agentkit-Beispiel ist

- **Komposition statt Monolith:** kleine, einzweckige Schritte — als Pipe verkettet (Batch) oder
  von einem Orchestrator komponiert (interaktiv).
- **Richtiges Werkzeug pro Schritt:** deterministische Werkzeuge (read-pdf, xcheck, DATEV, GoBD,
  Dublette) fürs Faktische, LLM-Agenten fürs Urteilen — statt eines „Mach-alles"-Prompts.
- **Ein Kern, zwei Frontends:** dieselben Bausteine, einmal ohne und einmal mit Mensch im Loop.
- **Human-in-the-Loop & Lernen:** `ask_user` + OKF-Wissensgraph zeigen, wie ein Agent Firmenwissen
  erfragt und dauerhaft behält — nicht auf Accounts Payable beschränkt (siehe
  [Benutzerhandbuch](../../docs/USER_MANUAL.md)).
- **Nachvollziehbarkeit:** jeder Zwischenschritt liegt als Datei vor — auditierbar, GoBD-nah.
