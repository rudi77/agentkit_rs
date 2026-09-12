# Produkt-Website

Die öffentliche Website zu agentkit — reines HTML und CSS, **ohne Build-Schritt**,
gehostet über GitHub Pages. Der Inhalt ist bewusst Englisch (Zielgruppe: die
allgemeine Öffentlichkeit); diese Notiz für Mitwirkende bleibt deutsch wie der
Rest des Repos.

```text
website/
  index.html            Startseite
  features.html         Funktionsumfang
  architecture.html     Architektur (Kurzfassung der READMEs und CLAUDE.md)
  principles.html       Design-Prinzipien (aus CODING_GUIDELINES.md)
  get-started.html      Installation und erste Schritte
  blog/index.html       Blog-Übersicht
  blog/<slug>.html      je ein Beitrag
  assets/style.css      das eine Stylesheet (hell/dunkel per prefers-color-scheme)
  assets/site.js        nur das Menü für schmale Bildschirme
  .nojekyll             GitHub Pages soll nichts vorverarbeiten
```

## Veröffentlichen

`.github/workflows/pages.yml` lädt `website/` bei jedem Push nach `main`, der
dieses Verzeichnis berührt, als Pages-Artefakt hoch (oder manuell per
`workflow_dispatch`). Einmalig muss in den Repo-Einstellungen unter
**Settings → Pages → Build and deployment** die Quelle auf **„GitHub Actions"**
stehen. Die Seite liegt dann unter `https://rudi77.github.io/agentkit_rs/`.

Alle Links sind relativ, damit die Seite unter dem Projekt-Unterpfad
funktioniert — keine Links mit führendem `/`.

## Einen Blog-Beitrag hinzufügen

1. `blog/<slug>.html` anlegen — am einfachsten den vorhandenen Beitrag kopieren
   und Kopf (`<title>`, `description`), Meta-Zeile und Inhalt ersetzen.
2. In `blog/index.html` und auf der Startseite (`index.html`, Abschnitt
   „From the blog") einen Listeneintrag ergänzen; die Liste ist nach Datum
   absteigend sortiert.
3. Lokal prüfen: `python3 -m http.server -d website 8000` und
   `http://localhost:8000/` öffnen.

Bilder gehören nach `assets/`; Diagramme sind bewusst `<pre>`-Blöcke wie in den
READMEs, damit sie ohne Werkzeuge gepflegt werden können.
