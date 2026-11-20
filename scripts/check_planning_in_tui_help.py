"""Verifiziert, dass das TUI-Hilfemenü den lokalen /planning-Befehl aufführt.

Ohne Argument wird der Arbeitsbaum geprüft. Mit --head wird der eingecheckte
Basisstand geprüft; das dient als Reproduktion des gemeldeten Fehlers.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

SOURCE = Path("agent_framework_rs/src/tui.rs")
EXPECTED = '("/planning on|off", "Planungsmodus ein- oder ausschalten")'


def source_text() -> str:
    if sys.argv[1:] == ["--head"]:
        return subprocess.check_output(
            ["git", "show", f"HEAD:{SOURCE.as_posix()}"],
            text=True,
            encoding="utf-8",
        )
    if len(sys.argv) != 1:
        raise SystemExit("Nutzung: check_planning_in_tui_help.py [--head]")
    return SOURCE.read_text(encoding="utf-8")


if EXPECTED not in source_text():
    raise SystemExit("/planning on|off fehlt in der TUI-Hilfe")

print("/planning on|off ist in der TUI-Hilfe aufgeführt")
