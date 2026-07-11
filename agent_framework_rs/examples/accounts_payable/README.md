# Accounts Payable mit agentkit — Rechnungsverarbeitung als komponierte Agenten-Pipeline

Ein praktisches Beispiel, wie man mit **agentkit** und **reinem PowerShell** einen kleinen,
aber vollständigen **Eingangsrechnungs-Prozess** (Accounts Payable) für deutsche
Kleinunternehmer und Freelancer baut — nach dem Unix-Prinzip *„ein Werkzeug, eine Aufgabe,
zusammensteckbar“*.

> ⚠️ **Kein Steuer- oder Rechtsrat.** Ein Demo, das das Kompositionsprinzip zeigt — nicht
> für den Produktiveinsatz in der Buchhaltung gedacht.

## Die Idee: ein Agent pro Schritt

Statt eines monolithischen „Mach-alles“-Prompts ist jeder Verarbeitungsschritt ein
**eigenständiger, spezialisierter Agent**, konfiguriert allein durch seinen System-Prompt
(`--system-file`). Die Schritte werden über **stdin/stdout** verkettet — genau wie klassische
Unix-Filter (`cat | grep | sort`). Das PowerShell-Skript ist nur der Klebstoff, der die
Kommandos verbindet und die Zwischenergebnisse je Rechnung in einem Ordner ablegt.

```
                 ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
  rechnung.pdf   │ 02 Extrakt. │   │ 03 Validln. │   │ 04 Buchung  │   │ 05 Report   │
   │             │  §14 UStG   │   │  Prüfregeln │   │   SKR03     │   │  Markdown   │
   ▼             │  → JSON     │   │  → JSON     │   │  → JSON     │   │  → .md      │
┌──────────┐     └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘
│01 Ingest │        ▲                  ▲                  ▲                  ▲
│read-pdf  │ ─txt─► agentkit ─json─►  agentkit ─json─►  agentkit ─json─►  agentkit ─md─►
│(kein LLM)│        (Agent 1)          (Agent 2)          (Agent 3)          (Agent 4)
└──────────┘
```

Jede Stufe ist für sich testbar, austauschbar und wiederverwendbar. Willst du z. B. die
Buchungslogik von SKR03 auf SKR04 umstellen, tauschst du **einen** System-Prompt — der Rest
der Pipeline bleibt unberührt. Das ist Komposition.

### Die Stufen

| # | Stufe | Kommando | LLM? | Ergebnis |
|---|-------|----------|------|----------|
| 01 | **Ingest** | `agentkit read-pdf 00_source.pdf` | nein (deterministisch) | `01_text.txt` |
| 02 | **Extraktion** | `agentkit -p --format json --system-file prompts/02_extract_fields.md` | ja | `02_fields.json` |
| 03 | **Validierung** | `agentkit -p --format json --system-file prompts/03_validate.md` | ja | `03_validation.json` |
| 04 | **Buchung (SKR03)** | `agentkit -p --format json --system-file prompts/04_book.md` | ja | `04_booking.json` |
| 05 | **Report** | `agentkit -p --system-file prompts/05_report.md` | ja | `05_report.md` |

Bewusste Design-Entscheidung: **Stufe 01 ist kein Agent.** Text aus einer PDF zu holen ist
eine deterministische Aufgabe — dafür gibt es das tokenfreie Utility `agentkit read-pdf`.
Nur die *urteilenden* Schritte (extrahieren, validieren, buchen, berichten) sind LLM-Agenten.
Das richtige Werkzeug pro Schritt zu wählen, ist Teil des Kompositionsgedankens.

### Was geprüft wird (§ 14 UStG)

Die Extraktion zieht die umsatzsteuerlichen Pflichtangaben nach **§ 14 Abs. 4 UStG**
(Name/Anschrift beider Parteien, Steuernummer/USt-IdNr., Rechnungsnummer, Ausstellungs- und
Leistungsdatum, Menge/Art, nach Steuersätzen aufgeschlüsseltes Entgelt, Steuersatz, Steuerbetrag).
Die Validierung rechnet nach (Netto + USt = Brutto, Steuer = Netto × Satz), prüft
Pflichtangaben und erkennt Sonderfälle: **Kleinunternehmer § 19 UStG**,
**Reverse-Charge § 13b UStG**, **Kleinbetragsrechnung § 33 UStDV** (≤ 250 € brutto).

