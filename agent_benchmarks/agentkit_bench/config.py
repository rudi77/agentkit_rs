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
    """Wissensgraph (`--graph DIR`) für die Task-Agenten. Default: AUS.

    Der Default war bis 2026-08-10 an. Drei Messreihen haben keinen Vorteil
    gezeigt — auch nicht, nachdem der Mechanismus repariert war (vorher
    verteilte sich das Wissen über `--graph-scope`-lose PID-Kollisionen auf 13
    zufällige Inseln). Mit benanntem Scope liest jeder Task zuverlässig, was
    die Vorgänger geschrieben haben, und liegt trotzdem hinten: Polyglot
    −6,3 Punkte, SWE-bench −1 Instanz. Für in sich geschlossene
    Benchmark-Aufgaben ist fremdes Wissen offenbar Ablenkung plus 150
    zusätzliche Werkzeugaufrufe je Arm.

    Wer den Graphen messen will, schaltet ihn mit BENCH_GRAPH=1 an; sinnvoll
    ist das erst bei Aufgabenfolgen, die wirklich aufeinander aufbauen.
    Braucht ein Binary mit dem Feature `graph`.
    """
    return os.environ.get("BENCH_GRAPH", "0").strip().lower() not in ("0", "false", "no")


def bench_work_enabled() -> bool:
    """Den Task über die Work-Runtime abarbeiten statt in einem Agentenlauf.

    Statt `agentkit --steps "<task>"` läuft dann `agentkit work create` +
    `work run`: der Task wird in Work Items zerlegt, jedes einzeln mit
    Versuchen, Leases und Artefakten abgearbeitet, der Zustand überlebt den
    Prozess. Das füllt den Work-Reiter im Betrachter und gibt jedem Item einen
    eigenen Versuchs-Zähler — ein gescheiterter Schritt reißt nicht den ganzen
    Task mit.

    Default: AUS (war bis 2026-08-10 an). Gemessen an Aufgaben dieser Größe
    verliert der Modus: Terminal-Bench 1/7 gegen 2/7, SWE-bench 3/10 gegen
    4/10. Der Grund steckt in der Konstruktion — jeder Item-Versuch bekommt
    einen FRISCH gebauten Agenten mit leerem Kontext, die Erkundung des Repos
    beginnt also in jedem Item von vorn, und `max_steps` gilt JE VERSUCH:
    sechs von zehn SWE-Instanzen liefen ins Limit, der Einzelagent bei keiner.
    Ø 13,4 Schritte je Agent gegen 34,8 beim Einzelagenten.

    Das widerlegt den Zweck der Runtime nicht — sie ist für Vorhaben gedacht,
    die ein Kontext nicht fasst. Es zeigt, dass eine SWE-bench-Lite-Instanz
    dafür zu kurz ist. BENCH_WORK=1 schaltet sie an.
    """
    return os.environ.get("BENCH_WORK", "0").strip().lower() not in ("0", "false", "no")


def bench_work_max_items() -> int:
    return int(os.environ.get("BENCH_WORK_MAX_ITEMS", "6"))


def bench_ctx_enabled() -> bool:
    """Kontext-Management (`--ctx DIR`) für die Task-Agenten.

    Braucht ein Binary mit dem Feature `ctxman` (scripts/build_musl.sh baut es
    mit). Im Work-Modus bekommt JEDER Item-Versuch ein eigenes
    Kontext-Verzeichnis — der leere Kontext je Versuch bleibt also erhalten,
    ctxman verwaltet nur, was innerhalb eines Versuchs anfällt.

    Abschaltbar mit BENCH_CTX=0.
    """
    return os.environ.get("BENCH_CTX", "1").strip().lower() not in ("0", "false", "no")


def shell_timeout(task: str = "") -> int:
    """Sekunden je `run_shell`-Aufruf eines Task-Agenten (`--shell-timeout`).

    Der Wert hängt am Benchmark, weil die Aufgaben verschieden sind:

    **60 s für Exercism/Polyglot.** Dort laufen die Tests in unter einer
    Sekunde, alles darüber ist kein „langsam", sondern ein Hänger. Beobachtet
    an `polyglot_python_two-bucket`, wo der Agent sich eine Endlosschleife
    schrieb: zwei Timeouts à 600 s kosteten 20 Minuten Wandzeit, ohne dass er
    aus „Timeout nach 600s" entnehmen konnte, dass sein Programm nicht
    terminiert — er schrieb dieselbe Struktur neu und lief erneut hinein.

    **600 s für Terminal-Bench.** Dort wird *gebaut*. Mit 60 s liefen in Runde
    2 neunzehn Kommandos in den Timeout, vierzehn davon allein in
    `build-pov-ray`: ein POV-Ray-Build wurde vierzehnmal abgeschnitten, und der
    Agent sah nie ein Ergebnis. Das A/B (identische Konfiguration, nur der
    Timeout verschieden) beseitigte alle sieben Timeouts — 7 → 0 — und änderte
    die Zahl gelöster Aufgaben NICHT (1/7 in beiden Armen). Der Wert steht
    hier also nicht, weil er Aufgaben löst, sondern weil abgeschnittene Builds
    Zeit ohne Gegenwert verbrennen.

    SWE-bench hat einen eigenen, fest verdrahteten Wert (siehe run_swebench):
    dort sind es echte Test-Suites, die tatsächlich Minuten brauchen.

    `BENCH_SHELL_TIMEOUT` überschreibt beides.
    """
    if wert := os.environ.get("BENCH_SHELL_TIMEOUT"):
        return int(wert)
    # Positiv auf Polyglot prüfen, nicht auf Terminal-Bench-Präfixe: Deren
    # Aufgabennamen sind offen (jedes neue TB-Dataset bringt eigene), während
    # `polyglot_` stabil ist. Ein unbekannter Task bekommt so den großzügigen
    # Wert — die sichere Richtung, denn ein zu kurzer Timeout schneidet Arbeit
    # ab, ein zu langer kostet nur im Fehlerfall Zeit.
    return 60 if task.startswith("polyglot_") else 600


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


