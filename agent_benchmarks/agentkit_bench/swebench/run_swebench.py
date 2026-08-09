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
import re
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
    bench_graph_dir,
    bench_graph_enabled,
    bench_graph_scope,
    bench_ctx_enabled,
    bench_graph_shared,
    bench_model_name,
    bench_trace_enabled,
    bench_work_enabled,
    bench_work_max_items,
    benchmark_prompt_path,
    binary_path,
    graph_addendum_path,
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
# Beobachtungs-Mount: hier hinein schreibt der Agent Trace und Graph. Das
# Host-Gegenstück ist <results>/swebench/<run-id>/<instance>, liegt also im
# selben Baum wie die Harbor-Ergebnisse — `agentkit viz --trace
# $BENCH_RESULTS_DIR` sieht damit ALLE Benchmarks als Sitzungen.
OUT_MOUNT = "/agentkit-out"
# Der laufübergreifende Graph: EIN Host-Verzeichnis, in jede Instanz gemountet.
GRAPH_MOUNT = "/agentkit-graph"
GRAPH_PROMPT_DEST = "/agentkit_graph_addendum.md"

TASK_TEMPLATE = """You are working in a Python repository checked out at the current \
working directory. The repository has a fully set-up development environment.

Below is a real GitHub issue from this repository:

<issue>
{problem_statement}
</issue>

Fix the issue by modifying the repository's source code. Do not modify any test \
files. Verify your fix by running tests: the targeted ones first, then the whole \
test file or module you touched — a fix that repairs the reported bug and breaks \
a neighbouring test counts as a failure, and the targeted test cannot see that. \
Delegate those runs to a `tester` sub-agent, and have a `reviewer` sub-agent \
read your change before you finish. When you are done, briefly state what you \
changed — the harness collects your changes via git diff, so do not print the \
diff yourself."""


# --------------------------------------------------- Diff säubern, bevor er zählt
# SWE-bench spielt VOR dem Modell-Patch seinen eigenen `test_patch` ein. Ändert der
# Agent dieselbe Testdatei, kollidieren beide und der GESAMTE Patch wird verworfen —
# auch der korrekte Quellcode darin. Gemessen am Lauf 2026-08-08: 4 von 40 Instanzen
# fielen so durch, und drei der Fixes waren nachweislich richtig (nach dem Filtern
# stieg `resolved` im Mittel von 2,5 auf 3,25). Dazu kommen Wegwerf-Skripte, die
# `git add -A` einsammelt und die im Diff nichts verloren haben.
#
# Bewertet werden ausschließlich Quelländerungen; alles andere ist Messrauschen.
# Der Agent bekommt dieselbe Regel seit v0.21 auch als harte Sperre mit
# (`--protect-paths`), aber der Filter bleibt: Er gilt auch für Patches, die über
# `run_shell` entstanden sind, und macht Altläufe vergleichbar.

DIFF_KOPF = re.compile(r"^diff --git a/(\S+) b/(\S+)$")


def _ist_testdatei(pfad: str) -> bool:
    teile = pfad.replace("\\", "/").split("/")
    name = teile[-1]
    return (
        "tests" in teile
        or "testing" in teile
        or name.startswith("test_")
        or name.endswith(("_test.py", "_tests.py"))
        or name == "conftest.py"
    )


def _ist_kladde(pfad: str) -> bool:
    name = pfad.replace("\\", "/").split("/")[-1]
    return name.startswith(("tmp_", "repro", "scratch", "check_")) or name.endswith(
        (".orig", ".rej", ".log")
    )


def diff_saeubern(patch: str) -> tuple[str, list[str]]:
    """(bereinigter Patch, entfernte Pfade) — Abschnitt für Abschnitt."""
    behalten: list[str] = []
    entfernt: list[str] = []
    pfad: str | None = None
    puffer: list[str] = []

    def abschluss() -> None:
        if pfad is None:
            return
        if _ist_testdatei(pfad) or _ist_kladde(pfad):
            entfernt.append(pfad)
        else:
            behalten.append("".join(puffer))

    for zeile in patch.splitlines(keepends=True):
        m = DIFF_KOPF.match(zeile.rstrip("\n"))
        if m:
            abschluss()
            pfad, puffer = m.group(2), [zeile]
        elif pfad is not None:
            puffer.append(zeile)
    abschluss()
    return "".join(behalten), entfernt


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


# Tatsachen über DIESE Umgebung — sie gehören in den Auftrag, nicht in den
# System-Prompt: Der beschreibt, wie der Agent arbeitet, und ist für jeden Lauf
# derselbe. Was hier steht, gilt nur für diesen Benchmark.
#
# Beide Punkte kamen früher über --system-file mit und fielen weg, als der
# Benchmark auf den eigenen Prompt des Agenten umgestellt wurde. Gemessen im
# Lauf einprompt (64 Polyglot-Aufgaben): die graph_*-Aufrufe fielen von 235 auf
# 4, graph_remember/graph_promote auf NULL, und 50 von 64 Schlussworten waren
# deutsch. Der Graph-Block im Prompt erklärt die WERKZEUGE; dass der Graph über
# alle Aufgaben geteilt wird, stand nur im Benchmark-Prompt — und ohne das hat
# ein Agent keinen Grund, für einen Nachfolger zu schreiben, den es aus seiner
# Sicht nicht gibt.
SPRACHE = """

Work and write in English — the repository, its tests and its history are English, and so are the people who will read your change."""

