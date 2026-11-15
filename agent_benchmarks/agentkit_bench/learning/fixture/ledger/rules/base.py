import re
from dataclasses import dataclass
from typing import Callable

from ledger.model import Entry, Finding

CODE_PATTERN = re.compile(r"^[A-Z]{3}\d{3}$")


@dataclass(frozen=True)
class Rule:
    """A check over all entries. `apply(entries, settings)` returns findings.

    `settings` is this rule's own block from `limits.json`, keyed by `code`.
    """

    code: str
    summary: str
    apply: Callable[[list[Entry], dict], list[Finding]]

    def __post_init__(self):
        if not CODE_PATTERN.match(self.code):
            raise ValueError(f"rule code must look like ABC123, got {self.code!r}")
