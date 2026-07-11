# Interaktiver Accounts-Payable-Orchestrator — Human-in-the-Loop & lernender Wissensgraph

Die **interaktive** Variante des [Accounts-Payable-Demos](../accounts_payable/README.md): Statt
einer Batch-Pipeline führt hier ein **Orchestrator-Agent** — *Frau Berger, die Leiterin der
Buchhaltung* — ein Team von Fach-Agenten, **redet mit dir** (Human-in-the-Loop), **fragt bei
Unklarheiten nach** und baut dabei einen **Company Knowledge Graph im OKF-Format** auf. Beim
nächsten Mal weiß die Buchhaltung Bescheid — sie **lernt dazu**.

> ⚠️ Kein Steuer-/Rechtsrat — ein Demo, das interaktive Orchestrierung, HITL und einen
> lernenden Wissensgraph zeigt.

## Die Idee

```
                                 ┌──────────────────────────────────────────┐
   Du (Mensch)  ◀── ask_user ──▶ │  Orchestrator: „Leiterin der Buchhaltung“ │
        ▲                        │  plant · sucht im Wissensgraph · fragt    │
        │  Bericht               │  nach · lernt · protokolliert             │
        │                        └───────┬───────────────┬───────────────┬───┘
        │                          task  │          task │          task │
        │                          ▼     │          ▼    │          ▼    │
        │                    ┌───────────┐   ┌───────────┐   ┌───────────┐
        │                    │ extractor │   │ validator │   │  booker   │
        │                    └───────────┘   └───────────┘   └───────────┘
        │
        └── liest/schreibt ──▶  knowledge/  (OKF-Wissensgraph: Lieferanten,
                                Kostenstellen, Personen, Rechnungen)
```

- **Orchestrator statt Pipeline:** Ein Leit-Agent managt die Fach-Agenten (`extractor`,
  `validator`, `booker`) über das `task`-Werkzeug — dieselben Aufgaben wie im Batch-Demo, jetzt
  als delegierbare Rollen.
- **Human-in-the-Loop:** Bei fehlendem Firmenwissen (unbekannter Lieferant, Kostenstelle,
  Freigabe-Verantwortliche) hält der Orchestrator inne und **fragt dich** über das neue
  agentkit-Werkzeug **`ask_user`** — mitten in der Aufgabe, nicht am Ende.
- **Lernen:** Deine Antwort wird als **OKF-Entität** dauerhaft im Wissensgraph gespeichert.
  Kommt der Lieferant wieder, entscheidet der Orchestrator ohne Rückfrage.

## Der Company Knowledge Graph (OKF)

`knowledge/` ist die Wissensbasis im **[Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf)**:
je Entität **eine Markdown-Datei** mit YAML-Frontmatter (die wenigen Felder, auf die man
filtert) und einem Markdown-Body; Beziehungen als Wiki-Links `[[pfad/slug]]` — der Graph ist
netz-, nicht baumförmig. Typen: `lieferant`, `kostenstelle`, `person`, `rechnung` (Details in
[`knowledge/index.md`](knowledge/index.md)). Beispiel eines gelernten Lieferanten:

```markdown
---
type: lieferant
id: LIEF-002
name: Bürobedarf Meier GmbH
ust_idnr: DE255558888
standard_kostenstelle: KST-4900
standard_konto_skr03: "4930"
freigabe_verantwortliche: PER-002
tags: [lieferant, buerobedarf]
status: aktiv
erfasst_am: 2026-07-11
---
# Bürobedarf Meier GmbH
- **Standard-Kostenstelle:** [[kostenstellen/kst-4900-verwaltung]]
- **Standard-Aufwandskonto (SKR03):** 4930 — Büromaterial
- **Freigabe-Verantwortliche:** [[personen/stefan-klein]]
## Verarbeitete Rechnungen
- [[rechnungen/BM-2025-3311]]
```

## Neu in agentkit: das `ask_user`-Werkzeug

