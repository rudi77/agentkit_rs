from ledger.model import Entry, Finding
from ledger.rules.base import Rule


def _apply(entries: list[Entry], settings: dict) -> list[Finding]:
    allowed = set(settings.get("allowed", []))
    return [
        Finding("CUR004", e.ref, "currency not allowed")
        for e in entries
        if e.currency not in allowed
    ]


RULE = Rule("CUR004", "entries in a currency that is not allowed", _apply)
