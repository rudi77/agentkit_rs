#!/usr/bin/env bash
# Baut agentkit als statisches x86_64-musl-Binary für die Benchmark-Container.
# Fallback-Leiter: nativer musl-Build -> cargo-zigbuild -> Docker-Build.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$HERE/../../agent_framework_rs"
OUT_DIR="$HERE/../build"
OUT="$OUT_DIR/agentkit-x86_64-musl"
TARGET=x86_64-unknown-linux-musl

mkdir -p "$OUT_DIR"

build_native() {
    rustup target add "$TARGET"
    (cd "$CRATE" && cargo build --release --target "$TARGET" --bin agentkit)
}

build_zig() {
    (cd "$CRATE" && cargo zigbuild --release --target "$TARGET" --bin agentkit)
}

build_docker() {
    # Git Bash (MSYS) würde /src/... in einen Windows-Pfad umschreiben —
    # Konvertierung abschalten und fürs Volume den Windows-Pfad (pwd -W) nehmen.
    local src
    src="$(cd "$CRATE/.." && (pwd -W 2>/dev/null || pwd))"
    MSYS_NO_PATHCONV=1 docker run --rm -v "$src":/src -w /src/agent_framework_rs \
        messense/rust-musl-cross:x86_64-musl \
        cargo build --release --target "$TARGET" --bin agentkit
}

if command -v cargo >/dev/null && (command -v musl-gcc >/dev/null || [ "$(uname -sm)" = "Linux x86_64" ]); then
    build_native || { echo "nativer Build fehlgeschlagen, versuche zigbuild/docker"; \
        (command -v cargo-zigbuild >/dev/null && build_zig) || build_docker; }
elif command -v cargo-zigbuild >/dev/null; then
    build_zig
else
    build_docker
fi

cp "$CRATE/target/$TARGET/release/agentkit" "$OUT"
file "$OUT" | grep -Eq "static-pie linked|statically linked" || {
    echo "FEHLER: $OUT ist nicht statisch gelinkt"; exit 1; }
echo "OK: $OUT ($(du -h "$OUT" | cut -f1))"
