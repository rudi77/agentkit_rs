# Benchmark-Report — agentkit 0.14.0

**Datum:** 2026-07-22 · **Modell:** Azure OpenAI, Deployment `agentkit-gpt-5.4-mini` · **Modus:** Solo (kein Swarm) · **max_steps:** 100 · **Binary:** statisches `agentkit-x86_64-musl` (4,1 MB)

Smoke-Läufe gemäß `make smoke` (25 SWE-bench-Tasks, Terminal-Bench-Subset, Polyglot python+rust). Rohdaten liegen unter `BENCH_RESULTS_DIR` (`D:\agent_bench_data\results`, nicht im Repo).

## Ergebnisse (Smoke, 2026-07-22)

| Benchmark | Run | Tasks | Ergebnis | Laufzeit |
|---|---|---|---|---|
| SWE-bench Lite | `smoke-20260722-205659` | 25 | **Patches: 25/25** (0 leer, 0 Fehler) — resolved-Rate noch offen, s. u. | ~7 min |
| Terminal-Bench 2.0 (Subset) | `smoke-20260722-210357` | 7 | **2/7 gelöst** (Mean-Reward 0,286), 0 Exceptions | 6 min |
| Aider Polyglot (py+rs) | `smoke-20260722-211052` | 64 | **58/64 gelöst** (90,6 %), 0 Exceptions | 24 min |

### SWE-bench Lite

Alle 25 Instanzen lieferten einen nicht-leeren Patch (astropy + django). Die
**resolved-Rate ist noch nicht ausgewertet**: Die Cloud-Auswertung braucht
einen `SWEBENCH_API_KEY` (E-Mail-Verifizierung nötig):

```bash
uv run sb-cli gen-api-key <email>   # Key in .env als SWEBENCH_API_KEY eintragen
uv run --env-file .env sb-cli submit swe-bench_lite test \
    --predictions_path <BENCH_RESULTS_DIR>/swebench/smoke-20260722-205659/preds.jsonl \
    --run_id smoke-20260722-205659
uv run --env-file .env sb-cli get-report swe-bench_lite test smoke-20260722-205659
```

### Terminal-Bench 2.0 (Subset `build-* git-* regex-* hello-* csv-*`)

| Task | Ergebnis |
|---|---|
| build-pmars, build-pov-ray | ✅ gelöst |
| build-cython-ext, git-leak-recovery, git-multibranch, regex-chess, regex-log | ❌ |

### Aider Polyglot (pass@1)

| Sprache | gelöst |
|---|---|
| Python | 29/34 (85 %) |
| Rust | 29/30 (97 %) |

Gescheitert: `paasio`, `scale-generator`, `simple-linked-list`, `transpose`,
`variable-length-quantity` (Python); `react` (Rust).

## Vergleich mit früheren Läufen (2026-07-21, gleiches Modell)

| Run | Terminal-Bench (7 Tasks) | Polyglot (64 Tasks) |
|---|---|---|
| Erstlauf (`*-20260721-162842`) | 1/7 (0,143) | 26/64 (0,406) |
| Swarm-Modus (`tb-swarm` / `poly-swarm`) | 1/7 (0,143) | 54/64 (0,844) |
| Solo v2 (`tb-v2` / `poly-v2`) | 2/7 (0,286) | 55/64 (0,859) |
| tb-v3 / tb-v4 | 2/7 (0,286) | — |
| tb-v5 | **4/7 (0,571)** | — |
| **Smoke 2026-07-22 (dieser Report)** | 2/7 (0,286) | **58/64 (0,906)** |

Terminal-Bench streut bei nur 7 Tasks stark (1–4 gelöst über identische
Konfigurationen); Polyglot ist mit 64 Tasks deutlich stabiler und hat sich
gegenüber dem Erstlauf von 41 % auf 91 % verbessert. Der Swarm-Modus brachte
bei Polyglot keinen Vorteil gegenüber Solo.

## Reproduktion

```bash
cd agent_benchmarks
make setup build-agent
make smoke          # swebench-smoke + tb-smoke + polyglot-smoke
make report         # -> $BENCH_RESULTS_DIR/summary.md
```

Hinweise aus diesem Lauf:

- Auf Hosts mit wenig Platz auf `C:` den SWE-bench-Driver mit
  `--cleanup-images` fahren (25 Per-Instance-Images ≈ 15–25 GB).
- Ohne `make` (Windows/Git-Bash) lassen sich alle Targets 1:1 mit
  `uv run …` nachbilden; `agentkit_bench/config.py` lädt die `.env` selbst.
