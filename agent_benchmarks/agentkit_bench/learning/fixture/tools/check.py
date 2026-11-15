#!/usr/bin/env python3
"""Runs the project's checks: the rule lint and every spec."""
import os
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ["LEDGER_FIXTURES"] = str(ROOT / "spec" / "fixtures")


def lint() -> list[str]:
    from ledger import engine

    problems = []
    listed = engine.rule_names()
    present = {
        p.stem for p in engine.RULES_DIR.glob("*.py") if p.stem not in ("__init__", "base")
    }
    for name in sorted(present - set(listed)):
        problems.append(f"rule module '{name}' is not listed in rules/manifest.txt")
    for name in sorted(set(listed) - present):
        problems.append(f"rules/manifest.txt lists '{name}', but there is no such module")
    if problems:
        return problems
    codes = [rule.code for rule in engine.load_rules()]
    for code in sorted({c for c in codes if codes.count(c) > 1}):
        problems.append(f"rule code {code} is used more than once")
    settings = engine.load_settings()
    for code in codes:
        if code not in settings:
            problems.append(f"rules/limits.json has no settings block for {code}")
    return problems


def main() -> int:
    problems = lint()
    for problem in problems:
        print(f"lint: {problem}")
    suite = unittest.defaultTestLoader.discover(
        str(ROOT / "spec"), pattern="*_spec.py", top_level_dir=str(ROOT)
    )
    result = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if result.wasSuccessful() and not problems else 1


if __name__ == "__main__":
    sys.exit(main())
