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

import os
import shlex
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
)

BINARY_DEST = "/usr/local/bin/agentkit"
PROMPT_DEST = "/installed-agent/benchmark_system.md"
OUTPUT_LOG = "/logs/agent/agentkit.txt"


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
        await self.exec_as_root(
            environment,
            f"chmod 755 {BINARY_DEST} && chmod 644 {PROMPT_DEST} && {BINARY_DEST} --version",
        )

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        env = agentkit_container_env()
        # `harbor run -m provider/modell` überschreibt OPENAI_MODEL
        if self.model_name:
            env["OPENAI_MODEL"] = self.model_name.split("/", 1)[-1]

        task = shlex.quote(self.render_instruction(instruction))
        # - </dev/null: agentkit liest non-TTY-stdin bis EOF (src/cli.rs) —
        #   ohne Redirect hängt der Aufruf.
        # - Exit 1 (max-steps/Laufzeitfehler) wird geschluckt: partielle
        #   Arbeit soll trotzdem verifiziert werden. Exit 2/3/4 (API/Kontext/
        #   Format) propagieren, damit Harbors Retry-Klassifikation greift.
        cmd = (
            f"mkdir -p /logs/agent; "
            f"{BINARY_DEST} -p {task} -w \"$PWD\" -y --no-color "
            f"--provider {agentkit_provider()} "
            f"--max-steps {agentkit_max_steps()} "
            f"--system-file {PROMPT_DEST} "
            f"</dev/null > {OUTPUT_LOG} 2>&1; "
            f"rc=$?; tail -c 20000 {OUTPUT_LOG}; "
            f"if [ $rc -eq 1 ]; then "
            f"echo '[agentkit] exit 1 (max steps/runtime) — weiter zur Verifikation'; "
            f"exit 0; fi; exit $rc"
        )
        await self.exec_as_agent(environment, cmd, env=env)
