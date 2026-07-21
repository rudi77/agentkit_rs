# Benchmark-Harness für agentkit — Smoke-Läufe per Default, Vollläufe via *-full.
# Konfiguration: .env (siehe .env.example). `make help` zeigt alle Targets.

-include .env
export

RUN_ID     ?= $(shell date +%Y%m%d-%H%M%S)
SWEBENCH_LIMIT   ?= 25
SWEBENCH_WORKERS ?= 4
TB_CONCURRENT    ?= 4
POLYGLOT_LANGS   ?= python rust
# ~15 repräsentative Terminal-Bench-2.0-Tasks für den Smoke-Lauf
TB_SMOKE_FILTER  ?= -i "hello-*" -i "csv-*" -i "git-*" -i "build-*" -i "regex-*"

AGENT      := agentkit_bench.harbor_agent:AgentkitAgent
POLY_INCLUDES = $(foreach l,$(POLYGLOT_LANGS),-i "polyglot_$(l)_*")

.PHONY: help setup build-agent litellm-up litellm-down \
        swebench-smoke swebench-full swebench-gold swebench-demo \
        tb-hello tb-smoke tb-full polyglot-smoke polyglot-full \
        report smoke

help:
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*##/\t/'

setup: ## Python-Env aufsetzen (uv sync)
	uv sync

build-agent: ## statisches musl-Binary bauen -> build/agentkit-x86_64-musl
	bash scripts/build_musl.sh

litellm-up: ## LiteLLM-Proxy starten (Port 4000)
	docker compose -f litellm/docker-compose.yml up -d
	@sleep 3 && curl -sf http://localhost:4000/health/liveliness >/dev/null \
	    && echo "LiteLLM läuft auf :4000" || echo "WARNUNG: Healthcheck fehlgeschlagen"

litellm-down: ## LiteLLM-Proxy stoppen
	docker compose -f litellm/docker-compose.yml down

# ---------------------------------------------------------------- SWE-bench
swebench-demo: ## Plumbing-Test ohne API-Kosten (1 Task, --provider demo)
	uv run python -m agentkit_bench.swebench.run_swebench \
	    --limit 1 --provider demo --run-id demo-$(RUN_ID)

swebench-gold: ## Eval-Pipeline-Sanity: 5 Gold-Patches einreichen (muss 5/5 ergeben)
	uv run python -m agentkit_bench.swebench.run_swebench \
	    --gold --limit 5 --run-id gold-$(RUN_ID)

swebench-smoke: ## Smoke-Lauf ($(SWEBENCH_LIMIT) Tasks)
	uv run python -m agentkit_bench.swebench.run_swebench \
	    --limit $(SWEBENCH_LIMIT) --workers $(SWEBENCH_WORKERS) --run-id $(RUN_ID)

swebench-full: ## alle 300 SWE-bench-Lite-Tasks
	uv run python -m agentkit_bench.swebench.run_swebench \
	    --limit 0 --workers $(SWEBENCH_WORKERS) --run-id $(RUN_ID)

# ------------------------------------------------- Terminal-Bench via Harbor
# Hinweis: OPENAI_BASE_URL wird container-sichtbar exportiert, damit ein
# lokaler LiteLLM-Proxy aus den Task-Containern erreichbar ist.
define HARBOR_RUN
	OPENAI_BASE_URL=$$(uv run python -c "from agentkit_bench.config import container_base_url; print(container_base_url() or '')") \
	uv run harbor run -a $(AGENT) --n-concurrent $(TB_CONCURRENT)
endef

tb-hello: ## 1-Task-Ende-zu-Ende-Test (hello-world-Dataset)
	$(HARBOR_RUN) -d hello-world@1.0 -o results/terminal_bench --job-name hello-$(RUN_ID)

tb-smoke: ## Terminal-Bench-2.0-Subset ($(TB_SMOKE_FILTER))
	$(HARBOR_RUN) -d terminal-bench@2.0 $(TB_SMOKE_FILTER) \
	    -o results/terminal_bench --job-name $(RUN_ID)

tb-full: ## alle 89 Terminal-Bench-2.0-Tasks
	$(HARBOR_RUN) -d terminal-bench@2.0 -o results/terminal_bench --job-name $(RUN_ID)

# ------------------------------------------------------ Aider Polyglot
polyglot-smoke: ## Polyglot-Subset (Sprachen: $(POLYGLOT_LANGS))
	$(HARBOR_RUN) -d aider-polyglot@1.0 $(POLY_INCLUDES) \
	    -o results/polyglot --job-name $(RUN_ID)

polyglot-full: ## alle 225 Polyglot-Tasks (6 Sprachen)
	$(HARBOR_RUN) -d aider-polyglot@1.0 -o results/polyglot --job-name $(RUN_ID)

# ----------------------------------------------------------------- Sammeln
report: ## Ergebnisse zu results/summary.md zusammenfassen
	uv run python -m agentkit_bench.report

smoke: build-agent swebench-smoke tb-smoke polyglot-smoke report ## kompletter Smoke-Durchlauf
