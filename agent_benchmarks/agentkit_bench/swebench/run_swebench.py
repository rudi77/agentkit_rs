"""SWE-bench-Driver: lässt agentkit pro Instanz laufen und schreibt Predictions-JSONL.

Smoke-Lauf (25 Tasks, Auswertung via sb-cli):

    uv run python -m agentkit_bench.swebench.run_swebench --limit 25
    uv run sb-cli submit swe-bench_lite test \
        --predictions_path results/swebench/<run_id>/preds.jsonl --run_id <run_id>

Voller Lauf: --limit 0. Plumbing-Test ohne API-Kosten: --provider demo --limit 1.
Eval-Pipeline-Sanity: --gold --limit 5 (reicht die Gold-Patches ein, muss 5/5 ergeben).
"""

from __future__ import annotations

import argparse
import datetime
import json
import platform
import shlex
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from rich.console import Console
from rich.table import Table

from agentkit_bench.config import (
    ROOT,
    agentkit_container_env,
    agentkit_max_steps,
    agentkit_provider,
    bench_model_name,
    benchmark_prompt_path,
    binary_path,
    results_dir,
    swarm_enabled,
    swarm_prompt_path,
    swarm_roles_dir,
)
from agentkit_bench.swebench.docker_env import SwebenchContainer, image_for, remove_image

console = Console(stderr=True)

BINARY_DEST = "/usr/local/bin/agentkit"
PROMPT_DEST = "/agentkit_benchmark_system.md"
# Swarm-Modus (AGENTKIT_SWARM=1, siehe config.py): Team-Rollen + kombinierter
# System-Prompt (Benchmark-Regeln + englische Team-Instruktionen).
ROLES_DEST = "/agentkit_roles"
SWARM_PROMPT_DEST = "/agentkit_teamlead_bench.md"
FULL_PROMPT_DEST = "/agentkit_system_full.md"

TASK_TEMPLATE = """You are working in a Python repository checked out at the current \
working directory. The repository has a fully set-up development environment.

Below is a real GitHub issue from this repository:

<issue>
{problem_statement}
</issue>

Fix the issue by modifying the repository's source code. Do not modify any test \
files. Verify your fix if practical by running the narrowest relevant tests. \
When you are done, briefly state what you changed — the harness collects your \
changes via git diff, so do not print the diff yourself."""


def load_instances(args: argparse.Namespace) -> list[dict]:
    from datasets import load_dataset

    ds = load_dataset(args.dataset, split=args.split)
    instances = sorted(ds, key=lambda r: r["instance_id"])
    if args.instance_id:
        wanted = set(args.instance_id)
        instances = [r for r in instances if r["instance_id"] in wanted]
    if args.slice:
        a, _, b = args.slice.partition(":")
        instances = instances[int(a or 0):int(b) if b else None]
    if args.limit and args.limit > 0:
        instances = instances[: args.limit]
    return instances


def render_task(inst: dict) -> str:
    return TASK_TEMPLATE.format(problem_statement=inst["problem_statement"].strip())


def agent_command(max_steps: int, provider: str, workspace: str) -> str:
    # </dev/null: agentkit liest non-TTY-stdin bis EOF — ohne Redirect hängt es.
    system_file = FULL_PROMPT_DEST if swarm_enabled() else PROMPT_DEST
    agents = f"--agents {ROLES_DEST} " if swarm_enabled() else ""
    return (
        f'{BINARY_DEST} -p "$SWE_TASK" -w {shlex.quote(workspace)} -y --no-color '
        f"--provider {provider} --max-steps {max_steps} "
        f"--system-file {system_file} {agents}</dev/null"
    )


