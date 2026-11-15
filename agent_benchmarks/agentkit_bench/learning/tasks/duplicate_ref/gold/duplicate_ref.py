from ledger.model import Entry, Finding
from ledger.rules.base import Rule


def _apply(entries: list[Entry], settings: dict) -> list[Finding]:
    ignored = tuple(settings.get("ignore_prefixes", []))
    seen: set[str] = set()
    findings = []
    for e in entries:
        if e.ref in seen and not (ignored and e.ref.startswith(ignored)):
            findings.append(Finding("DUP003", e.ref, "duplicate reference"))
        seen.add(e.ref)
    return findings


RULE = Rule("DUP003", "references that occur more than once", _apply)
