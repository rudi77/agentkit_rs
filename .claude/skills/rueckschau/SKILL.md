---
name: rueckschau
description: Eine Arbeitssitzung gemeinsam auswerten — was schiefging, was daraus ins Gedächtnis, in einen Skill, in einen Hook oder ins Repo gehört. Verwenden am Ende eines größeren Arbeitsblocks (Release, mehrere PRs, lange Fehlersuche) oder wenn nach Lehren aus der Sitzung gefragt wird.
---

# Rückschau auf eine Sitzung

Ziel ist, dass ein Umweg dieser Sitzung in der nächsten nicht wieder vorkommt.
Gesucht sind wenige, belegte Punkte. Fünf mit Beleg sind mehr wert als zwanzig
vage.

## 1. Momente sammeln

Die Sitzung der Reihe nach durchgehen und die Stellen notieren, an denen

- ein Ergebnis später korrigiert werden musste (durch den Nutzer, ein Review
  oder die CI),
- ein Werkzeugaufruf scheiterte oder wiederholt werden musste,
- der Nutzer unterbrach, nachfragte oder eine Frage beantworten musste, deren
  Antwort absehbar war,
- eine Frage unbeantwortet liegen blieb.

Zu jedem Moment: was geschah, die Ursache, und was ihn verhindert hätte.

## 2. Zweiter Leser

Dieselbe Analyse von einem Sub-Agenten machen lassen, der die ganze Sitzung
kennt (ein Fork). Sein Auftrag: nichts ändern, streng sein, nicht schmeicheln,
dieselben vier Fächer wie unten. Wo beide Analysen auseinandergehen, steht das
im Bericht.

## 3. Einsortieren

Jede Verhinderung gehört in genau ein Fach:

| Fach | Gehört hinein | Gehört nicht hinein |
|---|---|---|
| Gedächtnis | Vorlieben und Absprachen des Nutzers, Eigenheiten seines Rechners, eigene Arbeitsfehler mit Gegenmittel | was das Repo schon festhält; was nur für diese eine Aufgabe galt |
| Skill | ein Ablauf aus mehreren Schritten, der absehbar wiederkehrt | etwas, das einmal im Quartal vorkommt — dann eher ein Skript im Repo |
| Hook | eine Prüfung, die einen Fehler DIESER Sitzung gefangen hätte | Prüfungen auf Vorrat; den Fehler und die Kosten (Wartezeit, Fehlalarme) nennen |
| Repo | alles, was für jeden Mitwirkenden gilt: `CLAUDE.md`, ein Skript, `.gitattributes`, ein reparierter Test | Angaben zu einem einzelnen Rechner (das Repo ist öffentlich) |

Ein bestehender Skill wird nachgezogen, statt einen zweiten daneben zu legen.
`CLAUDE.md` gegen den Code prüfen, wo die Sitzung den beschriebenen Bereich
geändert hat: veraltete Namen dort führen die nächste Sitzung in die Irre.

## 4. Zusammenarbeit

- Wie viele Antworten endeten mit einer Rückfrage, und wie oft war die Antwort
  ein schlichtes Ja? Daraus einen Vorschlag ableiten, was künftig ohne
  Rückfrage geht. Entscheidungen, die dem Nutzer gehören (Tag, Merge,
  Veröffentlichen, Eingriffe in seine Konfiguration), bleiben bei ihm.
- Musste der Nutzer nach dem Ergebnis fragen? Dann stand das Urteil nicht am
  Anfang des Berichts.
- Was blieb liegen, weil ein neuer Auftrag eine offene Frage überholt hat?

## 5. Umsetzen und berichten

Einträge ins Gedächtnis schreibt Claude selbst und nennt sie. Skills, Hooks
und Änderungen am Repo werden vorgeschlagen, mit einer Empfehlung je Punkt; der
Nutzer entscheidet. Beschlossenes kommt als Pull Request (siehe den Skill
`pr-abschluss`).

Der Bericht beginnt mit der Antwort in einem Satz, dann: die Momente als
Tabelle, was gespeichert wurde, die Vorschläge mit Empfehlung, und am Ende die
Entscheidungen, die der Nutzer treffen muss.
