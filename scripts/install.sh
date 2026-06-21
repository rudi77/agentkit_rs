#!/usr/bin/env bash
#
# agentkit-Installer für Linux/macOS.
#
# Baut die agentkit-Executable lokal aus dem Quellcode und legt sie in den PATH.
# Zwei Varianten stehen zur Wahl (siehe ../INSTALL.md):
#   - rust   : nativer Rust-Build via `cargo install` (klein, schnell, keine Runtime)
#   - python : Python-Paket via pipx (oder pip), Console-Script `agentkit`
#
# Aufruf:
#   ./scripts/install.sh                # interaktiv bzw. Default (rust, falls cargo da)
#   ./scripts/install.sh rust
#   ./scripts/install.sh python
#   ./scripts/install.sh both
#
# Optionen:
#   --no-tui   Rust ohne Terminal-UI bauen (schlanker, kein ratatui)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$REPO_ROOT/agent_framework_rs"
PY_DIR="$REPO_ROOT/agent_framework"

TARGET="${1:-auto}"
WITH_TUI=1
for arg in "$@"; do
    [ "$arg" = "--no-tui" ] && WITH_TUI=0
done
# Das Positionsargument darf keine Option sein.
case "$TARGET" in
    --*) TARGET="auto" ;;
esac

info()  { printf '\033[1;34m»\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m! \033[0m%s\n' "$*" >&2; }
err()   { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; }
ok()    { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

have() { command -v "$1" >/dev/null 2>&1; }

install_rust() {
    if ! have cargo; then
        err "cargo nicht gefunden. Rust installieren: https://rustup.rs"
        return 1
    fi
    local features=()
    if [ "$WITH_TUI" -eq 1 ]; then
        features=(--features tui)
        info "Baue Rust-Executable 'agentkit' mit Terminal-UI (cargo install)…"
    else
        info "Baue Rust-Executable 'agentkit' ohne Terminal-UI (cargo install)…"
    fi
    cargo install --path "$RUST_DIR" --bin agentkit "${features[@]}" --force
    ok "Rust-agentkit installiert nach: ${CARGO_HOME:-$HOME/.cargo}/bin/agentkit"
    warn "Liegt \$HOME/.cargo/bin im PATH? Sonst hinzufügen: export PATH=\"\$HOME/.cargo/bin:\$PATH\""
}

install_python() {
    if have pipx; then
        info "Installiere Python-agentkit via pipx…"
        pipx install --force "$PY_DIR"
        ok "Python-agentkit via pipx installiert (Console-Script 'agentkit')."
    elif have pip || have pip3; then
        local pip_cmd; pip_cmd="$(command -v pip3 || command -v pip)"
        warn "pipx nicht gefunden — nutze '$pip_cmd install --user'."
        "$pip_cmd" install --user "$PY_DIR"
        ok "Python-agentkit via pip --user installiert (Console-Script 'agentkit')."
        warn "Liegt das User-bin-Verzeichnis im PATH (z. B. ~/.local/bin)?"
    else
        err "Weder pipx noch pip gefunden. Python 3.10+ mit pip installieren."
        return 1
    fi
}

# Default-Auswahl, wenn nichts angegeben.
if [ "$TARGET" = "auto" ]; then
    if have cargo; then
        TARGET="rust"
    elif have pipx || have pip || have pip3; then
        TARGET="python"
    else
        err "Weder cargo noch pip/pipx gefunden. Bitte Rust oder Python installieren."
        exit 1
    fi
    info "Keine Variante angegeben — wähle automatisch: $TARGET"
fi

case "$TARGET" in
    rust)   install_rust ;;
    python) install_python ;;
    both)   install_rust; install_python ;;
    *)      err "Unbekannte Variante '$TARGET'. Erlaubt: rust | python | both"; exit 1 ;;
esac

ok "Fertig. Test:  agentkit --demo \"Was ist 17 + 25?\""
