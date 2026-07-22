# agent_benchmarks — agentkit gegen öffentliche Benchmarks

Harness, um den Rust-Agent `agentkit` (../agent_framework_rs) gegen drei
öffentliche Agent-Benchmarks zu testen:

| Benchmark | Tasks | Integration | Metrik |
|---|---|---|---|
| **SWE-bench Lite** | 300 GitHub-Issues (Python) | eigener Driver → Patch-JSONL → sb-cli | % resolved |
| **Terminal-Bench 2.0** | 89 Terminal-Aufgaben | [Harbor](https://github.com/laude-institute/harbor)-Adapter | % Tasks gelöst |
| **Aider Polyglot** | 225 Exercism-Aufgaben, 6 Sprachen | derselbe Harbor-Adapter, Dataset `aider-polyglot@1.0` | Pass-Rate |

Der Agent läuft dabei immer **headless in den Task-Containern des jeweiligen
Benchmarks** (`agentkit -p … -y --system-file …`), als statisch gelinktes
musl-Binary — lauffähig in jedem glibc/musl/alpine-Image.

## Voraussetzungen

- Docker, Rust-Toolchain (`rustup`), [uv](https://docs.astral.sh/uv/), x86_64-Host
  (ARM geht per `--platform linux/amd64`-Emulation, nur langsamer)
- API-Zugang: OpenAI, Azure OpenAI **oder** beliebige Modelle via LiteLLM-Proxy

## Schnellstart

```bash
cd agent_benchmarks
cp .env.example .env          # Provider/Keys eintragen
make setup                    # uv sync (Python ≥3.12, holt uv bei Bedarf selbst)
make build-agent              # statisches Binary -> build/agentkit-x86_64-musl

# Optional: beliebige Modelle über LiteLLM (Alias "bench-model" in litellm/config.yaml)
make litellm-up

# Verifikation, billig -> teuer:
make swebench-demo            # Plumbing ohne API-Kosten (--provider demo, 1 Task)
make swebench-gold            # Gold-Patches einreichen -> muss 5/5 resolved ergeben
make tb-hello                 # 1 Harbor-Task Ende-zu-Ende (hello-world@1.0)

# Smoke-Läufe (Defaults: 25 SWE-Tasks, TB-Subset, Polyglot python+rust):
make smoke                    # oder einzeln: swebench-smoke / tb-smoke / polyglot-smoke
make report                   # results/summary.md

# Vollläufe:
make swebench-full tb-full polyglot-full
```

## Provider-Konfiguration

agentkit liest exakt diese Env-Vars (`agent_framework_rs/src/llm.rs`):

| Variante | Env |
|---|---|
| LiteLLM-Proxy | `OPENAI_BASE_URL=http://localhost:4000/v1`, `OPENAI_API_KEY=$LITELLM_MASTER_KEY`, `OPENAI_MODEL=bench-model` |
| direkt OpenAI | `OPENAI_API_KEY`, `OPENAI_MODEL` (Base-URL leer) |
| Azure | `AZURE_OPENAI_ENDPOINT/-API_KEY/-DEPLOYMENT` + `AGENTKIT_PROVIDER=azure` |
| offline | `AGENTKIT_PROVIDER=demo` (Plumbing-Tests, keine Kosten) |

Läuft der Proxy auf dem Host, schreibt `config.py::container_base_url()` die
URL automatisch auf eine container-sichtbare Adresse um (Docker Desktop:
`host.docker.internal`, Linux: Bridge-Gateway). Bei Custom-Netzwerken
`BENCH_CONTAINER_BASE_URL` auf die Host-LAN-IP setzen.

## SWE-bench-Auswertung

Der Driver schreibt `results/swebench/<run_id>/preds.jsonl`
(`{"instance_id", "model_name_or_path", "model_patch"}`). Auswertung:

```bash
# Cloud (empfohlen, kein lokales Eval-Setup): einmalig Key holen
uv run sb-cli gen-api-key <email>
uv run sb-cli submit swe-bench_lite test --predictions_path results/swebench/<run_id>/preds.jsonl --run_id <run_id>
uv run sb-cli get-report swe-bench_lite test <run_id>

# Lokal (x86_64, ~120 GB Disk):
uv sync --extra local-eval
uv run python -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path results/swebench/<run_id>/preds.jsonl \
    --max_workers 8 --run_id <run_id>
```

Der Driver ist **resumierbar** (bereits gelaufene instance_ids in
`preds.jsonl` werden übersprungen) und sammelt Patches auch bei
agentkit-Exit-Code 1 (max-steps) ein — Teilarbeit zählt.

## Wie die Teile zusammenspielen

- `agentkit_bench/harbor_agent.py` — Harbor-`BaseInstalledAgent`: lädt das
  musl-Binary per `upload_file` in den Container (Fallback:
  `AGENTKIT_BINARY_URL`), startet agentkit mit `</dev/null` (agentkit liest
  non-TTY-stdin bis EOF!) und schluckt Exit 1, damit Teilarbeit verifiziert
  wird; API-/Kontextfehler (Exit 2/3) propagieren in Harbors Retry-Logik.
- `agentkit_bench/swebench/` — Driver + Docker-Wrapper für die offiziellen
  Per-Instance-Images (`swebench/sweb.eval.x86_64.<id>`); Diff-Capture via
  `git add -A && git diff --cached`.
- `prompts/benchmark_system_prompt.md` — englischer Zusatz-System-Prompt
  (`--system-file` ist *additiv* zum deutschen Default von agentkit).
- `agentkit_bench/report.py` — sammelt alle Läufe in `results/summary.md`.

## Swarm-Modus: ein Dev-Team statt eines Solo-Agenten

Mit `AGENTKIT_SWARM=1` läuft agentkit in jedem Task-Container als
**Software-Dev-Team** — ein Tech-Lead-Orchestrator delegiert über das
`task`-Tool an Rollen-Sub-Agents (architect → developer → tester ∥ reviewer),
definiert in
[`../agent_framework_rs/examples/coding_swarm`](../agent_framework_rs/examples/coding_swarm/README.md):

```bash
AGENTKIT_SWARM=1 AGENTKIT_MAX_STEPS=160 make swebench-smoke   # oder tb-smoke, …
```

Der Harness lädt dazu die Rollen-`*.md` mit in den Container, hängt die
englischen Team-Instruktionen (`teamlead_bench.md`) an den Benchmark-System-Prompt
an und startet agentkit mit `--agents`. Overrides: `AGENTKIT_SWARM_ROLES=<dir>`
für ein eigenes Team. Der Modus kostet mehr Schritte/Tokens (Delegation läuft
über den Orchestrator) — `AGENTKIT_MAX_STEPS` entsprechend erhöhen. So lassen
sich Solo- und Team-Läufe desselben Modells direkt vergleichen
(`make report` trennt die Runs über ihre run_ids).

## Bekannte Grenzen / Risiken

- **Harbor-API-Drift**: `harbor` ist auf 0.20.0 gepinnt; bei Upgrade
  Adapter-Hooks gegen die installierte Source prüfen.
- **Disk**: jedes SWE-bench-Per-Instance-Image ist ~0,5–1 GB
  (25-Task-Smoke ≈ 15–25 GB) → `--cleanup-images` löscht nach jedem Task.
- **run_shell-Timeout** in agentkit ist 120 s pro Kommando — der
  Benchmark-Prompt weist den Agent deshalb an, nur die engsten Tests zu fahren.
- **Deutsches Prompt-Bleed-Through**: Der deutsche Basis-Prompt bleibt aktiv;
  der englische Zusatz erzwingt englisches Arbeiten. Gescored werden ohnehin
  Artefakte/Tests, nicht Prosa. Sauberer Fix wäre ein `--system-replace`-Flag
  in agentkit (Follow-up).
- **Polyglot-Pass@2**: agentkit wird einmal pro Task aufgerufen; die
  Ergebnisse entsprechen effektiv pass@1.
