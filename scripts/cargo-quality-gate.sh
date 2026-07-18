#!/bin/sh
set -eu

if command -v cargo >/dev/null 2>&1; then
    cargo_bin=cargo
elif [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
    cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin/cargo"
else
    echo "error: cargo is required to run the Rust quality gates" >&2
    exit 127
fi

exec "$cargo_bin" "$1" --locked --all-targets --all-features
