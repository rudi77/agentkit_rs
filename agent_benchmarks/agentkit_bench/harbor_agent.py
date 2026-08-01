"""Harbor-Adapter: agentkit als installierter Agent für Terminal-Bench 2.0,
Aider Polyglot und andere Harbor-Datasets.

Aufruf (aus agent_benchmarks/):

    uv run harbor run -d terminal-bench@2.0 \
        -a agentkit_bench.harbor_agent:AgentkitAgent \
        --n-concurrent 4 -o results/terminal_bench

Der Adapter lädt das statische musl-Binary (build/agentkit-x86_64-musl,
siehe scripts/build_musl.sh) per upload_file in den Task-Container —
funktioniert in jedem glibc/musl/alpine-Image ohne weitere Abhängigkeiten.
Alternativ zieht er es von AGENTKIT_BINARY_URL (für Cloud-Executors ohne
Host-Dateizugriff).

Provider-Konfiguration kommt aus dem Host-Env (OPENAI_*/AZURE_*, siehe
config.py); OPENAI_BASE_URL wird für die Container-Sicht umgeschrieben,
damit ein lokaler LiteLLM-Proxy erreichbar bleibt.
"""

from __future__ import annotations

import glob as globmod
import os
import shlex
from pathlib import Path
from typing import override

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    ContextWindowExceededError,
    ErrorPattern,
    UnknownApiError,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from agentkit_bench.config import (
    agentkit_container_env,
    agentkit_max_steps,
    agentkit_provider,
    bench_graph_enabled,
    bench_graph_dir,
    bench_graph_shared,
    bench_trace_enabled,
    bench_work_enabled,
    bench_work_max_items,
    benchmark_prompt_path,
    binary_path,
    graph_addendum_path,
    shell_timeout,
    swarm_enabled,
    swarm_prompt_path,
    swarm_roles_dir,
)

BINARY_DEST = "/usr/local/bin/agentkit"
PROMPT_DEST = "/installed-agent/benchmark_system.md"
OUTPUT_LOG = "/logs/agent/agentkit.txt"
# Neben dem Log, aus demselben Grund: /logs ist auf den Host bind-gemountet.
TRACE_DEST = "/logs/agent/trace"
GRAPH_DEST = "/logs/agent/graph"
# Work-Projekte des Tasks — ebenfalls im bind-gemounteten /logs.
WORK_DEST = "/logs/agent/work"
# Harbor-Task-Cache auf dem Host (Quelle für Polyglot-Testdateien, s. unten).
HARBOR_TASK_CACHE = Path.home() / ".cache" / "harbor" / "tasks"
# Swarm-Modus (AGENTKIT_SWARM=1, siehe config.py): Team-Rollen + kombinierter
# System-Prompt (Benchmark-Regeln + englische Team-Instruktionen).
ROLES_DEST = "/installed-agent/roles"
SWARM_PROMPT_DEST = "/installed-agent/teamlead_bench.md"
FULL_PROMPT_DEST = "/installed-agent/system_full.md"
# Prompt-Zusatz, der den Graphen erst benutzbar macht (siehe config.py).
GRAPH_PROMPT_DEST = "/installed-agent/graph_addendum.md"
# Dateiname des Graph-Journals (agentkit_graph::store::JOURNAL_FILE).
GRAPH_JOURNAL = "graph.jsonl"