## Voraussetzungen

1. **agentkit mit PDF-Support** bauen (Feature `pdf` bringt `read-pdf` + das `read_pdf`-Tool):

   ```powershell
   # im Repo-Ordner agent_framework_rs
   cargo build --release --bin agentkit --features pdf
   # (oder dauerhaft installieren:)
   cargo install --path . --bin agentkit --features "pdf tui"
   ```

2. **LLM-Credentials.** Die Pipeline lädt automatisch eine `.env` (erst neben dem Skript,
   dann `agent_framework_rs\.env`) und wählt den Provider per `auto`:
   - **Azure:** `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_DEPLOYMENT`
   - **OpenAI:** `OPENAI_API_KEY` (optional `OPENAI_MODEL`)

## Ausführen

```powershell
# 1) Beispielrechnungen (PDF) erzeugen — eine saubere, eine mit Mängeln:
.\tools\Build-Samples.ps1

# 2) Pipeline über alle PDFs in .\inbox laufen lassen:
.\Invoke-ApPipeline.ps1

# Optional: Provider/Modell erzwingen, eigener Inbox-/Out-Ordner:
.\Invoke-ApPipeline.ps1 -Provider azure
.\Invoke-ApPipeline.ps1 -InboxDir C:\rechnungen -OutDir C:\ap_ergebnisse
```

Ergebnis: pro Rechnung ein Ordner unter `out\<rechnungsname>\` mit **allen** Zwischen- und
Endergebnissen:

```
out/rechnung_sauber/
  00_source.pdf        # Kopie der Originalrechnung
  01_text.txt          # extrahierter Rohtext (read-pdf)
  02_fields.json       # §14-Merkmale (strukturiert)
  03_validation.json   # Prüfergebnis + Befunde
  04_booking.json      # SKR03-Buchungsvorschlag
  05_report.md         # menschenlesbarer Prüfbericht
```

## Eigene Rechnungen verarbeiten

Lege deine PDFs in `inbox\` (oder nutze `-InboxDir`) und starte `Invoke-ApPipeline.ps1`.
Echte Rechnungen mit Text-Ebene funktionieren direkt; reine Scan-Bilder ohne Textebene
liefern keinen Text (dieses Demo enthält kein OCR).

## Dateien in diesem Beispiel

```
accounts_payable/
  Invoke-ApPipeline.ps1        # Orchestrator (nur PowerShell + agentkit)
  prompts/
    02_extract_fields.md       # System-Prompt: §14-Extraktion → JSON
    03_validate.md             # System-Prompt: Validierung → JSON
    04_book.md                 # System-Prompt: SKR03-Buchung → JSON
    05_report.md               # System-Prompt: Markdown-Report
  tools/
    New-SampleInvoicePdf.ps1   # erzeugt eine gültige PDF (nur PowerShell)
    Build-Samples.ps1          # legt zwei Beispielrechnungen an
  inbox/                       # Eingangs-PDFs (Beispiele)
  out/                         # Ergebnisse (generiert, nicht eingecheckt)
```

## Warum das ein gutes agentkit-Beispiel ist

- **Komposition statt Monolith:** kleine Agenten mit einer Aufgabe, über stdin/stdout verkettet.
- **Format-Treue durch `--format json`:** jede Stufe liefert validiertes JSON, auf das die
  nächste sich verlassen kann (siehe [Unix-Pipe-Kompatibilität](../../README.md#unix-pipe-kompatibilität--agentkit-als-nativer-filter)).
- **Richtiges Werkzeug pro Schritt:** deterministisches `read-pdf` fürs Einlesen, LLM-Agenten
  fürs Urteilen.
- **Nachvollziehbarkeit:** jeder Zwischenschritt liegt als Datei vor — auditierbar und
  wiederholbar.
