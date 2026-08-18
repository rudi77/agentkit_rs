#!/usr/bin/env bash
#
# agentkit-Installer für Linux/macOS.
#
# Baut die agentkit-Executable lokal aus dem Quellcode (nativer Rust-Build via
# `cargo install`) und legt sie in den PATH. Siehe ../INSTALL.md.
#
# Aufruf:
#   ./scripts/install.sh
#   ./scripts/install.sh --no-tui   # ohne Terminal-UI bauen (schlanker, kein ratatui)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$REPO_ROOT/agentkit_app"

WITH_TUI=1
for arg in "$@"; do
    [ "$arg" = "--no-tui" ] && WITH_TUI=0
done

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
    # Derselbe Feature-Satz, den auch der Release baut (.github/workflows/release.yml,
    # scripts/build.ps1): PDF, Context-Management, Wissensgraph, Arbeits-Runtime und
    # Betrachter sind in BEIDEN Varianten drin — alles reines Rust ohne C-Toolchain.
    # Nur das TUI ist optional, das ist der einzige Unterschied zwischen 'voll' und 'cli'.
    local features=(--features "pdf ctxman tiktoken graph work viz")
    if [ "$WITH_TUI" -eq 1 ]; then
        features=(--features "tui pdf ctxman tiktoken graph work viz")
        info "Baue Rust-Executable 'agentkit' mit Terminal-UI (cargo install)…"
    else
        info "Baue Rust-Executable 'agentkit' ohne Terminal-UI (cargo install)…"
    fi
    cargo install --path "$RUST_DIR" --bin agentkit "${features[@]}" --force
    ok "agentkit installiert nach: ${CARGO_HOME:-$HOME/.cargo}/bin/agentkit"
    warn "Liegt \$HOME/.cargo/bin im PATH? Sonst hinzufügen: export PATH=\"\$HOME/.cargo/bin:\$PATH\""
    install_completions
}

# Shell-Completions best-effort in die Standard-User-Verzeichnisse legen.
# Fehlschläge sind nie fatal.
install_completions() {
    local bin="${CARGO_HOME:-$HOME/.cargo}/bin/agentkit"
    have agentkit && bin="agentkit"
    command -v "$bin" >/dev/null 2>&1 || [ -x "$bin" ] || {
        warn "agentkit nicht auffindbar — Shell-Completions übersprungen."
        return 0
    }
    # bash (XDG-User-Verzeichnis, von bash-completion automatisch gelesen)
    local bash_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions"
    if mkdir -p "$bash_dir" 2>/dev/null && "$bin" completions bash >"$bash_dir/agentkit" 2>/dev/null; then
        ok "bash-Completion: $bash_dir/agentkit"
    fi
    # fish (nur wenn fish vorhanden)
    if have fish; then
        local fish_dir="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions"
        if mkdir -p "$fish_dir" 2>/dev/null && "$bin" completions fish >"$fish_dir/agentkit.fish" 2>/dev/null; then
            ok "fish-Completion: $fish_dir/agentkit.fish"
        fi
    fi
    # zsh: fpath variiert je Setup — nur Hinweis geben.
    if have zsh; then
        info "zsh-Completion einrichten:  $bin completions zsh > \"\${fpath[1]}/_agentkit\"  (dann: compinit)"
    fi
}

install_rust

ok "Fertig. Test:  agentkit --demo \"Was ist 17 + 25?\""
