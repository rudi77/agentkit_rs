"""Lern-Messung: braucht der zweite Durchgang weniger als der erste?

Die übrigen Benchmarks (SWE-bench, Terminal-Bench, Polyglot) stellen jede
Aufgabe in einem frischen Container. Ein Gedächtnis kann dort nichts
beitragen, was über die Aufgabe hinausgeht — und die Messungen zeigen genau
das. Dieser Treiber misst die Frage, für die das Gedächtnis gebaut ist:

    Zwei Aufgaben DERSELBEN FAMILIE laufen nacheinander im SELBEN Workspace.
    Wird die zweite billiger, wenn der Agent sich an die erste erinnern kann?

Das Projekt (`fixture/`) hat Konventionen, die nirgends stehen und die jede
Aufgabe der Familie braucht: wie die Checks laufen, wo eine Regel registriert
wird, wo ihre Einstellungen liegen. Der erste Durchgang muss sie herausfinden.
Ob der zweite sie noch einmal herausfinden muss, ist der Unterschied zwischen
den Armen:

    nograph      --no-graph              (Vergleichsbasis: kein Gedächtnis)
    graph        (ohne Flag)             (Gedächtnis an, Modell merkt selbst)
    consolidate  --consolidate           (dazu Konsolidierung am Sitzungsende)

Jeder Versuch bekommt eine frische Kopie des Projekts UND ein frisches
`AGENTKIT_HOME` — sonst trüge ein Versuch dem nächsten Wissen zu, und die
Arme wären nicht mehr unabhängig. Die Reihenfolge der Aufgaben wird
durchgetauscht, damit „die zweite Aufgabe ist einfach leichter" keine
Erklärung sein kann.

Kein Docker: die Aufgaben sind reines Python ohne Abhängigkeiten. Der Agent
läuft mit `-y` in einem Temp-Verzeichnis auf dem Host.

    python -m agentkit_bench.learning.run_learning --gold      # ohne Modell: Aufgabensatz prüfen
    python -m agentkit_bench.learning.run_learning --repeats 3 # die Messung
"""

from __future__ import annotations

import argparse
import itertools
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent  # agent_benchmarks/
FIXTURE = HERE / "fixture"
TASKS = HERE / "tasks"

ARMS = {
    "nograph": ["--no-graph"],
    "graph": [],
    "consolidate": ["--consolidate"],
}

# Was über DIESE Umgebung gilt, steht im Auftrag, nicht im System-Prompt —
# dieselbe Linie wie in den anderen Treibern.
PREAMBLE = (
    "You are working in the repository in the current workspace. "
    "It is a plain Python project without third-party dependencies; `python3` is available.\n\n"
)


def task_names() -> list[str]:
    return sorted(p.name for p in TASKS.iterdir() if p.is_dir())


def find_binary() -> Path:
    """Das agentkit-Binary: ausdrücklich genannt, sonst der jüngste lokale Build."""
    explicit = os.environ.get("AGENTKIT_BINARY_PATH")
    candidates = [Path(explicit)] if explicit else []
    repo = ROOT.parent
    candidates += [
        repo / "agentkit_app" / "target" / "release" / "agentkit",
        ROOT / "build" / "agentkit-x86_64-musl",
        repo / "agentkit_app" / "target" / "debug" / "agentkit",
    ]
    for path in candidates:
        if path.is_file():
            # Absolut: agentkit läuft mit dem Workspace als Arbeitsverzeichnis.
            return path.resolve()
    raise FileNotFoundError(
        "agentkit-Binary fehlt. Bauen mit\n"
        "  cargo build --release --manifest-path agentkit_app/Cargo.toml "
        '--features "pdf ctxman tiktoken graph work viz"\n'
        "oder AGENTKIT_BINARY_PATH setzen."
    )


def fresh_workspace(workspace: Path) -> None:
    """Eine frische Kopie des Projekts — ohne Bytecode-Reste früherer Läufe."""
    shutil.copytree(FIXTURE, workspace, ignore=shutil.ignore_patterns("__pycache__"))


