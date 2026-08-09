"""Lokale SWE-bench-Auswertung — ohne Cloud, ohne Kontingent.

    uv run python -m agentkit_bench.swebench.eval_local \
        --predictions D:/…/preds.jsonl --run-id smoke-20260722-205659

Warum es das gibt: die Cloud-Auswertung über `sb-cli` hat in diesem Projekt
30 Einreichungen ausnahmslos als `failed` gemeldet — auch einen Patch, der
nachweislich korrekt ist (`django__django-10914`: der `FAIL_TO_PASS`-Test ist
ohne den Patch rot und mit ihm grün). Kein einziger landete in `unresolved`,
der Kategorie für „angewendet, aber Tests rot". Eine Zahl, die einen richtigen
Patch als Fehlschlag führt, misst nicht die Qualität der Patches.

Das Verfahren ist exakt das von SWE-bench, nur hier statt dort:

1. Eval-Image der Instanz besorgen (mit Spiegel-Rückfall, siehe `ensure_image`
   — das CDN von Docker Hub bricht regelmäßig mit EOF ab).
2. `test_patch` des Datensatzes anwenden — er bringt die Tests erst mit.
3. `model_patch` des Laufs anwenden. Schlägt das fehl, ist die Instanz
   `patch_failed` und nicht etwa gelöst.
4. `FAIL_TO_PASS` laufen lassen: müssen alle grün werden.
5. `PASS_TO_PASS` laufen lassen: dürfen nicht kaputtgehen (abschaltbar).

Gelöst = beides grün. Das Erfolgssignal ist der EXIT-CODE des Testlaufs, nicht
die Textausgabe: pytest, Djangos `runtests.py` und sympys `bin/test` liefern
alle 0 genau dann, wenn die ausgewählten Tests durchgelaufen sind. Ausgaben zu
parsen wäre je Projekt eine eigene Fehlerquelle.
"""

from __future__ import annotations

import argparse
import json
import shlex
import sys
from pathlib import Path

from rich.console import Console
from rich.table import Table

from agentkit_bench.config import results_dir
from agentkit_bench.swebench.docker_env import SwebenchContainer, image_for

console = Console(stderr=True)

TEST_PATCH_DEST = "/tmp/test_patch.diff"
MODEL_PATCH_DEST = "/tmp/model_patch.diff"

# Conda-Umgebung der Eval-Images. Ohne sie läuft das falsche Python.
ACTIVATE = "source /opt/miniconda3/bin/activate && conda activate testbed 2>/dev/null; "


def normalize_test_id(test: str) -> str:
    """Djangos Testnamen kommen im unittest-Format `name (pfad.Klasse)`.

    `runtests.py` will den Punkt-Pfad `pfad.Klasse.name`. Ohne die Umformung
    findet es den Test nicht und meldet Erfolg für null gelaufene Tests — der
    gefährlichste aller Ausgänge.
    """
    test = test.strip()
    if test.endswith(")") and " (" in test:
        name, _, rest = test.partition(" (")
        return f"{rest[:-1]}.{name}"
    return test


def test_command(repo: str, tests: list[str]) -> str:
    """Der Testaufruf des jeweiligen Projekts.

    Nur zwei Projekte in SWE-bench Lite weichen von pytest ab; für alles andere
    ist `pytest` richtig. Bewusst eine kleine Tabelle statt der vollständigen
    `MAP_REPO_VERSION_TO_SPECS` des offiziellen Harness: die brauchte das
    `swebench`-Paket als Abhängigkeit, und die Kommandos unterscheiden sich
    innerhalb eines Projekts über die Versionen hinweg nicht.
    """
    if repo == "django/django":
        # shlex.quote wie in den anderen Zweigen. Ohne das zerbrach die
        # Kommandozeile an Test-IDs mit Anführungszeichen: `django__django-10914`
        # kam im Lauf 2026-08-08 als `regression` zurück, obwohl die Ursache
        # `bash: -c: unexpected EOF while looking for matching '` war — die
        # Instanz wurde nie geprüft, nur falsch etikettiert.
        ids = " ".join(shlex.quote(normalize_test_id(t)) for t in tests)
        return f"./tests/runtests.py --verbosity 1 {ids}"
    if repo == "sympy/sympy":
        return "bin/test -C --verbose " + " ".join(shlex.quote(t) for t in tests)
    return "python -m pytest -rA -q " + " ".join(shlex.quote(t) for t in tests)


# Die Shell selbst ist gescheitert, nicht der Test. Eine Auswertung darf ihre
# eigenen Pannen nicht dem Agenten anlasten: `django__django-10914` stand im Lauf
# 2026-08-08 als `regression` in der Tabelle, während in `detail` ein
# `unexpected EOF while looking for matching '` lag — die Instanz war nie geprüft
# worden, zählte aber als kaputtgemacht.
SHELL_PANNE = (
    "unexpected EOF while looking for matching",
    "syntax error: unexpected end of file",
    "syntax error near unexpected token",
    "Argument list too long",
)


def shell_kaputt(res) -> str | None:
    """Grund, falls die Kommandozeile selbst zerbrach — sonst None."""
    text = (res.stdout or "") + (res.stderr or "")
    for muster in SHELL_PANNE:
        if muster in text:
            return f"{muster} — Kommandozeile der AUSWERTUNG zerbrochen, Instanz ungeprüft"
    return None


