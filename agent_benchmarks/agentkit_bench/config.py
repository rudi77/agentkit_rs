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
    # Docker Desktop (macOS/Windows) bietet host.docker.internal; auf nativem
    # Linux führt stattdessen die Bridge-Gateway-IP zum Host.
    host = (
        "host.docker.internal"
        if sys.platform in ("darwin", "win32")
        else docker_bridge_gateway()
    )
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


# --------------------------------------------------------------- Swarm-Modus
# AGENTKIT_SWARM=1 lässt agentkit als Software-Dev-Team laufen (Orchestrator +
# Rollen-Sub-Agents, siehe agent_framework_rs/examples/coding_swarm): der Harness
# lädt die Rollen-`*.md` mit in den Container, hängt die englischen
# Team-Instruktionen an den Benchmark-System-Prompt an und startet agentkit mit
# `--agents`. Kostet mehr Schritte/Tokens — AGENTKIT_MAX_STEPS ggf. erhöhen.

SWARM_EXAMPLE_DIR = ROOT.parent / "agent_framework_rs" / "examples" / "coding_swarm"


def swarm_enabled() -> bool:
    return os.environ.get("AGENTKIT_SWARM", "").strip().lower() in ("1", "true", "yes")


def swarm_roles_dir() -> Path:
    d = Path(os.environ.get("AGENTKIT_SWARM_ROLES", SWARM_EXAMPLE_DIR / "roles"))
    if not d.is_dir() or not any(d.glob("*.md")):
        raise FileNotFoundError(f"Swarm-Rollen fehlen: {d} (erwartet *.md-Rollendateien)")
    return d


def swarm_prompt_path() -> Path:
    p = SWARM_EXAMPLE_DIR / "teamlead_bench.md"
    if not p.is_file():
        raise FileNotFoundError(f"Swarm-Team-Prompt fehlt: {p}")
    return p


# ------------------------------------------------------ Beobachtung (viz)
# Jeder Task schreibt seinen eigenen NDJSON-Trace neben seine übrigen
# Artefakte — bei Harbor unter <trial>/agent/trace, bei SWE-bench unter
# <results>/swebench/<run-id>/<instance>/trace. `agentkit viz --trace
# <BENCH_RESULTS_DIR>` findet sie rekursiv und zeigt jeden Task als eigene
# Sitzung, live wie nachträglich.
#
# WARNUNG: der Trace ist unredigiert (Dateiinhalte, Shell-Ausgaben,
# Modellantworten) und wächst auf mehrere hundert KB je Task.


def bench_trace_enabled() -> bool:
    return os.environ.get("BENCH_TRACE", "1").strip().lower() not in ("0", "false", "no")


def bench_graph_enabled() -> bool:
    """Wissensgraph (`--graph DIR`) für die Task-Agenten.

    ACHTUNG, das ändert den Benchmark: mit Graph bekommt der Agent zusätzlich
    die `graph_*`-Tools, die Ergebnisse sind also nicht mehr direkt mit Läufen
    ohne Graph vergleichbar. Braucht ein Binary mit dem Feature `graph`
    (scripts/build_musl.sh baut es mit). Abschaltbar mit BENCH_GRAPH=0.
    """
    return os.environ.get("BENCH_GRAPH", "1").strip().lower() not in ("0", "false", "no")


def bench_work_enabled() -> bool:
    """Den Task über die Work-Runtime abarbeiten statt in einem Agentenlauf.

    Statt `agentkit --steps "<task>"` läuft dann `agentkit work create` +
    `work run`: der Task wird in Work Items zerlegt, jedes einzeln mit
    Versuchen, Leases und Artefakten abgearbeitet, der Zustand überlebt den
    Prozess. Das füllt den Work-Reiter im Betrachter und gibt jedem Item einen
    eigenen Versuchs-Zähler — ein gescheiterter Schritt reißt nicht den ganzen
    Task mit.

    ACHTUNG, das misst etwas anderes: jeder Item-Versuch bekommt einen FRISCH
    gebauten Agenten mit leerem Kontext, die Erkundung des Repos passiert also
    mehrfach. Kostet deutlich mehr Tokens und ist mit Läufen ohne Work nicht
    direkt vergleichbar — BENCH_WORK=0 stellt den Einzelagenten wieder her.
    """
    return os.environ.get("BENCH_WORK", "1").strip().lower() not in ("0", "false", "no")


def bench_work_max_items() -> int:
    return int(os.environ.get("BENCH_WORK_MAX_ITEMS", "6"))


def shell_timeout() -> int:
    """Sekunden je `run_shell`-Aufruf eines Task-Agenten (`--shell-timeout`).

    60 statt der früheren 600: bei Harbor-Datasets (Exercism, Terminal-Bench)
    laufen die Tests in unter einer Sekunde, alles darüber ist kein „langsam",
    sondern ein Hänger. Beobachtet an `polyglot_python_two-bucket`, wo der
    Agent sich eine Endlosschleife schrieb: zwei Timeouts à 600 s haben 20
    Minuten Wandzeit gekostet, ohne dass er aus der Meldung („Timeout nach
    600s") entnehmen konnte, dass sein Programm nicht terminiert — er schrieb
    dieselbe Struktur neu und lief erneut hinein. Mit 60 s kostet derselbe
    Fehler eine Minute, und dem Agenten bleiben Schritte, ihn zu bemerken.

    SWE-bench hat einen eigenen Wert (siehe run_swebench): dort sind es echte
    Test-Suites, die tatsächlich Minuten brauchen.
    """
    return int(os.environ.get("BENCH_SHELL_TIMEOUT", "60"))


def bench_graph_shared() -> bool:
    """Ein GEMEINSAMER Graph für alle Tasks eines Laufs statt einem je Task.

    Das ist der Modus, in dem ein Graph im Benchmark überhaupt etwas bedeutet:
    Task N+1 sieht, was Task N gelernt hat. Genau deshalb verlangt er
    **sequenzielle Ausführung** (`--n-concurrent 1` bzw. `--workers 1`) — und
    zwar zweifach: ein späterer Task kann nur profitieren, wenn der frühere
    fertig ist, und der Store kompaktiert sein Journal ab 256 Zeilen (schreibt
    die Datei also neu), was zwei gleichzeitige Schreiber zerlegen würde.
    """
    return os.environ.get("BENCH_GRAPH_SHARED", "1").strip().lower() not in ("0", "false", "no")


def graph_addendum_path() -> Path:
    """Prompt-Zusatz, der den Agenten den Graphen überhaupt benutzen lässt.

    Ohne ihn bleibt der Graph leer: die Tools sind registriert, aber der
    Benchmark-Prompt erwähnt sie nicht, und eine in sich geschlossene Aufgabe
    gibt von sich aus keinen Anlass, sich etwas zu merken.
    """
    return ROOT / "prompts" / "graph_addendum.md"


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


def results_root() -> Path:
    """Wurzel für alle Lauf-Ergebnisse (BENCH_RESULTS_DIR, Default: ./results)."""
    return Path(os.environ.get("BENCH_RESULTS_DIR", ROOT / "results"))


def results_dir(benchmark: str, run_id: str) -> Path:
    d = results_root() / benchmark / run_id
    d.mkdir(parents=True, exist_ok=True)
    return d
