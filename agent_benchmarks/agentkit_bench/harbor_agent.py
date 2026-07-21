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
    benchmark_prompt_path,
    binary_path,
    swarm_enabled,
    swarm_prompt_path,
    swarm_roles_dir,
)

BINARY_DEST = "/usr/local/bin/agentkit"
PROMPT_DEST = "/installed-agent/benchmark_system.md"
OUTPUT_LOG = "/logs/agent/agentkit.txt"
# Harbor-Task-Cache auf dem Host (Quelle für Polyglot-Testdateien, s. unten).
HARBOR_TASK_CACHE = Path.home() / ".cache" / "harbor" / "tasks"
# Swarm-Modus (AGENTKIT_SWARM=1, siehe config.py): Team-Rollen + kombinierter
# System-Prompt (Benchmark-Regeln + englische Team-Instruktionen).
ROLES_DEST = "/installed-agent/roles"
SWARM_PROMPT_DEST = "/installed-agent/teamlead_bench.md"
FULL_PROMPT_DEST = "/installed-agent/system_full.md"


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
        if swarm_enabled():
            await self.exec_as_root(environment, f"mkdir -p {ROLES_DEST}")
            for role in sorted(swarm_roles_dir().glob("*.md")):
                await environment.upload_file(role, f"{ROLES_DEST}/{role.name}")
            await environment.upload_file(swarm_prompt_path(), SWARM_PROMPT_DEST)
            # Ein --system-file: Benchmark-Regeln + Team-Instruktionen kombiniert.
            await self.exec_as_root(
                environment,
                f"{{ cat {PROMPT_DEST}; echo; cat {SWARM_PROMPT_DEST}; }} > {FULL_PROMPT_DEST}",
            )
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
        system_file = FULL_PROMPT_DEST if swarm_enabled() else PROMPT_DEST
        agents_flag = f"--agents {ROLES_DEST} " if swarm_enabled() else ""
        cmd = (
            f"mkdir -p /logs/agent; "
            f"{BINARY_DEST} -p {task} -w \"$PWD\" -y --no-color --verify "
            f"--provider {agentkit_provider()} "
            f"--max-steps {agentkit_max_steps()} "
            f"--system-file {system_file} {agents_flag}"
            f"</dev/null > {OUTPUT_LOG} 2>&1; "
            f"rc=$?; tail -c 20000 {OUTPUT_LOG}; "
            f"if [ $rc -eq 1 ]; then "
            f"echo '[agentkit] exit 1 (max steps/runtime) — weiter zur Verifikation'; "
            f"exit 0; fi; exit $rc"
        )
        await self.exec_as_agent(environment, cmd, env=env)