def run_instance_docker(inst: dict, args: argparse.Namespace) -> tuple[dict, dict]:
    iid = inst["instance_id"]
    image = image_for(iid)
    plat = "linux/amd64" if platform.machine() not in ("x86_64", "AMD64") else None
    env = agentkit_container_env() | {"SWE_TASK": render_task(inst)}
    with SwebenchContainer(image, platform=plat) as c:
        c.copy_in(binary_path(), BINARY_DEST)
        c.copy_in(benchmark_prompt_path(), PROMPT_DEST)
        if swarm_enabled():
            # docker cp kopiert Verzeichnisse rekursiv — die Team-Rollen als Ganzes.
            c.copy_in(swarm_roles_dir(), ROLES_DEST)
            c.copy_in(swarm_prompt_path(), SWARM_PROMPT_DEST)
            c.exec(
                f"{{ cat {PROMPT_DEST}; echo; cat {SWARM_PROMPT_DEST}; }} > {FULL_PROMPT_DEST}",
                timeout=60,
            )
        c.exec("git config --global --add safe.directory /testbed", timeout=60)
        res = c.exec(
            agent_command(args.max_steps, args.provider, "/testbed"),
            env=env,
            timeout=args.task_timeout,
        )
        # Diff unabhängig vom Exit-Code einsammeln — auch bei max-steps (exit 1)
        # ist der Patch oft brauchbar. `git add -A` erfasst neue Dateien.
        diff = c.exec(
            "git add -A >/dev/null 2>&1 && git -c core.quotepath=false diff --cached",
            timeout=300,
        )
    if args.cleanup_images:
        remove_image(image)
    status = {
        "instance_id": iid,
        "agent_exit_code": res.returncode,
        "stdout_tail": res.stdout[-4000:],
        "stderr_tail": res.stderr[-4000:],
    }
    pred = {
        "instance_id": iid,
        "model_name_or_path": args.model_name,
        "model_patch": diff.stdout if diff.returncode == 0 else "",
    }
    return pred, status


def run_instance_local(inst: dict, args: argparse.Namespace) -> tuple[dict, dict]:
    """Docker-freier Fallback: Repo klonen, Host-Binary laufen lassen.

    Nur Patch-Erzeugung — der Agent kann die Tests des Projekts hier nicht
    ausführen (keine eingerichtete Umgebung). Für --provider demo /
    Plumbing-Tests gedacht.
    """
    iid = inst["instance_id"]
    host_bin = binary_path()
    with tempfile.TemporaryDirectory(prefix=f"swe-{iid}-") as tmp:
        ws = Path(tmp) / "repo"
        for cmd in (
            ["git", "clone", "--quiet", f"https://github.com/{inst['repo']}.git", str(ws)],
            ["git", "-C", str(ws), "checkout", "--quiet", inst["base_commit"]],
        ):
            subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=600)
        env = {
            **agentkit_container_env(),
            "SWE_TASK": render_task(inst),
            "HOME": str(Path.home()),
            "PATH": "/usr/local/bin:/usr/bin:/bin",
        }
        system_file = benchmark_prompt_path()
        agents = ""
        if swarm_enabled():
            system_file = Path(tmp) / "system_full.md"
            system_file.write_text(
                benchmark_prompt_path().read_text() + "\n" + swarm_prompt_path().read_text()
            )
            agents = f"--agents {shlex.quote(str(swarm_roles_dir()))} "
        cmd = (
            f'{host_bin} -p "$SWE_TASK" -w {shlex.quote(str(ws))} -y --no-color '
            f"--provider {args.provider} --max-steps {args.max_steps} "
            f"--system-file {shlex.quote(str(system_file))} {agents}</dev/null"
        )
        res = subprocess.run(
            ["bash", "-c", cmd], env=env, capture_output=True, text=True,
            timeout=args.task_timeout,
        )
        subprocess.run(["git", "-C", str(ws), "add", "-A"], capture_output=True)
        diff = subprocess.run(
            ["git", "-C", str(ws), "-c", "core.quotepath=false", "diff", "--cached"],
            capture_output=True, text=True,
        )
    status = {
        "instance_id": iid,
        "agent_exit_code": res.returncode,
        "stdout_tail": res.stdout[-4000:],
        "stderr_tail": res.stderr[-4000:],
    }
    pred = {
        "instance_id": iid,
        "model_name_or_path": args.model_name,
        "model_patch": diff.stdout,
    }
    return pred, status