GETEILTER_GRAPH = """

Your `graph_*` tools share ONE knowledge graph across all tasks of this benchmark run: earlier workers recorded what they found out, and whatever you record is handed to the workers after you. Search it before you work something out yourself, and record what you learned before you finish — you are one worker in a series, not a one-off."""


def umgebungshinweise() -> str:
    """Die Zusätze, die zu DIESEM Lauf gehören (Sprache; Graph; Team).

    Alles hier landet im AUFTRAG, nicht im System-Prompt — seit dem Umstieg auf
    den eigenen Prompt des Agenten ist das der einzige Kanal, der ankommt.

    Genau daran ist der Swarm-Modus vorher gescheitert: `teamlead_bench.md`
    wurde in jeden Container hochgeladen, zu `system_full.md` zusammengefügt —
    und dann nie übergeben, weil `--system-file` im Kommando fehlte. Im Lauf
    2026-08-08 hatte der „Tech-Lead" also nie Team-Instruktionen; entsprechend
    gingen nur 4,5 % seiner Delegationen an `architect`/`developer`. Was nicht
    im Auftrag steht, existiert für den Agenten nicht.
    """
    text = SPRACHE
    if bench_graph_enabled() and bench_graph_shared():
        text += GETEILTER_GRAPH
    if swarm_enabled():
        text += "\n\n" + swarm_prompt_path().read_text(encoding="utf-8").strip()
    return text


def render_task(inst: dict) -> str:
    return (
        TASK_TEMPLATE.format(problem_statement=inst["problem_statement"].strip())
        + umgebungshinweise()
    )


def graph_scope() -> str:
    """`--graph-scope` — nur wenn es überhaupt einen Graphen gibt."""
    if not bench_graph_enabled():
        return ""
    return f"--graph-scope {shlex.quote(bench_graph_scope())} "


def agent_command(max_steps: int, provider: str, workspace: str) -> str:
    # --no-project-instructions in JEDEM Zweig: SWE-bench-Repos bringen
    # zunehmend selbst eine AGENTS.md mit, und die laedt agentkit seit v0.21
    # standardmaessig in den System-Prompt (plus Leitplanken fuer run_shell).
    # Ohne das Flag saehe der Prompt in jeder Instanz anders aus — die Laeufe
    # waeren nicht mehr untereinander vergleichbar. Das Flag ist der
    # ausdrueckliche Ersatz fuer den frueheren Schutz durch den exotischen
    # Dateinamen AGENTKIT.md.
    # </dev/null: agentkit liest non-TTY-stdin bis EOF — ohne Redirect hängt es.
    # --agents-only: sonst stehen neben den Team-Rollen weiter die eingebauten
    # (`explorer`/`tester`/`reviewer`), und das Modell greift zu den Namen, die
    # es kennt — im Lauf 2026-08-08 gingen 95 % der Delegationen dorthin.
    agents = f"--agents {ROLES_DEST} --agents-only " if swarm_enabled() else ""
    # Testdateien sind für den Agenten gesperrt. Der Auftrag verbietet sie
    # ohnehin, nur hielt sich niemand daran: 4 von 40 Instanzen fielen deshalb
    # durch (siehe `diff_saeubern`). Eine Regel im Werkzeug kann man nicht
    # übergehen.
    schutz = "--protect-paths 'tests/**,test_*.py,*_test.py,conftest.py,testing/**' "
    # Trace und Graph landen im gemounteten Verzeichnis, sind also schon
    # WÄHREND des Laufs auf dem Host lesbar (siehe OUT_MOUNT).
    beobachtung = ""
    if bench_trace_enabled():
        beobachtung += f"--trace {OUT_MOUNT}/trace "
    if bench_graph_enabled():
        # Geteilt: ein eigener Mount auf das Laufverzeichnis, den ALLE Instanzen
        # sehen. Sonst der Graph dieser einen Instanz.
        beobachtung += f"--graph {GRAPH_MOUNT if bench_graph_shared() else OUT_MOUNT + '/graph'} "
    if bench_ctx_enabled():
        # Im Work-Modus legt agentkit darunter ein Verzeichnis JE VERSUCH an.
        beobachtung += f"--ctx {OUT_MOUNT}/ctx "
    if bench_work_enabled():
        # Work-Runtime statt eines Agentenlaufs: zerlegen, dann abarbeiten.
        # `work create` gibt die Projekt-ID auf stdout aus — letzte Zeile.
        # Das Work-Verzeichnis liegt im Beobachtungs-Mount, der Work-Reiter im
        # Betrachter findet es damit neben dem Trace.
        return (
            f'PID=$({BINARY_DEST} work create --title "SWE-bench" --objective "$SWE_TASK" '
            f"-w {shlex.quote(workspace)} --dir {OUT_MOUNT}/work "
            f"--max-items {bench_work_max_items()} --max-steps {max_steps} "
            f"</dev/null | tail -1); "
            f'{BINARY_DEST} work run "$PID" -w {shlex.quote(workspace)} --dir {OUT_MOUNT}/work '
            f"-y --steps --no-project-instructions "
            f"--provider {provider} --max-steps {max_steps} "
            f"{schutz}{graph_scope()}{beobachtung}</dev/null"
        )
    # --steps statt -p: stdout bleibt final-only (gepipte Ausgabe), stderr trägt
    # den Tool-Trace in stderr_tail — sonst sind Fehlläufe nicht diagnostizierbar.
    return (
        f'{BINARY_DEST} --steps "$SWE_TASK" -w {shlex.quote(workspace)} -y --no-color --verify '
        f"--no-project-instructions "
        f"--shell-timeout 600 --provider {provider} --max-steps {max_steps} "
        f"{agents}{schutz}{graph_scope()}{beobachtung}</dev/null"
    )


