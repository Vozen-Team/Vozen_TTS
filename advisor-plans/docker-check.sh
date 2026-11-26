#!/usr/bin/env bash
# Runs a cargo command for this worktree inside the Linux QA toolchain container.
#
# Windows App Control blocks locally built Rust test binaries (os error 4551), so every cargo
# gate for plans 019-030 runs here instead of on the Windows host.
#
# Usage (from Git Bash, at the worktree root):
#   bash advisor-plans/docker-check.sh cargo test -p vozen-core text_cleaning --locked
#   bash advisor-plans/docker-check.sh gate      # full repository gate, see advisor-plans/README.md
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && (pwd -W 2>/dev/null || pwd))"

if [ "${1:-}" = "gate" ]; then
  set -- bash -ec '
    echo "== fmt";   cargo fmt --check
    echo "== check"; cargo check --workspace --all-targets --locked
    echo "== clippy"; cargo clippy --workspace --all-targets --locked -- -D warnings
    echo "== test";  cargo test --workspace --locked
    echo "== voice-driver"
    cargo check -p vozen-runtime --features voice-driver --all-targets --locked
    cargo test -p vozen-runtime --features voice-driver --locked
    cargo clippy -p vozen-runtime --features voice-driver --all-targets --locked -- -D warnings
    echo "== GATE OK"'
fi

if [ "$#" -eq 0 ]; then
  echo "usage: $0 gate | <command...>" >&2
  exit 2
fi

MSYS_NO_PATHCONV=1 exec docker run --rm \
  -v "$root:/src" \
  -v vozen-audit-cargo:/usr/local/cargo/registry \
  -v vozen-audit-tts-vd-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  vozen-qa-toolchain:20260930 "$@"
