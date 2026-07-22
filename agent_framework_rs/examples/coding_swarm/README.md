# Coding-Swarm — ein Software-Dev-Team als Agent-Schwarm

Dieses Beispiel zeigt, wie man mit **agentkit** ein ganzes Software-Entwicklungsteam
baut, das Coding-Aufgaben gemeinsam löst — vom lokalen Bugfix bis zu den Benchmarks
aus [`../../../agent_benchmarks`](../../../agent_benchmarks/README.md) (SWE-bench Lite,
Terminal-Bench 2.0, Aider Polyglot).

Das Team:

| Rolle | Datei | Zugriff | Aufgabe |
|---|---|---|---|
| **Tech-Lead** (Orchestrator) | `teamlead.md` | `task`-Tool | zerlegt, delegiert, iteriert, baut zusammen |
| **architect** | `roles/architect.md` | read-only, Plan-Strategie | Ursache lokalisieren, minimalen Fix-Plan liefern |
| **developer** | `roles/developer.md` | voll (write/edit/shell) | genau EINEN umrissenen Auftrag umsetzen + selbst verifizieren |
| **tester** | `roles/tester.md` | read + `run_shell` + `git_diff` | engste relevante Tests fahren, Pass/Fail berichten |
| **reviewer** | `roles/reviewer.md` | read-only, Plan-Strategie | Diff gegen den Auftrag reviewen (Korrektheit, Minimalität) |

Der Ablauf, den der Tech-Lead fährt: **architect → developer → (tester ∥ reviewer) →
ggf. Nacharbeit → Abschlussbericht**. tester und reviewer werden in EINER Antwort
delegiert (zwei `task`-Aufrufe) und laufen dadurch **parallel** — gefahrlos, weil beide
read-only sind.

## Kein neuer Framework-Code

Der Schwarm besteht **komplett aus Daten**: Markdown-Rollen + ein System-Prompt.
Möglich machen das zwei vorhandene Bausteine:

- Das **`task`-Tool** (`src/roles.rs`): der Orchestrator delegiert eine Mission an einen
  frischen Sub-Agenten mit eigenem Kontext und der Tool-Teilmenge seiner Rolle. Mehrere
  `task`-Aufrufe aus einer Antwort laufen parallel; alle Events landen — getaggt mit
  `source` — im selben EventBus.
- **Custom-Rollen als `*.md`** (`--agents DIR`): Frontmatter = Metadaten
  (`name`/`description`/`tools`/`strategy`), Body = System-Prompt. Gleichnamige Rollen
  **überschreiben** die Builtins — `roles/tester.md` und `roles/reviewer.md` ersetzen
  hier die generischen Presets durch team-spezifische.

Die `description` jeder Rolle wandert ins `task`-Schema — sie ist die Stellenbeschreibung,
nach der das Orchestrator-LLM entscheidet, wen es beauftragt.

## Ausführen

**Offline-Demo** (FakeLlm, kein Netz, kein Key) — zeigt die komplette Verdrahtung
inklusive echtem Datei-Fix und parallelem tester/reviewer-Lauf:

```bash
cargo run --example coding_swarm --no-default-features
```

**Echter Lauf** auf einem beliebigen Repo (Credentials wie üblich via `.env`/Env):

```bash
agentkit -w /pfad/zum/repo \
  --agents examples/coding_swarm/roles \
  --system-file examples/coding_swarm/teamlead.md \
  "Behebe den Bug: … (Issue-Text)"
```

Headless (Pipeline/CI) zusätzlich `-p -y` und ggf. `--max-steps 160` — Delegation
kostet Schritte des Orchestrators, das Budget sollte großzügiger sein als für einen
Solo-Agenten. Mit `--steps` (ohne `-p`) sieht man den Live-Trace aller Teammitglieder.

## Gegen die Benchmarks

Der Benchmark-Harness in `agent_benchmarks/` kann denselben Schwarm headless in den
Task-Containern fahren — eine Env-Variable genügt:

```bash
cd agent_benchmarks
AGENTKIT_SWARM=1 make swebench-smoke     # oder tb-smoke, polyglot-smoke, …
```

Der Harness lädt dann `roles/` mit in den Container, hängt `teamlead_bench.md`
(englische Team-Instruktionen) an den Benchmark-System-Prompt an und startet agentkit
mit `--agents`. Details: [`agent_benchmarks/README.md`](../../../agent_benchmarks/README.md).
Empfehlung: `AGENTKIT_MAX_STEPS` erhöhen (z. B. 160), da der Orchestrator seine
Schritte auf Delegationen verwendet.

## Muss es Orchestrator + Sub-Agents sein?

Nein — agentkit kennt drei Formen von Mehr-Agenten-Arbeit, mit unterschiedlichen
Trade-offs:

1. **Orchestrator + Rollen-Sub-Agents** (dieses Beispiel): Ein LLM entscheidet
   *dynamisch*, wer wann was tut, und iteriert auf Befunde. Richtig, wenn der
   Lösungsweg vorher unbekannt ist — genau der Fall bei SWE-bench & Co. Kosten:
   mehr Schritte/Tokens, und die Qualität der Delegationen hängt am Orchestrator.
2. **Deterministische Pipeline** (wie der Batch-Modus von
   [`../accounts_payable`](../accounts_payable/README.md)): eine feste Stufenfolge aus
   einzelnen `agentkit`-Aufrufen (`analyse | fix | test | review`), verkettet über
   stdin/stdout und `--format json`. Richtig, wenn der Ablauf immer gleich ist —
   billiger, reproduzierbar, CI-freundlich. Aber: keine dynamische Iteration; ob
   nachgebessert werden muss, entscheidet Shell-Logik statt ein LLM.
3. **Fester Peer-Schwarm in Rust** (`add_subagent`, siehe
   [`../parallel_subagents.rs`](../parallel_subagents.rs)): jedes Teammitglied als
   eigenes Tool mit eigenem LLM/Toolset fest verdrahtet — maximale Kontrolle (z. B.
   billigeres Modell für den tester), dafür Code statt Daten.

Die Formen kombinieren sich: Der Tech-Lead ist hier bewusst die dynamische Variante,
weil Benchmark-Aufgaben heterogen sind; für einen festen Nightly-Job wäre Variante 2
die bessere Wahl. Bewusste Grenze des Frameworks bleibt in allen Varianten: Sub-Agenten
bekommen **kein** `task`-Tool (genau eine Ebene, keine Rekursion) und teilen sich den
EINEN Workspace — deshalb schreibt in diesem Team immer nur ein Agent zur Zeit, und
parallelisiert wird nur Lesendes.

## Dateien

```
coding_swarm/
  README.md            # dieses Dokument
  teamlead.md          # System-Prompt des Orchestrators (--system-file), deutsch
  teamlead_bench.md    # englische Team-Instruktionen für den Benchmark-Harness (additiv)
  roles/               # das Team als Custom-Rollen (--agents)
    architect.md · developer.md · tester.md · reviewer.md
  demo.rs              # Offline-Demo (cargo run --example coding_swarm)
```