def run_instance_docker(inst: dict, args: argparse.Namespace) -> tuple[dict, dict]:
    iid = inst["instance_id"]
    image = image_for(iid)
    plat = "linux/amd64" if platform.machine() not in ("x86_64", "AMD64") else None
    env = agentkit_container_env() | {"SWE_TASK": render_task(inst)}
    mounts = []
    if bench_trace_enabled() or bench_graph_enabled():
        mounts.append((results_dir("swebench", args.run_id) / iid, OUT_MOUNT))
    if bench_graph_enabled() and bench_graph_shared():
        # EIN Host-Verzeichnis für jede Instanz und für JEDEN Lauf (siehe
        # bench_graph_dir) — daher --workers 1 (siehe main()): der Graph-Store
        # kompaktiert sein Journal und verträgt keine zwei Schreiber.
        mounts.append((bench_graph_dir(), GRAPH_MOUNT))
    with SwebenchContainer(image, platform=plat, mounts=mounts) as c:
        c.copy_in(binary_path(), BINARY_DEST)
        # Keine Prompt-Dateien mehr: agentkit läuft mit seinem eigenen
        # System-Prompt, alles Lauf-Spezifische steht im Auftrag (siehe
        # `umgebungshinweise`). Die früher hier zusammengefügte `system_full.md`
        # wurde nie übergeben — siehe die Begründung im Harbor-Adapter.
        if swarm_enabled():
            # docker cp kopiert Verzeichnisse rekursiv — die Team-Rollen als Ganzes.
            c.copy_in(swarm_roles_dir(), ROLES_DEST)
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
    roh = diff.stdout if diff.returncode == 0 else ""
    patch, entfernt = diff_saeubern(roh)
    status = {
        "instance_id": iid,
        "agent_exit_code": res.returncode,
        "stdout_tail": res.stdout[-4000:],
        "stderr_tail": res.stderr[-4000:],
        # Was der Filter wegnahm, gehört ins Protokoll: ein stiller Eingriff in
        # die Messung wäre keiner, den man später noch prüfen kann.
        "gefiltert": entfernt,
        "patch_roh_bytes": len(roh),
    }
    if entfernt:
        console.print(f"[dim]{iid}: {len(entfernt)} Abschnitt(e) gefiltert: "
                      f"{', '.join(entfernt)}[/dim]")
    pred = {
        "instance_id": iid,
        "model_name_or_path": args.model_name,
        "model_patch": patch,
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
        # Ohne Container kein Mount — der Agent schreibt direkt an denselben
        # Ort, den der Docker-Pfad gemountet hätte.
        beobachtung = ""
        ziel = results_dir("swebench", args.run_id) / iid
        if bench_trace_enabled():
            beobachtung += f"--trace {shlex.quote(str(ziel / 'trace'))} "
        if bench_graph_enabled():
            graph = bench_graph_dir() if bench_graph_shared() else ziel / "graph"
            beobachtung += f"--graph {shlex.quote(str(graph))} "
        cmd = (
            f'{host_bin} --steps "$SWE_TASK" -w {shlex.quote(str(ws))} -y --no-color --verify '
            f"--no-project-instructions "
            f"--shell-timeout 600 --provider {args.provider} --max-steps {args.max_steps} "
            f"{agents}{beobachtung}</dev/null"
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

    # Ein GETEILTER Graph verträgt keine parallelen Worker: alle Instanzen
    # schreiben dasselbe Journal, und der Store kompaktiert es (schreibt die
    # Datei also neu). Der Modus verlangt Reihenfolge ohnehin — ein späterer
    # Task kann nur profitieren, wenn der frühere fertig ist.
    if bench_graph_enabled() and bench_graph_shared() and args.workers > 1:
        console.print(
            f"[yellow]--workers {args.workers} auf 1 gesetzt: der geteilte "
            f"Wissensgraph (BENCH_GRAPH_SHARED=1) braucht sequenzielle Läufe. "
            f"Mit BENCH_GRAPH_SHARED=0 bekommt jede Instanz ihren eigenen "
            f"Graphen und die Worker bleiben parallel.[/yellow]"
        )
        args.workers = 1

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