Damit ein Agent **mitten in der Aufgabe** eine Rückfrage stellen kann, hat agentkit jetzt das
Werkzeug **`ask_user`** (nur der Haupt-/Orchestrator-Agent hat es; Sub-Agenten melden
Unklarheiten an ihn zurück). Es wirkt im **REPL** (Antwort über stdin) und im **TUI**
(Eingabedialog). In einer nicht-interaktiven Pipe liefert es eine Sentinel-Antwort, damit nichts
blockiert. Ergänzend macht `--repl` die interaktive Session **scriptbar** (Kommandos und
Rückfrage-Antworten von stdin) — praktisch für Automatisierung und Tests.

## Voraussetzungen

- agentkit mit TUI + PDF: `cargo build --release --bin agentkit --features "tui pdf"` (im Ordner
  `agent_framework_rs`).
- LLM-Credentials wie im Batch-Demo (`.env` mit `AZURE_OPENAI_*` bzw. `OPENAI_API_KEY`).

## Starten (TUI)

```powershell
.\Start-ApOrchestrator.ps1          # baut Arbeitsordner aus dem Seed, startet das TUI
.\Start-ApOrchestrator.ps1 -Fresh   # Gelerntes verwerfen und neu aus dem Seed aufsetzen
```

Dann im TUI z. B. eintippen:

```
Verarbeite die Eingangsrechnung inbox/rechnung_meier.txt und melde mir das Ergebnis.
```

Der Orchestrator extrahiert, sucht den Lieferanten im Graph — und da **Bürobedarf Meier**
unbekannt ist, **fragt er dich** nach Kostenstelle, Konto und Freigabe-Verantwortlicher
(Antwort im Eingabefeld, Enter). Danach legt er die OKF-Entitäten an, bucht und berichtet.
Beim zweiten Lauf mit demselben Lieferanten fragt er **nicht** mehr.

Der Seed enthält bereits den **bekannten** Lieferanten *Tischlerei Thomas Berg* — verarbeite
`inbox/rechnung_berg.txt`, um den „kein-Nachfragen“-Fall zu sehen.

## Wie es funktioniert (Ablauf je Rechnung)

1. `extractor` liest die Rechnung → §14-Merkmale (JSON).
2. Orchestrator sucht den Lieferanten im Wissensgraph (Name/USt-IdNr).
3. **Bekannt** → Kontierung aus der Entität. **Unbekannt** → **`ask_user`** (eine gebündelte Frage).
4. Bei neuem Wissen: OKF-Entitäten anlegen/verknüpfen (`write_file`/`edit_file`) — **lernen**.
5. `validator` prüft, `booker` erstellt den SKR03-Buchungssatz.
6. Rechnung als `rechnung`-Entität protokollieren, Lieferant verlinken.
7. Ergebnis an den Menschen berichten.

## Tests

```powershell
# Scriptbarer End-to-End-Test des HITL-Lernpfads (echtes Modell nötig; nutzt --repl):
pwsh -File .\tests\Test-Interactive.ps1
```

Der Test setzt einen frischen Arbeitsordner auf, verarbeitet die Rechnung eines **unbekannten**
Lieferanten mit einer gescripteten Antwort und prüft, dass (a) eine Rückfrage gestellt wurde
und (b) danach eine neue Lieferanten-Entität im Wissensgraph steht (Lernen).

## Dateien

```
accounts_payable_interactive/
  orchestrator.md              # System-Prompt der „Leiterin der Buchhaltung“
  roles/                       # Sub-Agenten-Rollen: extractor, validator, booker
  knowledge/                   # OKF-Wissensgraph (Seed): index + Lieferant/Kostenstelle/Person
  inbox/                       # Beispielrechnungen (bekannter + unbekannter Lieferant)
  Start-ApOrchestrator.ps1     # Launcher (TUI/REPL); baut Arbeitsordner aus dem Seed
  tests/Test-Interactive.ps1   # scriptbarer HITL-/Lern-Test
  workspace/                   # Arbeitsordner (generiert, nicht eingecheckt) — hier wächst der Graph
```

## Bezug zum Rest

- Fachlogik (Extraktion/Validierung/Buchung) = wie im [Batch-Demo](../accounts_payable/README.md);
  hier als delegierbare Rollen statt Pipe-Stufen.
- `ask_user` und `--repl` sind allgemeine agentkit-Fähigkeiten (siehe
  [Benutzerhandbuch](../../docs/USER_MANUAL.md)) — nicht auf Accounts Payable beschränkt.
