from ledger.model import Entry, Finding
from ledger.rules.base import Rule


def _apply(entries: list[Entry], settings: dict) -> list[Finding]:
    limit = settings["max_cents"]
    return [
        Finding("AMT001", e.ref, "amount above limit")
        for e in entries
        if abs(e.amount_cents) > limit
    ]


RULE = Rule("AMT001", "single entries above a configured amount", _apply)