# Der gemessene Standard: `-s plan`, sonst nichts. Der Schalter existiert in
# agentkit seit jeher und stand in acht Benchmark-Läufen in keiner einzigen
# Kommandozeile.
#
# Runde 3 und 4, je 64 Polyglot- + 25 SWE-bench-Aufgaben:
#
#   ReAct + Sub-Agenten (alter Default)   54/64 + 4/25 = 58/89
#   --no-subagents                        56/64 + 6/25 = 62/89
#   -s plan                               57/64 + 8/25 = 65/89   (Runde 3)
#   -s plan                               59/64 + 8/25 = 67/89   (Runde 4, Wdh.)
#   -s plan --no-subagents                54/64 + 7/25 = 61/89   (Runde 4)
#
# `-s plan` gewinnt in beiden Benchmarks und halbiert die Regressionen (6 → 3).
# Die Wiederholung in Runde 4 liegt mit 67/89 innerhalb der Streuung — der Arm
# ist reproduzierbar, anders als die Ein-Aufgaben-Unterschiede der Runden 1/2.
#
# Kombinieren hilft NICHT: `-s plan --no-subagents` verliert gegen `-s plan`
# allein (61/89 gegen 67/89), obwohl jeder Schalter einzeln gegen den alten
# Default gewinnt. Der Grund ist in den Traces sichtbar: `--no-subagents`
# entfernt nur `task`, nicht das `swarm`-Werkzeug. Auf den langen
# SWE-bench-Aufgaben baut sich der Agent daraufhin zur Laufzeit einen Swarm
# (24 von 25 Instanzen, 309 `swarm_*`-Aufrufe) — die Delegation verschwindet
# also nicht, sie nimmt den teureren Weg. Auf den kurzen Polyglot-Aufgaben
# greift er gar nicht zur Delegation; dort kostet der fehlende `task` schlicht
# fünf Aufgaben (59/64 → 54/64).
STANDARD_AGENT_FLAGS = "-s plan"


def bench_agent_flags() -> str:
    """Zusätzliche agentkit-Flags für die Task-Agenten (`BENCH_AGENT_FLAGS`).

    Ein generischer Durchreicher statt einer Env-Variable je Flag. Default ist
    [`STANDARD_AGENT_FLAGS`] — der beste gemessene Aufbau. `BENCH_AGENT_FLAGS=""`
    (leer gesetzt, nicht ungesetzt) fährt den nackten Agenten ohne Zusätze.

    Beispiel: BENCH_AGENT_FLAGS="-s react" für einen Vergleichsarm.
    """
    return os.environ.get("BENCH_AGENT_FLAGS", STANDARD_AGENT_FLAGS).strip()


def bench_graph_scope() -> str:
    """Arbeits-Scope des Graphen — EINE Kennung für alle Tasks eines Laufs.

    Vorher gab es keine, und agentkit fiel auf `pid-<prozess-id>` zurück. In
    frisch gestarteten Containern sind PIDs klein und vorhersagbar, also
    kollidieren sie: Im Lauf 2026-08-08 verteilten sich 81 Task-Läufe auf ganze
    13 Scopes. `poker`, `book-store`, `bowling` und `dot-dsl` liefen alle als
    PID 73 und lasen deshalb einander; `grade-school` lief als PID 72 und sah
    nichts davon. Wer wessen Wissen erbte, war ausgelost — und ein Lauf ohne
    Container (verstreute PIDs) hätte gar nichts geteilt.

    Mit `--graph-scope` ist es eine Zusage. Default: `bench`, damit alle Läufe
    desselben Graph-Verzeichnisses zusammenhängen; für ein sauberes A/B setzt
    man BENCH_GRAPH_SCOPE je Arm (wie BENCH_GRAPH_DIR).
    """
    return os.environ.get("BENCH_GRAPH_SCOPE", "bench")


def bench_graph_dir() -> Path:
    """Wo der geteilte Wissensgraph liegt — EIN Ort für ALLE Läufe.

    Vorher lag er je Lauf unter `<results>/<benchmark>/<lauf>/graph`. Das teilte
    ihn innerhalb eines Laufs und über Läufe hinweg gar nicht: nach 22 Läufen
    standen 22 winzige Wissensinseln da, jede bei null begonnen. Ein Gedächtnis,
    das bei jedem Start vergisst, ist keins.

    Jetzt ist es `<BENCH_RESULTS_DIR>/graph` — Lauf 23 fängt mit dem an, was die
    22 davor gelernt haben. Override: BENCH_GRAPH_DIR.

    ACHTUNG für Vergleichsmessungen: damit sind Läufe nicht mehr unabhängig.
    Ein späterer profitiert vom früheren, was für „hilft Gedächtnis?" der Sinn
    der Sache ist und für ein A/B zweier Prompt-Varianten eine Verunreinigung.
    Wer sauber vergleichen will, setzt BENCH_GRAPH_DIR je Arm — oder
    BENCH_GRAPH_SHARED=0 für einen Graphen je Task.
    """
    if p := os.environ.get("BENCH_GRAPH_DIR"):
        return Path(p)
    return results_root() / "graph"


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