def evaluate(inst: dict, patch: str, args: argparse.Namespace) -> dict:
    """Eine Instanz bewerten. Gibt den Status plus die Belege zurück."""
    iid = inst["instance_id"]
    if not patch.strip():
        return {"instance_id": iid, "status": "empty_patch"}

    with SwebenchContainer(image_for(iid), pull_timeout=args.pull_timeout) as c:
        c.exec("git checkout -q .", workdir="/testbed", timeout=120)
        Path(args.tmp).mkdir(parents=True, exist_ok=True)
        tp = Path(args.tmp) / f"{iid}.test.diff"
        mp = Path(args.tmp) / f"{iid}.model.diff"
        # newline="\n": ein Patch mit CRLF wird von `git apply` abgelehnt, und
        # dieser Treiber läuft unter Windows.
        tp.write_text(inst["test_patch"], encoding="utf-8", newline="\n")
        mp.write_text(patch, encoding="utf-8", newline="\n")
        c.copy_in(tp, TEST_PATCH_DEST)
        c.copy_in(mp, MODEL_PATCH_DEST)

        angewendet = c.exec(f"git apply {TEST_PATCH_DEST}", workdir="/testbed", timeout=120)
        if angewendet.returncode != 0:
            return {"instance_id": iid, "status": "test_patch_failed",
                    "detail": angewendet.stderr[-500:]}
        angewendet = c.exec(f"git apply {MODEL_PATCH_DEST}", workdir="/testbed", timeout=120)
        if angewendet.returncode != 0:
            return {"instance_id": iid, "status": "patch_failed",
                    "detail": angewendet.stderr[-500:]}

        f2p = json.loads(inst["FAIL_TO_PASS"])
        res = c.exec(ACTIVATE + test_command(inst["repo"], f2p),
                     workdir="/testbed", timeout=args.test_timeout)
        if (grund := shell_kaputt(res)):
            return {"instance_id": iid, "status": "eval_error", "detail": grund}
        if res.returncode != 0:
            return {"instance_id": iid, "status": "unresolved",
                    "detail": (res.stdout + res.stderr)[-800:]}

        p2p = json.loads(inst["PASS_TO_PASS"])
        if p2p and not args.skip_pass_to_pass:
            # In Blöcken: manche Instanzen haben hunderte, und eine
            # Kommandozeile hat auch unter Linux eine Grenze.
            for i in range(0, len(p2p), 50):
                res = c.exec(ACTIVATE + test_command(inst["repo"], p2p[i:i + 50]),
                             workdir="/testbed", timeout=args.test_timeout)
                if (grund := shell_kaputt(res)):
                    return {"instance_id": iid, "status": "eval_error", "detail": grund}
                if res.returncode != 0:
                    return {"instance_id": iid, "status": "regression",
                            "detail": (res.stdout + res.stderr)[-800:]}

        return {"instance_id": iid, "status": "resolved"}


def main() -> int:
    ap = argparse.ArgumentParser(description="SWE-bench lokal auswerten")
    ap.add_argument("--predictions", required=True, help="preds.jsonl des Laufs")
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--dataset", default="princeton-nlp/SWE-bench_Lite")
    ap.add_argument("--split", default="test")
    ap.add_argument("--limit", type=int, default=0, help="0 = alle")
    ap.add_argument("--instance-id", action="append", default=[])
    ap.add_argument("--skip-pass-to-pass", action="store_true",
                    help="nur FAIL_TO_PASS prüfen (schneller, weniger streng)")
    ap.add_argument("--test-timeout", type=int, default=1800)
    ap.add_argument("--pull-timeout", type=int, default=3600)
    ap.add_argument("--tmp", default=".eval-tmp")
    args = ap.parse_args()

    preds = {}
    with open(args.predictions, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                row = json.loads(line)
                preds[row["instance_id"]] = row.get("model_patch", "")
    if args.instance_id:
        preds = {k: v for k, v in preds.items() if k in set(args.instance_id)}
    if args.limit > 0:
        preds = dict(list(preds.items())[: args.limit])
    if not preds:
        console.print("[red]Keine Predictions gefunden.[/red]")
        return 1

    from datasets import load_dataset

    ds = load_dataset(args.dataset, split=args.split)
    instanzen = {r["instance_id"]: r for r in ds if r["instance_id"] in preds}
    fehlend = set(preds) - set(instanzen)
    if fehlend:
        console.print(f"[yellow]nicht im Datensatz, übersprungen: {sorted(fehlend)}[/yellow]")

    ergebnisse = []
    for n, (iid, patch) in enumerate(sorted(preds.items()), 1):
        if iid not in instanzen:
            continue
        console.print(f"[dim]({n}/{len(preds)})[/dim] {iid} …")
        try:
            r = evaluate(instanzen[iid], patch, args)
        except Exception as e:  # ein kaputter Container darf den Lauf nicht beenden
            r = {"instance_id": iid, "status": "error", "detail": f"{type(e).__name__}: {e}"}
        ergebnisse.append(r)
        console.print(f"    → [bold]{r['status']}[/bold]")

    out = results_dir("swebench", args.run_id) / "eval_local.json"
    gelöst = [r["instance_id"] for r in ergebnisse if r["status"] == "resolved"]
    bericht = {
        "run_id": args.run_id,
        "dataset": args.dataset,
        "split": args.split,
        "total": len(ergebnisse),
        "resolved": len(gelöst),
        "resolved_ids": gelöst,
        "results": ergebnisse,
    }
    out.write_text(json.dumps(bericht, indent=2, ensure_ascii=False), encoding="utf-8")

    t = Table(title=f"SWE-bench lokal — {args.run_id}")
    t.add_column("Status")
    t.add_column("Anzahl", justify="right")
    from collections import Counter

    for status, anzahl in Counter(r["status"] for r in ergebnisse).most_common():
        t.add_row(status, str(anzahl))
    console.print(t)
    quote = 100.0 * len(gelöst) / len(ergebnisse) if ergebnisse else 0.0
    console.print(f"[bold]resolved: {len(gelöst)}/{len(ergebnisse)} ({quote:.1f} %)[/bold]")
    console.print(f"Bericht: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
