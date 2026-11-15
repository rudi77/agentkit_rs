import importlib
import json
from pathlib import Path

from ledger.model import Entry, Finding
from ledger.rules.base import Rule

RULES_DIR = Path(__file__).parent / "rules"


def rule_names() -> list[str]:
    lines = (RULES_DIR / "manifest.txt").read_text().splitlines()
    return [line.strip() for line in lines if line.strip() and not line.startswith("#")]


def load_rules() -> list[Rule]:
    rules = []
    for name in rule_names():
        rule = importlib.import_module(f"ledger.rules.{name}").RULE
        if not isinstance(rule, Rule):
            raise TypeError(f"ledger.rules.{name}.RULE is not a Rule")
        rules.append(rule)
    return rules


def load_settings() -> dict:
    return json.loads((RULES_DIR / "limits.json").read_text())


def check(entries: list[Entry], settings: dict | None = None) -> list[Finding]:
    settings = load_settings() if settings is None else settings
    findings: list[Finding] = []
    for rule in load_rules():
        findings.extend(rule.apply(entries, settings.get(rule.code, {})))
    return findings
