---
name: pr-abschluss
description: Eine fertige Änderung bis zum gemergten Pull Request bringen — PR anlegen, von unabhängigen Agenten reviewen lassen, Befunde beheben, CI abwarten, in der richtigen Reihenfolge mergen. Verwenden, wenn ein oder mehrere PRs erzeugt, reviewt oder gemergt werden sollen.
---

# Pull Request abschließen

Der Ablauf bringt eine Änderung von „lokal fertig“ bis „auf `main`“. Was davon
der Auftrag deckt, entscheidet der Nutzer: ein Tag, ein Merge nach `main` und
alles, was die Website veröffentlicht, brauchen seinen ausdrücklichen Auftrag.

## 1. Vor dem PR

Diese Fragen stellt später der Reviewer. Sie vorher selbst zu beantworten spart
eine Runde:

- **Was ging bisher, und geht es noch?** Bei jeder Änderung an einer Regel
  (Suche, Auflösung, Reihenfolge, Filter) die Fälle aufschreiben, die vorher
  funktionierten, und prüfen, ob sie sich ändern.
- **Schlägt der neue Test ohne die Änderung fehl?** Und läuft er unter Linux?
  Die CI ist Linux, das Dateisystem dort unterscheidet Groß- und
  Kleinschreibung.
- **Prüfskripte:** einmal mit absichtlich kaputter Eingabe starten. Ein
  Skript, das im Fehlerfall „ok“ meldet, ist schlechter als keines.
- **Öffentliches Repo:** den Diff auf lokale Pfade, Hostnamen und Angaben zum
  Netz des Rechners durchsehen (siehe `CLAUDE.md`).
- **Abhängigkeiten zwischen Branches:** Doku zu einem Feature gehört in
  denselben PR oder wird erst nach dessen Merge geschrieben. Die Website geht
  mit dem Merge nach `main` live.

Dann: `cargo fmt`, Tests und Clippy für jedes berührte Crate (Befehle in
`CLAUDE.md`), bei Änderungen am Agent-Kern auch die abhängigen Crates.

## 2. PR anlegen

- Ein Thema je PR, Branch von `origin/main`.
- Titel und Text deutsch. Der Text hat die Abschnitte „Worum es geht“,
  „Änderung“, „Tests“ und „Nicht geprüft“. Unter „Tests“ steht nur, was
  wirklich gelaufen ist.
- `gh pr create --base main --head <branch>`. Verweist ein PR auf einen
  anderen, die Nummer erst eintragen, wenn es sie gibt.

## 3. Unabhängiges Review

Je Art von Änderung ein frischer Agent (Code, Dokumentation, Skripte), alle
gleichzeitig gestartet. Ein Agent bekommt den Code, nicht die eigene
Einschätzung dazu. In jeden Auftrag gehört:

- **Nur lesen.** Nichts ändern, committen, pushen oder auf GitHub kommentieren.
- **Wie er an den Inhalt kommt:** `git fetch origin`, dann `gh pr diff <N>` und
  `git show origin/<branch>:<pfad>`. Das Arbeitsverzeichnis wechselt
  währenddessen den Branch.
- **Woran er misst:** die Absicht aus `gh pr view <N>`, dazu `CLAUDE.md` und
  `CODING_GUIDELINES.md`; für die Website `website/EDITORIAL_GUIDE.md`.
- **Wonach er sucht**, konkret für diesen PR: Fälle, die vorher gingen;
  übersehene Aufrufpfade; Sicherheitsfolgen; ob die Tests ohne die Änderung
  fehlschlagen; bei Doku jede technische Aussage gegen den Code.
- **Form der Befunde:** Datei und Zeile, was falsch ist, ein konkretes
  Szenario, und ob es durch Lesen oder Ausführen bestätigt ist. Echte Fehler
  getrennt von Kleinigkeiten. Kein Auffüllen.
- **Aufräumen:** Testdateien nur in einem temporären Verzeichnis außerhalb des
  Repos anlegen und am Ende entfernen; was liegen bleibt, im Bericht nennen.

Codex prüft jeden PR von selbst und nach jedem Push erneut. Vor dem Merge beide
Stellen lesen; `gh` setzt `{owner}` und `{repo}` selbst ein:

```
gh api repos/{owner}/{repo}/issues/<N>/comments   # Zusammenfassung: Status, commit_sha, has_any_findings
gh api repos/{owner}/{repo}/pulls/<N>/comments    # die Befunde an den Zeilen
```

Eine leere Liste an der zweiten Stelle heißt nur dann „keine Befunde“, wenn die
Zusammenfassung an der ersten den Stand `completed` für den aktuellen Commit
nennt. Sonst läuft das Review noch.

## 4. Befunde beheben

- Jeden echten Befund beheben oder begründet ablehnen. Zu jedem behobenen
  Fehler ein Test, der ihn gefangen hätte, wo das mit vertretbarem Aufwand geht.
- Ein Befund, der Code auf `main` betrifft und nicht den PR, wird gemeldet und
  nicht nebenbei mitgeändert.
- Nach dem Push die CI abwarten: `gh pr checks <N> --watch`. Erst dann den
  nächsten Branch anfassen.

## 5. Mergen

- Reihenfolge nach Abhängigkeit: zuerst, worauf andere aufbauen; Doku zuletzt.
- Vor jedem Merge `gh pr view <N> --json mergeable` prüfen; nach jedem Merge
  ändert sich die Basis der übrigen.
- `gh pr merge <N> --merge`. Den eigenen PR kann man nicht approven, GitHub
  lehnt das ab.
- Danach `main` holen und die Tests der berührten Crates auf dem gemergten
  Stand laufen lassen; den Lauf der CI auf `main` und, bei Änderungen unter
  `website/`, den Pages-Lauf ansehen.

## 6. Bericht

Zuerst in einem Satz, was gemergt ist. Dann je PR, was das Review gefunden hat
und was daraus wurde, und eine nummerierte Liste dessen, was offen bleibt:
nicht Geprüftes, abgelehnte Befunde, liegengebliebene Branches oder Dateien.
Offene Punkte aus früheren Berichten bleiben in der Liste, bis sie erledigt
sind.