class AgentkitAgent(BaseInstalledAgent):
    ERROR_PATTERNS = BaseInstalledAgent.ERROR_PATTERNS + [
        # agentkit meldet API-Probleme deutsch (exit code 2, src/cli.rs)
        ErrorPattern(r"API-Fehler|\(keine Antwort\)", UnknownApiError),
        ErrorPattern(r"Kontext zu groß|Prompt zu groß", ContextWindowExceededError),
    ]

    @staticmethod
    @override
    def name() -> str:
        return "agentkit"

    @override
    def get_version_command(self) -> str | None:
        return f"{BINARY_DEST} --version"

    # ------------------------------------------------ Polyglot: Tests sichtbar machen
    # Der originale Aider-Benchmark zeigt dem Modell die Testdatei; Harbor spielt sie
    # erst beim Verifier ein. Ohne Tests muss der Agent die exakte API raten — das
    # kostete im ersten Lauf 31 von 34 Python-Tasks (Rust rettet der Compiler).
    # Deshalb: Testdateien aus dem Host-Task-Cache in den Workspace (/app) laden.
    # Abschaltbar mit BENCH_SHOW_TESTS=0; greift NUR bei polyglot_*-Tasks.

    def _task_name(self) -> str:
        # logs_dir = <trial-dir>/agent; Trial-Name = "<task>__<suffix>".
        return Path(self.logs_dir).parent.name.rsplit("__", 1)[0]

    def _polyglot_test_files(self) -> list[tuple[Path, str]]:
        """(Host-Pfad, Container-Ziel)-Paare der Testdateien des aktuellen Tasks."""
        task = self._task_name()
        if not task.startswith("polyglot_"):
            return []
        if os.environ.get("BENCH_SHOW_TESTS", "1").strip() == "0":
            return []
        hits = globmod.glob(str(HARBOR_TASK_CACHE / "*" / task / "tests"))
        if not hits:
            return []
        tests_dir = Path(hits[0])
        out: list[tuple[Path, str]] = []
        for p in tests_dir.rglob("*"):
            if not p.is_file():
                continue
            rel = p.relative_to(tests_dir)
            # .meta enthält die Musterlösung, test.sh ist Verifier-Interna — beides tabu.
            if rel.parts[0] == ".meta" or rel.name == "test.sh":
                continue
            out.append((p, f"/app/{rel.as_posix()}"))
        return out

    # ------------------------------------------------ geteilter Wissensgraph
    # Harbor mountet nur `/logs` (das Trial-Verzeichnis) — ein Verzeichnis
    # DARÜBER kann der Container nicht sehen. Der laufübergreifende Graph wird
    # deshalb vor dem Task hineinkopiert und danach wieder heraus. Das ist
    # korrekt, solange die Tasks NACHEINANDER laufen (`--n-concurrent 1`) —
    # was der Modus ohnehin verlangt, siehe config.bench_graph_shared.

    def _geteilter_graph(self) -> Path | None:
        """Host-Verzeichnis des laufübergreifenden Graphen (siehe config)."""
        if not bench_graph_shared():
            return None
        return bench_graph_dir()

    async def _graph_hineinlegen(self, environment: BaseEnvironment) -> None:
        geteilt = self._geteilter_graph()
        if geteilt is None:
            return
        await self.exec_as_root(environment, f"mkdir -p {GRAPH_DEST}")
        journal = geteilt / GRAPH_JOURNAL
        if journal.is_file():
            await environment.upload_file(journal, f"{GRAPH_DEST}/{GRAPH_JOURNAL}")
        # Der Agent läuft nicht als root; ohne das schreibt er nicht hinein.
        await self.exec_as_root(environment, f"chmod -R a+rw {GRAPH_DEST}")

    async def _graph_herausholen(self, environment: BaseEnvironment) -> None:
        geteilt = self._geteilter_graph()
        if geteilt is None:
            return
        geteilt.mkdir(parents=True, exist_ok=True)
        try:
            await environment.download_file(
                f"{GRAPH_DEST}/{GRAPH_JOURNAL}", geteilt / GRAPH_JOURNAL
            )
        except Exception as e:
            # Kein Journal heißt: der Agent hat nichts gemerkt. Das ist ein
            # zulässiges Ergebnis und darf den Task nicht scheitern lassen.
            print(f"[agentkit] geteilter Graph nicht zurückgeholt: {e}")

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        url = os.environ.get("AGENTKIT_BINARY_URL", "").strip()
        if url:
            q = shlex.quote(url)
            await self.exec_as_root(
                environment,
                f"curl -fsSL {q} -o {BINARY_DEST} || wget -qO {BINARY_DEST} {q}",
            )
        else:
            await environment.upload_file(binary_path(), BINARY_DEST)
        await environment.upload_file(benchmark_prompt_path(), PROMPT_DEST)
        # Der System-Prompt wird aus Bausteinen zusammengesetzt: Benchmark-Regeln
        # plus, je nach Modus, Team-Instruktionen und Graph-Anleitung.
        teile = [PROMPT_DEST]
        if swarm_enabled():
            await self.exec_as_root(environment, f"mkdir -p {ROLES_DEST}")
            for role in sorted(swarm_roles_dir().glob("*.md")):
                await environment.upload_file(role, f"{ROLES_DEST}/{role.name}")
            await environment.upload_file(swarm_prompt_path(), SWARM_PROMPT_DEST)
            teile.append(SWARM_PROMPT_DEST)
        if bench_graph_enabled():
            await environment.upload_file(graph_addendum_path(), GRAPH_PROMPT_DEST)
            teile.append(GRAPH_PROMPT_DEST)
            await self._graph_hineinlegen(environment)
        if len(teile) > 1:
            gefuege = "; ".join(f"cat {t}; echo" for t in teile)
            await self.exec_as_root(environment, f"{{ {gefuege}; }} > {FULL_PROMPT_DEST}")
        # Polyglot: Testdateien in den Workspace + pytest für die Python-Spur —
        # der Agent kann damit gegen die ECHTEN Tests arbeiten statt zu raten.
        test_files = self._polyglot_test_files()
        for src, dest in test_files:
            parent = os.path.dirname(dest)
            if parent not in ("", "/app"):
                await self.exec_as_root(environment, f"mkdir -p {shlex.quote(parent)}")
            await environment.upload_file(src, dest)
        if any(dest.endswith(".py") for _, dest in test_files):
            await self.exec_as_root(
                environment,
                "python3 -m pip install -q pytest 2>/dev/null || pip install -q pytest || true",
            )
        await self.exec_as_root(
            environment,
            f"chmod 755 {BINARY_DEST} && chmod -R a+r /installed-agent && {BINARY_DEST} --version",
        )

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        env = agentkit_container_env()
        # `harbor run -m provider/modell` überschreibt das Modell — beim
        # Azure-Provider heißt das Deployment (Modellvergleiche ohne .env-Edit:
        # `harbor run -m azure/<deployment> ...`).
        if self.model_name:
            model = self.model_name.split("/", 1)[-1]
            env["OPENAI_MODEL"] = model
            if agentkit_provider() == "azure":
                env["AZURE_OPENAI_DEPLOYMENT"] = model

        task = shlex.quote(self.render_instruction(instruction))
        # - </dev/null: agentkit liest non-TTY-stdin bis EOF (src/cli.rs) —
        #   ohne Redirect hängt der Aufruf.
        # - Exit 1 (max-steps/Laufzeitfehler) wird geschluckt: partielle
        #   Arbeit soll trotzdem verifiziert werden. Exit 2/3/4 (API/Kontext/
        #   Format) propagieren, damit Harbors Retry-Klassifikation greift.
        # Zusammengesetzt wird der Prompt, sobald ein Baustein dazukommt
        # (Team-Instruktionen oder Graph-Anleitung) — siehe install().
        system_file = (
            FULL_PROMPT_DEST if (swarm_enabled() or bench_graph_enabled()) else PROMPT_DEST
        )
        agents_flag = f"--agents {ROLES_DEST} " if swarm_enabled() else ""
        # /logs ist von Harbor auf das Trial-Verzeichnis des Hosts
        # bind-gemountet (dort landet auch OUTPUT_LOG). Trace und Graph sind
        # deshalb schon WÄHREND des Laufs auf dem Host lesbar:
        # `agentkit viz --trace $BENCH_RESULTS_DIR` zeigt jeden Task als Sitzung.
        beobachtung = ""
        if bench_trace_enabled():
            beobachtung += f"--trace {TRACE_DEST} "
        if bench_graph_enabled():
            beobachtung += f"--graph {GRAPH_DEST} "
        if bench_work_enabled():
            # Work-Runtime: zerlegen, dann Item für Item abarbeiten. `work
            # create` gibt die Projekt-ID auf stdout aus (letzte Zeile). Das
            # Work-Verzeichnis liegt neben Trace und Graph im gemounteten
            # /logs/agent, der Work-Reiter im Betrachter findet es dort.
            agentenlauf = (
                f"PID=$({BINARY_DEST} work create --title 'Benchmark-Task' "
                f"--objective {task} -w \"$PWD\" --dir {WORK_DEST} "
                f"--max-items {bench_work_max_items()} "
                f"--max-steps {agentkit_max_steps()} </dev/null | tail -1); "
                f"{BINARY_DEST} work run \"$PID\" -w \"$PWD\" --dir {WORK_DEST} "
                f"-y --steps --provider {agentkit_provider()} "
                f"--max-steps {agentkit_max_steps()} "
                f"--system-file {system_file} {beobachtung}"
            )
        else:
            # --steps statt -p: stdout bleibt bei gepipter Ausgabe die finale
            # Antwort, aber stderr trägt den vollen Tool-Trace — beides landet
            # im OUTPUT_LOG (ohne Trace waren Fehlläufe nicht diagnostizierbar).
            agentenlauf = (
                f"{BINARY_DEST} --steps {task} -w \"$PWD\" -y --no-color --verify "
                f"--shell-timeout {shell_timeout()} "
                f"--provider {agentkit_provider()} "
                f"--max-steps {agentkit_max_steps()} "
                f"--system-file {system_file} {agents_flag}{beobachtung}"
            )
        cmd = (
            f"mkdir -p /logs/agent; "
            f"{agentenlauf}"
            f"</dev/null > {OUTPUT_LOG} 2>&1; "
            f"rc=$?; tail -c 20000 {OUTPUT_LOG}; "
            f"if [ $rc -eq 1 ]; then "
            f"echo '[agentkit] exit 1 (max steps/runtime) — weiter zur Verifikation'; "
            f"exit 0; fi; exit $rc"
        )
        await self.exec_as_agent(environment, cmd, env=env)
        # NACH dem Lauf: was der Agent gelernt hat, dem nächsten Task übergeben.
        if bench_graph_enabled():
            await self._graph_herausholen(environment)
