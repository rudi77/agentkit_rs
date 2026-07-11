---
type: index
title: Company Knowledge Graph — Buchhaltung
format: OKF
tags: [knowledge-graph, buchhaltung]
---

# Company Knowledge Graph (OKF)

Die Wissensbasis der Buchhaltung im **Open Knowledge Format (OKF)**: jede Entität ist **eine
Markdown-Datei** mit YAML-Frontmatter (die wenigen Felder, auf die man filtert/indiziert) und
einem Markdown-Body (Prosa + Beziehungen). Beziehungen werden als **Wiki-Links** `[[pfad/slug]]`
ausgedrückt — der Graph ist damit netz-, nicht nur baumförmig.

## Entitätstypen (Verzeichnisse)

| Typ | Ordner | Zweck |
|---|---|---|
| `lieferant` | `lieferanten/` | Lieferant mit Standard-Kostenstelle, Freigabe-Verantwortlicher, Standard-Aufwandskonto (SKR03) |
| `kostenstelle` | `kostenstellen/` | Kostenstelle (Nummer + Name) |
| `person` | `personen/` | Verantwortliche, v. a. für die Rechnungsfreigabe |
| `rechnung` | `rechnungen/` | verarbeitete Eingangsrechnung (Link zu Lieferant, Beleg, Buchung) |

## Pflichtfelder je Frontmatter

- alle: `type`, `id`, `name`, `tags`, `erfasst_am`
- `lieferant`: zusätzlich `ust_idnr` (falls bekannt)

## Konventionen

- Dateiname = kebab-case-Slug des Namens (z. B. `tischlerei-thomas-berg.md`).
- Neue Erkenntnisse (nach Rückfrage beim Menschen) werden als neue/aktualisierte Entität
  gespeichert — so **lernt** die Buchhaltung dazu.
