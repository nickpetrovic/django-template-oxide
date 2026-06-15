#!/usr/bin/env bash
#
# Local quality gate. Runs the full Rust + Python check suite a reviewer
# (or you, before pushing) should pass. No CI required.
#
#   scripts/check.sh         # everything
#   scripts/check.sh --fast  # skip the Python suite + fuzzer
#
# Optional heavier tools, run on demand:
#   cargo +nightly llvm-cov --summary-only   # Rust coverage of `cargo test`
#   cargo geiger                             # unsafe usage across the dep tree
# Miri is NOT usable here: the suite executes CPython through PyO3 (a foreign
# function), which Miri cannot interpret. Use it only on isolated pure-Rust
# tests if needed.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export PYO3_PYTHON="${PYO3_PYTHON:-$ROOT/.venv/bin/python}"
FAST=0
[[ "${1:-}" == "--fast" ]] && FAST=1

step() { printf '\n\033[1;36m>> %s\033[0m\n' "$1"; }
have() { command -v "$1" >/dev/null 2>&1; }

step "rustfmt (check)"
cargo fmt --check

step "clippy (all targets, -D warnings)"
cargo clippy --all-targets -- -D warnings

step "rust tests"
cargo test

step "rustdoc (-D warnings)"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

if have cargo-machete; then
  step "cargo-machete (unused deps)"
  cargo machete
else
  echo ">> skip cargo-machete (cargo install cargo-machete)"
fi

if have cargo-audit; then
  step "cargo-audit (advisories)"
  cargo audit
else
  echo ">> skip cargo-audit (cargo install cargo-audit)"
fi

if have cargo-deny; then
  step "cargo-deny (advisories/licenses/bans/sources)"
  cargo deny check
else
  echo ">> skip cargo-deny (cargo install cargo-deny)"
fi

if [[ "$FAST" -eq 0 ]]; then
  step "python suite (pytest)"
  uv run --no-sync pytest tests/ -q

  step "differential fuzzer (oxide vs stock Django)"
  uv run --no-sync python scripts/fuzz_differential.py
fi

printf '\n\033[1;32mAll checks passed.\033[0m\n'
