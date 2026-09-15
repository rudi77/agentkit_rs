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
stehen — sonst bricht `configure-pages` mit „Get Pages site failed … Not Found"
ab (die Action kann Pages nur mit einem eigenen PAT selbst aktivieren, nicht
mit `GITHUB_TOKEN`). Danach den fehlgeschlagenen Lauf unter *Actions → pages*
per „Re-run" wiederholen oder den Workflow manuell starten. Die Seite liegt
dann unter `https://rudi77.github.io/agentkit_rs/`.

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

Bilder gehören nach `assets/`; Ablaufdiagramme können als `<pre>`-Blöcke mit
einer erklärenden Bildunterschrift eingebunden werden.

## Texte schreiben und überarbeiten

Die öffentliche Dokumentation und der Blog erklären AgentKit anhand konkreter
Aufgaben. Die Website bleibt Englisch; diese Hinweise bleiben Deutsch.

- Mit dem Problem des Lesers beginnen: Was möchte er tun, welches Wissen fehlt
  ihm dafür? Erst den Ablauf erklären, dann interne Modul- und Typnamen nennen.
- Fachbegriffe bei der ersten Verwendung erklären. Beispielsweise ist ein
  Artefakt eine Ergebnisdatei, die ein späterer Arbeitsschritt verwenden kann.
- Ein Beispiel vollständig durchführen: Voraussetzungen, Befehl oder Code,
  erwartetes Ergebnis und dessen Bedeutung. Bash und PowerShell kennzeichnen;
  Platzhalter und benötigte Dateien ausdrücklich benennen.
- Absätze verbinden und jeweils einen Gedanken entwickeln. Tabellen helfen bei
  Entscheidungen; eine Liste interner Merkmale ersetzt keine Erklärung.
- Entscheidungen mit Gründen und Folgen erklären. Werbeformeln, absolute
  Qualitätsversprechen, künstliche Vertraulichkeit und wiederkehrende
  Gegensatz-Slogans vermeiden.
- Implementiertes Verhalten am Code prüfen. Eine Abstimmung beweist keine
  Korrektheit, ein Fake-Modell misst keine Modellqualität und ein Journal macht
  externe Kommandoeffekte nicht rückgängig. Zahlen brauchen eine belegte
  Konfiguration oder Messung.
- Quellen zu AgentKit direkt an den passenden Abschnitt setzen. Bestehende URLs
  und Abschnittsanker erhalten. Nach einer inhaltlichen Blog-Überarbeitung das
  Änderungsdatum ergänzen und Titel sowie Vorschautext in beiden Übersichten
  aktualisieren. Keine ungesicherten Lesezeitangaben oder versprochenen Folgeartikel.

Als redaktionelle Orientierung dienen die schrittweisen technischen Erklärungen
in [The Big LLM Architecture Comparison](https://magazine.sebastianraschka.com/p/the-big-llm-architecture-comparison)
und [The State of LLM Reasoning Model Inference](https://magazine.sebastianraschka.com/p/state-of-llm-reasoning-and-inference-scaling).
AgentKit-Beispiele und Formulierungen bleiben eigenständig; die Artikel sind keine
Quelle für Aussagen über die AgentKit-Implementierung.

Vor der Abgabe alle lokalen Links und Anker prüfen, kopierbare Beispiele gegen
die Implementierung abgleichen und die Seiten in breiter und schmaler
Browseransicht ansehen. Die Website hat weiterhin keinen Build-Schritt.
