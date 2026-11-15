from datetime import date

from ledger.model import Entry, Finding
from ledger.rules.base import Rule


def _apply(entries: list[Entry], settings: dict) -> list[Finding]:
    exempt = set(settings.get("allow_accounts", []))
    return [
        Finding("WKD002", e.ref, "booked on a weekend")
        for e in entries
        if e.account not in exempt and date.fromisoformat(e.booked_on).weekday() >= 5
    ]


RULE = Rule("WKD002", "entries booked on a weekend", _apply)
