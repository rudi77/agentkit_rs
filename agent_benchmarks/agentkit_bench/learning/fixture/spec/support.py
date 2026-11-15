import json
import os
from pathlib import Path

from ledger.model import Entry


def load_fixture(name: str) -> list[Entry]:
    root = os.environ.get("LEDGER_FIXTURES")
    if not root:
        raise RuntimeError("LEDGER_FIXTURES is not set")
    rows = json.loads((Path(root) / f"{name}.json").read_text())
    return [Entry(**row) for row in rows]
