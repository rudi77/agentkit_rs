---
name: iis-logs
description: IIS-Zugriffslogs im W3C-Format lesen (u_ex*.log) — Feldaufbau, typisches Rauschen, echte Alarme (5xx, sc-win32-status, Traversal-Versuche, Latenz).
---

# IIS-Zugriffslogs (W3C Extended)

## Feldaufbau

Die `#Fields:`-Zeile im Kopf definiert die Spalten. Üblich:

```
date time s-ip cs-method cs-uri-stem cs-uri-query s-port cs-username c-ip cs(User-Agent) cs(Referer) sc-status sc-substatus sc-win32-status time-taken
```

Die drei Spalten, auf die es ankommt:

- **`sc-status`** — der HTTP-Status (200, 404, 500 …).
- **`sc-substatus`** — die Feindiagnose. `500.19` = Konfigurationsfehler (`web.config`),
  `500.0` = Anwendungsfehler, `401.3` = ACL/NTFS-Rechte, `404.0` = nicht gefunden.
  **Ohne den Substatus ist eine 500 nur eine Zahl** — mit ihm ist sie eine Diagnose.
- **`time-taken`** — Millisekunden. Der Wert, der einen Ausfall *ankündigt*, bevor er passiert.

## Was Rauschen ist (nicht melden)

- **`404` auf `/favicon.ico`, `/robots.txt`, `/apple-touch-icon*`** — Browser und Crawler fragen
  das ungefragt ab. Auf jedem Webserver der Welt. Kein Befund.
- **`404` von Scannern auf `/wp-login.php`, `/.env`, `/phpmyadmin`** — Internet-Grundrauschen,
  solange der Server gar kein WordPress/PHP ausliefert. Erwähnenswert nur, wenn es **massiv**
  wird oder wenn eine dieser Anfragen plötzlich **200** liefert.
- **`401` gefolgt von `200`** desselben Clients — das ist der normale Ablauf einer
  Authentifizierung (Challenge → Antwort), kein Fehlversuch.
- **`304 Not Modified`** — Caching funktioniert. Gut so.

## Was ein echter Alarm ist

| Muster | Warum |
|---|---|
| **`5xx` gehäuft auf demselben `cs-uri-stem`** | Die Anwendung ist kaputt, nicht der Client. Der wichtigste Befund überhaupt. Nenne **Substatus** und **Anzahl**. |
| **`500.19`** | Die Anwendung startet gar nicht — `web.config` oder Modul defekt. Betrifft *alles* darunter. |
| **`sc-win32-status` ≠ 0** | Der Windows-Fehlercode dahinter. `1236` = Verbindung abgebrochen, `64` = Netzwerkname weg, `2` = Datei fehlt. Verrät oft mehr als der HTTP-Status. |
| **`time-taken` steigt über Minuten** | Ein Ausfall mit Vorlauf (Thread-Pool, Datenbank, GC). Sag: von *welchem* Wert auf *welchen*. |
| **`..%2f`, `../`, `%00`, `/etc/passwd`, `cmd.exe` in `cs-uri-stem`/`cs-uri-query`** | **Traversal-/Injection-Versuch.** Immer melden — und **prüfen, ob er 200 bekam.** `404` = abgewehrt; `200` = Vorfall. |
| **`401` in Serie ohne folgendes `200`** | Anmeldeversuche, die scheitern. Menge + Quell-IP nennen. |

## Vorgehen

1. Gruppiere nach `cs-uri-stem` + `sc-status` — **nicht** Zeile für Zeile denken. Eine kaputte
   Route erzeugt hunderte identische Zeilen; das ist **ein** Befund mit einer Anzahl.
2. Nenne bei jedem Befund: **Route, Status(+Substatus), Anzahl, Zeitfenster, Quell-IP** (falls
   relevant) und die entscheidende Rohzeile als Beleg.
3. Bei Sicherheitsbefunden **immer** dazusagen, ob der Versuch **erfolgreich** war (Status 2xx)
   oder abgewehrt (4xx). Das ist der Unterschied zwischen „Notiz" und „heute Nacht aufstehen".
