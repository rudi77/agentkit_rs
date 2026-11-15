from dataclasses import dataclass


@dataclass(frozen=True)
class Entry:
    ref: str
    account: str
    amount_cents: int
    currency: str
    booked_on: str  # ISO date, e.g. "2026-03-14"


@dataclass(frozen=True)
class Finding:
    code: str
    ref: str
    message: str
