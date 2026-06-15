#!/usr/bin/env bash
#
# Build a Profile-Guided-Optimization (PGO) wheel for django-template-oxide.
#
# Flow: build an instrumented extension, run benches/bench.py to collect
# branch/call profiles, merge them, then rebuild with the profile data.
# Measured gains on this codebase: ~10-13% on template compile (parse-heavy,
# pure Rust), ~6% on render, ~20% on forloop-counter bodies.
#
# Usage:
#   scripts/build-pgo.sh            # -> target/wheels/<optimized>.whl
#   PGO_KEEP=1 scripts/build-pgo.sh # keep the profile dir for inspection
#
# Requires: rustup component add llvm-tools
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VENV="$ROOT/.venv"
PGO_DIR="$(mktemp -d -t dto-pgo-XXXXXX)"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
PROFDATA="$(rustc --print sysroot)/lib/rustlib/${HOST}/bin/llvm-profdata"
MATURIN=(uvx --from 'maturin>=1,<2' maturin)

if [[ ! -x "$PROFDATA" ]]; then
  echo "error: llvm-profdata not found at $PROFDATA" >&2
  echo "       install it with: rustup component add llvm-tools" >&2
  exit 1
fi
if [[ ! -d "$VENV" ]]; then
  echo "error: dev venv missing at $VENV (run 'uv sync' first)" >&2
  exit 1
fi

# macOS 26+ rejects stripped release dylibs at load time (dyld LINKEDIT).
export CARGO_PROFILE_RELEASE_STRIP=false
# maturin develop installs into this interpreter's environment.
export VIRTUAL_ENV="$VENV"

cleanup() { [[ "${PGO_KEEP:-0}" == "1" ]] || rm -rf "$PGO_DIR"; }
trap cleanup EXIT

echo ">> [1/4] building instrumented extension (profile-generate)"
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" "${MATURIN[@]}" develop --release

echo ">> [2/4] collecting profiles (benches/bench.py x2)"
"$VENV/bin/python" benches/bench.py >/dev/null
"$VENV/bin/python" benches/bench.py >/dev/null

echo ">> [3/4] merging profiles"
"$PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"/*.profraw

echo ">> [4/4] building optimized wheel (profile-use)"
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata -Cllvm-args=-pgo-warn-missing-function" \
  "${MATURIN[@]}" build --release

echo
echo "done: optimized wheel is in $ROOT/target/wheels/"
echo "the dev tree still holds the instrumented build; restore a normal one with:"
echo "  uvx --from 'maturin>=1,<2' maturin develop --release"