def verify(workspace: Path, task: str) -> bool:
    """Legt die versteckte Spec der Aufgabe ins Projekt und lässt dessen Checks laufen."""
    for spec in (TASKS / task / "hidden").glob("*.py"):
        shutil.copy(spec, workspace / "spec" / spec.name)
    try:
        result = subprocess.run(
            [sys.executable, "tools/check.py"],
            cwd=workspace, capture_output=True, text=True, timeout=120,
        )
    except subprocess.TimeoutExpired:
        return False
    return result.returncode == 0


def apply_gold(workspace: Path, task: str) -> None:
    """Die Referenzlösung: Modul, Manifest-Zeile, Einstellungen."""
    rules = workspace / "ledger" / "rules"
    shutil.copy(TASKS / task / "gold" / f"{task}.py", rules / f"{task}.py")
    with (rules / "manifest.txt").open("a") as manifest:
        manifest.write(f"{task}\n")
    limits = json.loads((rules / "limits.json").read_text())
    limits.update(json.loads((TASKS / task / "gold" / "settings.json").read_text()))
    (rules / "limits.json").write_text(json.dumps(limits, indent=2) + "\n")


def metrics_from_trace(trace_dir: Path) -> dict:
    """Schritte, Tool-Aufrufe und Tokens eines Laufs aus seinem NDJSON-Trace.

    Gezählt wird, was bezahlt wird: Tokens ALLER Quellen (auch Sub-Agenten).
    Schritte und Tool-Aufrufe dagegen nur vom Haupt-Agenten — sie sind das Maß
    dafür, wie viel er selbst herausfinden musste.
    """
    out = {"steps": 0, "tool_calls": 0, "graph_calls": 0, "input_tokens": 0, "output_tokens": 0}
    for path in sorted(trace_dir.glob("*.jsonl")):
        for line in path.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            etype, data = event.get("etype"), event.get("data")
            main = not event.get("source")
            if etype == "step" and main:
                out["steps"] += 1
            elif etype == "tool_call" and main and isinstance(data, dict):
                name = (data.get("tool_call") or {}).get("name", "")
                out["tool_calls"] += 1
                out["graph_calls"] += name.startswith("graph_")
            elif etype == "token_usage" and isinstance(data, dict):
                usage = data.get("token_usage") or {}
                out["input_tokens"] += usage.get("input_tokens", 0)
                out["output_tokens"] += usage.get("output_tokens", 0)
    return out


def run_agent(binary: Path, workspace: Path, home: Path, trace: Path, task: str,
              arm: str, args: argparse.Namespace) -> dict:
    """Ein Durchgang: eine Aufgabe, ein agentkit-Aufruf, danach die Prüfung."""
    instruction = PREAMBLE + (TASKS / task / "instruction.md").read_text()
    env = dict(os.environ, AGENTKIT_HOME=str(home))
    command = [
        str(binary), "-p", "-y", "-w", str(workspace),
        "--provider", args.provider, "--max-steps", str(args.max_steps),
        "--trace", str(trace), *ARMS[arm], *args.agent_flags.split(),
        "--", instruction,
    ]
    started = time.monotonic()
    try:
        # stdin geschlossen: agentkit liest einen Nicht-TTY-stdin bis EOF.
        proc = subprocess.run(command, cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=args.timeout)
        exit_code, stderr = proc.returncode, proc.stderr
    except subprocess.TimeoutExpired as expired:
        teil = expired.stderr or ""
        stderr = teil.decode(errors="replace") if isinstance(teil, bytes) else teil
        exit_code = 124
    seconds = round(time.monotonic() - started, 1)
    (trace.parent / f"{trace.name}.stderr.log").write_text(stderr)
    return {
        "task": task, "exit_code": exit_code, "seconds": seconds,
        "passed": verify(workspace, task), **metrics_from_trace(trace),
    }


def graph_claims(home: Path, workspace: Path, binary: Path) -> str:
    """Stand des Gedächtnisses nach einem Durchgang — für das Protokoll."""
    try:
        out = subprocess.run([str(binary), "graph", "status", "-w", str(workspace)],
                             env=dict(os.environ, AGENTKIT_HOME=str(home)),
                             capture_output=True, text=True, timeout=30).stdout
    except (subprocess.TimeoutExpired, OSError):
        return ""
    return out.strip().splitlines()[0] if out.strip() else ""


