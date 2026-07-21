"""Zentrale Konfiguration des Benchmark-Harness.

Alles läuft über Env-Vars (geladen aus agent_benchmarks/.env, siehe
.env.example). Der Env-Contract entspricht exakt dem, was agentkit selbst
liest (agent_framework_rs/src/llm.rs::openai_from_env / azure_from_env):

  OPENAI_API_KEY, OPENAI_MODEL, OPENAI_BASE_URL   -> --provider openai
  AZURE_OPENAI_ENDPOINT/-API_KEY/-DEPLOYMENT/...  -> --provider azure

Besonderheit Container-Netzwerk: Läuft ein LiteLLM-Proxy auf dem Host
(OPENAI_BASE_URL=http://localhost:4000/v1), ist "localhost" aus einem
Task-Container heraus der Container selbst. container_base_url() schreibt
die URL deshalb auf eine container-sichtbare Adresse um
(host.docker.internal auf Docker Desktop, sonst die Docker-Bridge-Gateway-IP).
Override: BENCH_CONTAINER_BASE_URL.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from dotenv import load_dotenv

ROOT = Path(__file__).resolve().parent.parent  # agent_benchmarks/
load_dotenv(ROOT / ".env")

BINARY_NAME = "agentkit-x86_64-musl"

# Env-Vars, die 1:1 in die Task-Container durchgereicht werden.
PASSTHROUGH_KEYS = [
    "OPENAI_API_KEY",
    "OPENAI_MODEL",
    "AZURE_OPENAI_ENDPOINT",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_DEPLOYMENT",
    "AZURE_OPENAI_API_VERSION",
    "AGENTKIT_PROVIDER",
    "AGENTKIT_MAX_STEPS",
]


def docker_bridge_gateway() -> str:
    """Gateway-IP der Docker-Default-Bridge (Linux-Fallback für 'localhost')."""
    try:
        out = subprocess.run(
            ["docker", "network", "inspect", "bridge",
             "--format", "{{(index .IPAM.Config 0).Gateway}}"],
            capture_output=True, text=True, timeout=10, check=True,
        ).stdout.strip()
        if out:
            return out
    except Exception:
        pass
    return "172.17.0.1"


def container_base_url() -> str | None:
    """OPENAI_BASE_URL aus Sicht eines Task-Containers (oder None = direkt OpenAI)."""
    override = os.environ.get("BENCH_CONTAINER_BASE_URL")
    if override:
        return override
    url = os.environ.get("OPENAI_BASE_URL", "").strip()
    if not url:
        return None
    if "localhost" not in url and "127.0.0.1" not in url:
        return url  # bereits von überall erreichbar
    host = "host.docker.internal" if sys.platform == "darwin" else docker_bridge_gateway()
    return url.replace("localhost", host).replace("127.0.0.1", host)


def agentkit_container_env() -> dict[str, str]:
    """Env-Block für agentkit-Aufrufe *innerhalb* von Task-Containern."""
    env = {k: v for k in PASSTHROUGH_KEYS if (v := os.environ.get(k))}
    if url := container_base_url():
        env["OPENAI_BASE_URL"] = url
    return env


def agentkit_provider() -> str:
    return os.environ.get("AGENTKIT_PROVIDER", "openai")


def agentkit_max_steps() -> int:
    return int(os.environ.get("AGENTKIT_MAX_STEPS", "100"))


def bench_model_name() -> str:
    """model_name_or_path in den SWE-bench-Predictions."""
    if name := os.environ.get("BENCH_MODEL_NAME"):
        return name
    model = os.environ.get("OPENAI_MODEL", "unknown-model")
    return f"agentkit-{model}"


def binary_path() -> Path:
    """Pfad zum statischen musl-Binary (Build via scripts/build_musl.sh)."""
    p = Path(os.environ.get("AGENTKIT_BINARY_PATH", ROOT / "build" / BINARY_NAME))
    if not p.is_file():
        raise FileNotFoundError(
            f"agentkit-Binary fehlt: {p}\n"
            f"Erst bauen: make build-agent  (oder scripts/build_musl.sh)"
        )
    return p


def benchmark_prompt_path() -> Path:
    return ROOT / "prompts" / "benchmark_system_prompt.md"


def results_dir(benchmark: str, run_id: str) -> Path:
    d = ROOT / "results" / benchmark / run_id
    d.mkdir(parents=True, exist_ok=True)
    return d
