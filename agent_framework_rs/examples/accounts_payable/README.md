# Accounts Payable mit agentkit — E-Rechnung, GoBD, DATEV & Dublettenprüfung

Ein praxisnahes Beispiel, wie man mit **agentkit**, der **xcheck-E-Rechnungs-API** und
**reinem PowerShell** einen kleinen, aber vollständigen **Eingangsrechnungs-Prozess**
(Accounts Payable) für deutsche Kleinunternehmer und Freelancer baut — nach dem Unix-Prinzip
*„ein Werkzeug, eine Aufgabe, zusammensteckbar“*.

> ⚠️ **Kein Steuer- oder Rechtsrat.** Ein Demo, das Komposition zeigt — nicht für den
> Produktiveinsatz gedacht.

## Warum das gerade wichtig ist

Seit **01.01.2025** gilt in Deutschland die **E-Rechnungspflicht** im B2B: Unternehmen müssen
strukturierte elektronische Rechnungen (**XRechnung**, **ZUGFeRD/Factur-X** nach **EN 16931**)
empfangen und verarbeiten können. Diese Pipeline deckt beides ab — klassische Papier-/PDF-
Rechnungen **und** E-Rechnungen — und ergänzt die für die Buchhaltung wichtigen Bausteine:
**GoBD-konforme Ablage**, **DATEV-Export** und **Dublettenprüfung**.

## Die Idee: ein Agent (bzw. ein Werkzeug) pro Schritt

Jeder Schritt ist ein eigenständiges, komponierbares Kommando. Die LLM-Schritte sind
spezialisierte agentkit-Agenten, konfiguriert allein über ihren System-Prompt
(`--system-file`); Datenfluss über stdin/stdout. Deterministische Schritte (PDF-Text,
E-Rechnungs-Konformität, DATEV, GoBD, Dublette) sind reine Werkzeuge — kein LLM-Raten, wo
strukturierte Daten oder Rechenregeln genügen.

| # | Stufe | Werkzeug | LLM? | Ergebnis |
|---|-------|----------|------|----------|
| 00 | **Ingest** | Format erkennen, Original GoBD-konform ablegen (schreibgeschützt) | nein | `00_source.*` |
| 01 | **Inhalt** | `agentkit read-pdf` (PDF/ZUGFeRD-Sichtebene) bzw. XML direkt | nein | `01_content.txt` |
| 02 | **E-Rechnung** | **xcheck-API** `POST /validate` (EN 16931 / KoSIT) | nein | `02_einvoice_check.json` |
| 03 | **Extraktion** | agentkit-Agent (§14-Merkmale, Text **oder** XML) | ja | `03_fields.json` |
| 04 | **Validierung** | agentkit-Agent (Arithmetik + §14 + EN-Verdikt + Dublette) | ja | `04_validation.json` |
| 05 | **Buchung** | agentkit-Agent (SKR03; blockiert bei Fehler/Dublette) | ja | `05_booking.json` |
| 06 | **DATEV** | Buchungsstapel EXTF-CSV | nein | `06_datev.csv` |
| 07 | **Report** | agentkit-Agent (Markdown) | ja | `07_report.md` |
| — | **GoBD** | SHA-256-Manifest über alle Artefakte | nein | `manifest.json` |

Jede Stufe ist testbar, austauschbar und wiederverwendbar. Buchungslogik von SKR03 auf SKR04
umstellen? Ein System-Prompt tauschen. Ein anderer E-Rechnungs-Validator? Nur Stufe 02.

### Die vier Eingangsformate

Die Ingest-Stufe erkennt das Format rein per PowerShell (Endung + Byte-Scan auf
`/EmbeddedFile`) und routet entsprechend:

- **Papier-/PDF-Rechnung** (`.pdf`) → Text via `read-pdf`, keine EN-Prüfung.
- **XRechnung** (`.xml`, UBL/CII) → XML ist die Wahrheit; EN-Prüfung via xcheck.
- **ZUGFeRD/Factur-X** (`.pdf` mit eingebettetem XML) → Sichtebene via `read-pdf`, EN-Prüfung
  des eingebetteten XML via xcheck.

### E-Rechnungs-Prüfung über die xcheck-API

Für E-Rechnungen ruft die Pipeline die **xcheck-API** (`InvoicePort`) auf — ein separater
.NET-Dienst, der XRechnung/ZUGFeRD gegen **EN 16931** validiert (offizieller **KoSIT**-
Validator). Die Antwort (`isValid`, `formatDetected`, `syntaxValid`, `semanticErrors[]`) fließt
in Validierung und Report ein. Die Anbindung ist **konfigurierbar und degradiert sauber**: ohne
`XCheckUrl`/`XCheckApiKey` wird die E-Rechnungs-Prüfung übersprungen (der Rest läuft weiter).

### Was geprüft wird (§ 14 UStG)

Pflichtangaben nach **§ 14 Abs. 4 UStG** (Parteien, Steuernummer/USt-IdNr., Rechnungsnummer,
Ausstellungs-/Leistungsdatum, Menge/Art, nach Steuersätzen aufgeschlüsseltes Entgelt, Satz,
Steuerbetrag), Arithmetik (Netto + USt = Brutto, Steuer = Netto × Satz) und Sonderfälle:
**Kleinunternehmer § 19**, **Reverse-Charge § 13b**, **Kleinbetragsrechnung § 33 UStDV**.

### GoBD, DATEV, Dublettenprüfung

- **GoBD:** Das Original wird unverändert und **schreibgeschützt** abgelegt; `manifest.json`
  hält **SHA-256** je Artefakt (Unveränderbarkeit) und einen Aufbewahrungshinweis (10 Jahre).