def run_trial(binary: Path, out_dir: Path, arm: str, first: str, second: str,
              repeat: int, args: argparse.Namespace) -> dict:
    name = f"{arm}-{first}-{second}-{repeat}"
    trial_dir = out_dir / name
    trial_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="agentkit-learning-") as tmp:
        workspace, home = Path(tmp) / "ws", Path(tmp) / "home"
        fresh_workspace(workspace)
        home.mkdir()
        passes = []
        for index, task in enumerate((first, second), start=1):
            result = run_agent(binary, workspace, home, trial_dir / f"trace-{index}", task, arm, args)
            result["memory"] = graph_claims(home, workspace, binary) if arm != "nograph" else ""
            passes.append(result)
            print(f"  {name} Durchgang {index} ({task}): "
                  f"{'ok' if result['passed'] else 'NICHT GELÖST'}, "
                  f"{result['steps']} Schritte, {result['tool_calls']} Tool-Aufrufe, "
                  f"{result['input_tokens'] + result['output_tokens']} Tokens", flush=True)
        # Das Bundle bleibt als Beleg liegen: was hat der Agent sich gemerkt?
        bundle = workspace / ".agentkit" / "graph"
        if bundle.is_dir():
            shutil.copytree(bundle, trial_dir / "graph", dirs_exist_ok=True)
    return {"trial": name, "arm": arm, "repeat": repeat, "first": passes[0], "second": passes[1]}


def gold_check() -> int:
    """Prüft den Aufgabensatz ohne Modell: die Referenzlösung besteht, ohne sie scheitert die Prüfung.

    Beides muss gelten, sonst misst der Satz nichts — eine Aufgabe, die ohne
    Lösung besteht, zählt jeden Durchgang als Erfolg; eine, deren Referenz
    scheitert, keinen.
    """
    failures = 0
    names = task_names()
    for first, second in itertools.permutations(names, 2):
        with tempfile.TemporaryDirectory(prefix="agentkit-learning-gold-") as tmp:
            workspace = Path(tmp) / "ws"
            fresh_workspace(workspace)
            for task in (first, second):
                unsolved = verify(workspace, task)
                apply_gold(workspace, task)
                solved = verify(workspace, task)
                ok = solved and not unsolved
                failures += not ok
                print(f"{'ok  ' if ok else 'FEHLER'} {first} → {second}: {task} "
                      f"(ohne Lösung {'bestanden' if unsolved else 'gescheitert'}, "
                      f"mit Lösung {'bestanden' if solved else 'gescheitert'})")
    print("Aufgabensatz in Ordnung." if not failures else f"{failures} Fehler im Aufgabensatz.")
    return 1 if failures else 0


FIELDS = ["steps", "tool_calls", "input_tokens", "output_tokens", "seconds"]


def mean(values: list[float]) -> float:
    return round(statistics.fmean(values), 1) if values else 0.0


