---
name: windows-eventlog
description: Exportierte Windows-Event-Log-Zeilen lesen — die wichtigsten Event-IDs (Kernel-Power 41, BugCheck 1001, SCM 7000/7001, 4625) und was Routine ist.
---

# Windows-Event-Log (exportierte Zeilen)

> Für eine vollständige Incident-Triage über mehrere Windows-Logs gibt es ein eigenes Beispiel:
> [`examples/win_triage`](../../../win_triage/README.md). Dieser Skill ist die schlanke Variante
> für den Fall, dass exportierte Event-Log-Zeilen im Log-Strom mitlaufen.

## Die Event-IDs, die zählen

| ID | Quelle | Bedeutung |
|---|---|---|
| **41** | Kernel-Power | System ging aus, ohne sauber herunterzufahren. Absturz oder Stromausfall. |
| **1001** | WER-SystemErrorReporting | Bluescreen. **Stopcode und Modulname** sind die eigentliche Information. |
| **6008** | EventLog | Unerwartetes Herunterfahren (bestätigt 41). |
| **7000 / 7001** | Service Control Manager | Dienst startet nicht / hängt von einem anderen ab, der nicht startet. `7001` nennt die **Abhängigkeitskette**. |
| **7031 / 7034** | Service Control Manager | Dienst ist abgestürzt (wiederholt). |
| **129** | storahci / stornvme / Treiber | Gerätereset — oft der **Vorbote** eines Speichertreiber-Absturzes. |
| **4625** | Security-Auditing | Fehlgeschlagene Anmeldung. **Einzeln Alltag, hundertfach ein Angriff.** |
| **4740** | Security-Auditing | Konto gesperrt. |

## Was Rauschen ist (nicht melden)

- **7036** (`Der Dienst … befindet sich jetzt im Status "Wird ausgeführt"`) — der Dienste-Manager
  protokolliert *jeden* Start und Stopp. Davon gibt es immer Dutzende. Bedeutungslos.
- **4624 mit LogonType 5** (Dienst) und Anmeldungen von `NT-AUTORITÄT\SYSTEM` oder dem
  Rechnerkonto (`RECHNER$`) — Grundrauschen jedes Windows-Systems.
- Erfolgreiche Update-Installationen ohne Folgen.

## Vorgehen

1. **Reihenfolge ist Kausalitäts-Verdacht, nicht Kausalität.** Was kurz *vor* einem Absturz
   passierte, ist verdächtig; was *danach* kommt, ist meist Folge.
2. Ein Dienst, der nach einem Neustart nicht hochkommt, ist eine **Folge** des Neustarts.
3. **Nicht alles verknüpfen, was in derselben Nacht passiert.** Ein Anmeldeangriff um 01:00 und
   ein Treiberabsturz um 04:00 sind zwei Vorfälle, keiner.