def run_instance(inst: dict, args: argparse.Namespace) -> tuple[dict, dict]:
    if args.gold:
        pred = {
            "instance_id": inst["instance_id"],
            "model_name_or_path": f"{args.model_name}-gold",
            "model_patch": inst["patch"],
        }
        return pred, {"instance_id": inst["instance_id"], "agent_exit_code": 0, "gold": True}
    if args.mode == "local":
        return run_instance_local(inst, args)
    return run_instance_docker(inst, args)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dataset", default="princeton-nlp/SWE-bench_Lite")
    ap.add_argument("--split", default="test")
    ap.add_argument("--limit", type=int, default=25, help="0 = alle Instanzen")
    ap.add_argument("--slice", default="", help="z.B. 0:50")
    ap.add_argument("--instance-id", action="append", default=[])
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--max-steps", type=int, default=agentkit_max_steps())
    ap.add_argument("--mode", choices=["docker", "local"], default="docker")
    ap.add_argument("--gold", action="store_true",
                    help="Gold-Patches statt Agent — Sanity-Check der Eval-Pipeline")
    ap.add_argument("--provider", default=agentkit_provider(),
                    help="auto|azure|openai|demo (demo = offline Plumbing-Test)")
    ap.add_argument("--run-id",
                    default=datetime.datetime.now().strftime("%Y%m%d-%H%M%S"))
    ap.add_argument("--task-timeout", type=int, default=1800, help="Sekunden pro Instanz")
    ap.add_argument("--cleanup-images", action="store_true",
                    help="Per-Instance-Image nach dem Lauf löschen (spart Disk)")
    ap.add_argument("--model-name", default=bench_model_name())
    args = ap.parse_args()

    out = results_dir("swebench", args.run_id)
    preds_path = out / "preds.jsonl"
    logs_dir = out / "logs"
    logs_dir.mkdir(exist_ok=True)

    done: set[str] = set()
    if preds_path.exists():  # resumable: bereits gelaufene Instanzen überspringen
        with preds_path.open() as f:
            done = {json.loads(line)["instance_id"] for line in f if line.strip()}

    instances = [r for r in load_instances(args) if r["instance_id"] not in done]
    console.print(f"[bold]{len(instances)}[/bold] Instanzen zu laufen "
                  f"({len(done)} bereits in {preds_path})")

    (out / "metadata.json").write_text(json.dumps({
        "run_id": args.run_id,
        "dataset": args.dataset,
        "split": args.split,
        "mode": args.mode,
        "provider": args.provider,
        "model_name": args.model_name,
        "openai_model": __import__("os").environ.get("OPENAI_MODEL", ""),
        "base_url_set": bool(__import__("os").environ.get("OPENAI_BASE_URL")),
        "max_steps": args.max_steps,
        "gold": args.gold,
        "started_at": datetime.datetime.now().isoformat(),
    }, indent=2))

    n_ok = n_err = n_empty = 0
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = {pool.submit(run_instance, inst, args): inst for inst in instances}
        for fut in as_completed(futures):
            iid = futures[fut]["instance_id"]
            try:
                pred, status = fut.result()
            except Exception as e:
                n_err += 1
                console.print(f"[red]FEHLER[/red] {iid}: {e}")
                (logs_dir / f"{iid}.error.txt").write_text(str(e))
                continue
            with preds_path.open("a") as f:
                f.write(json.dumps(pred) + "\n")
            (logs_dir / f"{iid}.json").write_text(json.dumps(status, indent=2))
            if pred["model_patch"].strip():
                n_ok += 1
                console.print(f"[green]patch[/green] {iid} "
                              f"({len(pred['model_patch'])} bytes, "
                              f"exit {status['agent_exit_code']})")
            else:
                n_empty += 1
                console.print(f"[yellow]leer[/yellow]  {iid} "
                              f"(exit {status['agent_exit_code']})")

    t = Table(title=f"SWE-bench Lauf {args.run_id}")
    t.add_column("mit Patch"); t.add_column("leer"); t.add_column("Fehler")
    t.add_row(str(n_ok), str(n_empty), str(n_err))
    console.print(t)
    console.print(
        f"\nAuswertung (Cloud, kostenloser Key via `sb-cli gen-api-key <email>`):\n"
        f"  uv run sb-cli submit swe-bench_lite test "
        f"--predictions_path {preds_path} --run_id {args.run_id}\n"
        f"Lokal (x86_64, ~120 GB Disk, extra `local-eval` installieren):\n"
        f"  uv run --extra local-eval python -m swebench.harness.run_evaluation "
        f"--dataset_name {args.dataset} --predictions_path {preds_path} "
        f"--max_workers 8 --run_id {args.run_id}"
    )
    return 0 if n_err == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