- **DATEV:** Aus dem Buchungsvorschlag entsteht ein **DATEV-EXTF-Buchungsstapel** je Rechnung
  (`06_datev.csv`) sowie ein **Sammelstapel** (`out/datev_buchungsstapel.csv`) über alle
  buchbaren Rechnungen — Übergabe an den Steuerberater. (Vereinfachtes Demo-Format.)
- **Dublettenprüfung:** Ein Register (`out/_register.json`) verhindert Doppelbuchungen anhand
  von Rechnungsnummer + Lieferant + Bruttobetrag. Eine erkannte Dublette wird als `fehler`
  markiert und **nicht erneut gebucht**.

## Voraussetzungen

1. **agentkit mit PDF-Support** (Feature `pdf`):

   ```powershell
   # im Repo-Ordner agent_framework_rs
   cargo build --release --bin agentkit --features pdf
   ```

2. **LLM-Credentials.** Die Pipeline lädt automatisch eine `.env` (neben dem Skript oder
   `agent_framework_rs\.env`) und wählt den Provider per `auto`:
   - **Azure:** `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_DEPLOYMENT`
   - **OpenAI:** `OPENAI_API_KEY` (optional `OPENAI_MODEL`)

3. **xcheck-API** (optional, aber empfohlen für die E-Rechnungs-Prüfung). Das Repo liegt unter
   [`../../../xcheck`](../../../xcheck). Lokaler Start (Postgres + KoSIT via Docker, API per
   `dotnet run`):

   ```bash
   cd xcheck
   docker compose -f docker-compose.dev.yml up -d --build      # Postgres + KoSIT-Validator
   export ConnectionStrings__Postgres="Host=localhost;Database=invoiceport;Username=invoiceuser;Password=dev"
   export Kosit__BaseUrl="http://localhost:8080"  Stripe__WebhookSecret="whsec_localtest"
   dotnet run --project src/InvoicePort.Api --urls http://localhost:5080 &
   CREDITS=1000 ./scripts/seed-tenant.sh          # legt Tenant + API-Key an (Key wird ausgegeben)
   ```

   Den ausgegebenen `inv_port_…`-Key an die Pipeline geben (Parameter oder Env `XCHECK_API_KEY`).

## Ausführen

```powershell
# 1) Beispielrechnungen erzeugen (Papier-PDF, Mängel-PDF, XRechnung-XML, ZUGFeRD-PDF):
.\tools\Build-Samples.ps1

# 2) Pipeline über alle Rechnungen in .\inbox — mit E-Rechnungs-Prüfung via xcheck:
.\Invoke-ApPipeline.ps1 -XCheckUrl 'http://localhost:5080' -XCheckApiKey 'inv_port_…'

# ohne xcheck (E-Rechnungs-Prüfung wird übersprungen, alles andere läuft):
.\Invoke-ApPipeline.ps1
```

Ergebnis: pro Rechnung ein Ordner `out\<name>\` mit allen Zwischen- und Endergebnissen, plus
`out\datev_buchungsstapel.csv` und `out\_register.json`. Eine Zusammenfassungstabelle zeigt je
Rechnung Format, EN-16931-Status, Gesamtstatus und Buchbarkeit.

## Tests

```powershell
# Deterministische Offline-Tests der Helfer (Klassifizierung, DATEV, GoBD, Dublette) — kein Netz:
pwsh -File .\tests\Test-ApHelpers.ps1

# Live-Integrationstest gegen die xcheck-API (übersprungen, wenn nicht konfiguriert):
$env:XCHECK_URL='http://localhost:5080'; $env:XCHECK_API_KEY='inv_port_…'
pwsh -File .\tests\Test-XCheckIntegration.ps1
```

## Dateien

```
accounts_payable/
  Invoke-ApPipeline.ps1        # Orchestrator (PowerShell + agentkit + xcheck-API)
  modules/ap-helpers.ps1       # Klassifizierung, xcheck-Aufruf, GoBD, DATEV, Dublette
  prompts/                     # je LLM-Stufe ein System-Prompt (Extraktion/Validierung/Buchung/Report)
  tools/
    New-SampleInvoicePdf.ps1   # erzeugt PDF bzw. ZUGFeRD (mit eingebettetem XML) — nur PowerShell
    Build-Samples.ps1          # legt die vier Beispielrechnungen an
    zugferd-embed.xml          # konformes CII-XML, das in die ZUGFeRD-PDF eingebettet wird
  tests/
    Test-ApHelpers.ps1         # Offline-Unit-Tests
    Test-XCheckIntegration.ps1 # opt-in Live-Test der xcheck-Anbindung
  inbox/                       # Eingangsrechnungen (Beispiele: PDF, XML, ZUGFeRD)
  out/                         # Ergebnisse (generiert, nicht eingecheckt)
```

## Warum das ein gutes agentkit-Beispiel ist

- **Komposition statt Monolith:** kleine, einzweckige Schritte über stdin/stdout verkettet.
- **Richtiges Werkzeug pro Schritt:** deterministische Werkzeuge (read-pdf, xcheck, DATEV,
  GoBD, Dublette) fürs Faktische, LLM-Agenten fürs Urteilen — statt eines „Mach-alles“-Prompts.
- **Format-Treue durch `--format json`:** jede Stufe liefert validiertes JSON für die nächste.
- **Fremd-Dienst-Integration:** die xcheck-E-Rechnungs-API wird als komponierbarer Baustein
  eingebunden (und ist zugleich ein eigenständiges, verkaufbares Produkt).
- **Nachvollziehbarkeit:** jeder Zwischenschritt liegt als Datei vor — auditierbar, GoBD-nah.