def summarize(trials: list[dict]) -> str:
    """Je Arm: Mittelwerte des ersten und zweiten Durchgangs und die Ersparnis.

    Die Ersparnis ist der Befund — und nur innerhalb eines Arms aussagekräftig
    im Vergleich mit `nograph`: auch ohne Gedächtnis kann der zweite Durchgang
    billiger sein (das Projekt enthält dann schon eine zweite Beispiel-Regel).
    Was das Gedächtnis beiträgt, ist die Differenz der Ersparnisse.
    """
    lines = [
        "# Lern-Messung",
        "",
        "Zwei Aufgaben derselben Familie nacheinander im selben Workspace. "
        "Mittelwerte je Arm; „Ersparnis“ = zweiter gegenüber erstem Durchgang.",
        "",
        "| Arm | Versuche | gelöst 1 | gelöst 2 | Schritte 1 → 2 | Tool-Aufrufe 1 → 2 "
        "| Tokens 1 → 2 | Ersparnis Tokens | Graph-Aufrufe 2 |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    for arm in ARMS:
        rows = [t for t in trials if t["arm"] == arm]
        if not rows:
            continue
        def avg(which: str, field: str) -> float:
            return mean([t[which][field] for t in rows])
        def tokens(which: str) -> float:
            return mean([t[which]["input_tokens"] + t[which]["output_tokens"] for t in rows])
        saved = (1 - tokens("second") / tokens("first")) * 100 if tokens("first") else 0.0
        lines.append(
            f"| {arm} | {len(rows)} "
            f"| {sum(t['first']['passed'] for t in rows)}/{len(rows)} "
            f"| {sum(t['second']['passed'] for t in rows)}/{len(rows)} "
            f"| {avg('first', 'steps')} → {avg('second', 'steps')} "
            f"| {avg('first', 'tool_calls')} → {avg('second', 'tool_calls')} "
            f"| {tokens('first'):.0f} → {tokens('second'):.0f} "
            f"| {saved:.1f} % | {avg('second', 'graph_calls')} |"
        )
    lines += [
        "",
        "Lesehilfe: Der Arm `nograph` zeigt, wie viel der zweite Durchgang ohnehin "
        "spart. Lernt der Agent, liegt die Ersparnis der anderen Arme darüber — "
        "bei mindestens gleicher Lösungsquote. Die Streuung von Lauf zu Lauf ist "
        "groß; unter drei Wiederholungen je Reihenfolge ist die Tabelle ein Eindruck, "
        "keine Messung.",
    ]
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--gold", action="store_true",
                        help="Aufgabensatz ohne Modell prüfen (Referenzlösungen)")
    parser.add_argument("--arms", default=",".join(ARMS),
                        help=f"kommagetrennt aus: {', '.join(ARMS)}")
    parser.add_argument("--repeats", type=int, default=1,
                        help="Wiederholungen je Arm und Reihenfolge (für eine Messung: ≥ 3)")
    parser.add_argument("--orders", type=int, default=0,
                        help="nur die ersten N Aufgaben-Reihenfolgen (0 = alle sechs)")
    parser.add_argument("--provider", default=os.environ.get("AGENTKIT_PROVIDER", "openai"))
    parser.add_argument("--max-steps", type=int, default=60)
    parser.add_argument("--timeout", type=int, default=900, help="Sekunden je Durchgang")
    parser.add_argument("--agent-flags", default=os.environ.get("BENCH_AGENT_FLAGS", ""),
                        help="zusätzliche agentkit-Optionen für alle Arme")
    parser.add_argument("--run-id", default=time.strftime("%Y%m%d-%H%M%S"))
    args = parser.parse_args()

    if args.gold:
        return gold_check()

    try:  # Dieselbe .env wie die anderen Treiber, falls python-dotenv da ist.
        from dotenv import load_dotenv
        load_dotenv(ROOT / ".env")
    except ImportError:
        pass

    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    unknown = [a for a in arms if a not in ARMS]
    if unknown:
        parser.error(f"unbekannter Arm: {', '.join(unknown)}")
    binary = find_binary()
    orders = list(itertools.permutations(task_names(), 2))
    if args.orders:
        orders = orders[: args.orders]
    out_dir = Path(os.environ.get("BENCH_RESULTS_DIR", ROOT / "results")) / "learning" / args.run_id
    out_dir.mkdir(parents=True, exist_ok=True)
    print(f"Binary: {binary}\nErgebnisse: {out_dir}\n"
          f"{len(arms)} Arme × {len(orders)} Reihenfolgen × {args.repeats} Wiederholungen "
          f"= {len(arms) * len(orders) * args.repeats} Versuche (je zwei Durchgänge)", flush=True)

    trials = []
    with (out_dir / "trials.jsonl").open("w") as log:
        # Arme abwechselnd statt nacheinander: driftet der Anbieter über die
        # Laufzeit (Last, Drosselung), trifft es alle Arme gleich.
        for repeat in range(1, args.repeats + 1):
            for first, second in orders:
                for arm in arms:
                    trial = run_trial(binary, out_dir, arm, first, second, repeat, args)
                    trials.append(trial)
                    log.write(json.dumps(trial, ensure_ascii=False) + "\n")
                    log.flush()
    summary = summarize(trials)
    (out_dir / "summary.md").write_text(summary)
    print("\n" + summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
