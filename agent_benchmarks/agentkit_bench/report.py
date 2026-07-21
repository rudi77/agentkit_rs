"""Sammelt alle Läufe unter results/ in eine Markdown-Übersicht.

    uv run python -m agentkit_bench.report            # -> results/summary.md + stdout
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from agentkit_bench.config import results_root

RESULTS = results_root()


def swebench_rows() -> list[dict]:
    rows = []
    for run in sorted((RESULTS / "swebench").glob("*/")):
        preds = run / "preds.jsonl"
        if not preds.exists():
            continue
        meta = {}
        if (run / "metadata.json").exists():
            meta = json.loads((run / "metadata.json").read_text())
        n = n_patch = 0
        for line in preds.read_text().splitlines():
            if not line.strip():
                continue
            n += 1
            if json.loads(line).get("model_patch", "").strip():
                n_patch += 1
        # sb-cli-Report (falls per `sb-cli get-report` daneben gelegt)
        resolved = ""
        for rep in run.glob("*report*.json"):
            try:
                d = json.loads(rep.read_text())
                resolved = str(d.get("resolved", d.get("resolved_instances", "")))
            except Exception:
                pass
        rows.append({
            "benchmark": "SWE-bench Lite",
            "run_id": run.name,
            "model": meta.get("openai_model") or meta.get("model_name", "?"),
            "n": n,
            "score": f"resolved: {resolved}" if resolved else f"Patches: {n_patch}/{n}",
        })
    return rows


def harbor_rows(subdir: str, label: str) -> list[dict]:
    rows = []
    base = RESULTS / subdir
    if not base.exists():
        return rows
    # harbor legt pro Job ein Verzeichnis mit result.json (JobResult) an;
    # Trial-Verzeichnisse enthalten je ein result.json (TrialResult).
    for job_result in sorted(base.rglob("result.json")):
        try:
            d = json.loads(job_result.read_text())
        except Exception:
            continue
        if "stats" not in d:  # TrialResult überspringen, nur JobResult zählen
            continue
        stats = d.get("stats") or {}
        evals = stats.get("evals") or {}
        score = ""
        # evals enthält Reward-Aggregationen, z.B. {"reward": {"mean": ..}}
        for key, agg in evals.items():
            if isinstance(agg, dict) and "mean" in agg:
                score = f"{key}: {agg['mean']:.3f}"
                break
        rows.append({
            "benchmark": label,
            "run_id": job_result.parent.name,
            "model": "",
            "n": d.get("n_total_trials", ""),
            "score": score or f"completed: {stats.get('n_completed_trials', '?')}",
        })
    return rows


def main() -> int:
    rows = (
        swebench_rows()
        + harbor_rows("terminal_bench", "Terminal-Bench 2.0")
        + harbor_rows("polyglot", "Aider Polyglot")
    )
    if not rows:
        print("Keine Ergebnisse unter results/ gefunden.", file=sys.stderr)
        return 1
    lines = [
        "# Benchmark-Ergebnisse",
        "",
        "| Benchmark | Run | Modell | Tasks | Ergebnis |",
        "|---|---|---|---|---|",
    ]
    for r in rows:
        lines.append(
            f"| {r['benchmark']} | {r['run_id']} | {r['model']} | {r['n']} | {r['score']} |"
        )
    out = "\n".join(lines) + "\n"
    (RESULTS / "summary.md").write_text(out)
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
