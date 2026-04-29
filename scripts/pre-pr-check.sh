#!/usr/bin/env bash
# Pre-PR health check — runs fast local gates before opening a pull request.
#
# Usage:
#   ./scripts/pre-pr-check.sh <crate-name>   # per-crate mode (required by orchestration rule #2)
#   ./scripts/pre-pr-check.sh [--fast]       # workspace mode (default; --fast skips clippy)
#
# Per-crate mode gates (in order):
#   1. cargo build -p <crate>
#   2. cargo test  -p <crate>
#   3. cargo clippy -p <crate> -- -D warnings
#
# Workspace mode gates (in order):
#   1. cargo check (type-check all workspace crates, no codegen)
#   2. cargo fmt --check (formatting)
#   3. cargo clippy (lints, warnings-as-errors)
#   4. cargo test -p phantom-ui   (layout + arbiter unit tests)
#   5. cargo test -p phantom-app coordinator  (coordinator integration tests)
#
# Exit code: 0 = all gates passed, non-zero = at least one gate failed.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAILURES=0

run_gate() {
    local name="$1"
    shift
    echo ""
    echo "==> [$name]"
    if "$@"; then
        echo "    PASS"
    else
        echo "    FAIL"
        FAILURES=$((FAILURES + 1))
    fi
}

# Per-crate mode: first arg is a crate name (not a flag)
FIRST="${1:-}"
if [[ -n "$FIRST" && "$FIRST" != "--fast" ]]; then
    CRATE="$FIRST"
    echo "pre-pr-check: per-crate mode for '$CRATE'"
    run_gate "build ($CRATE)"   cargo build  -p "$CRATE"
    run_gate "test ($CRATE)"    cargo test   -p "$CRATE"
    run_gate "clippy ($CRATE)"  cargo clippy -p "$CRATE" -- -D warnings
    echo ""
    if [[ "$FAILURES" -eq 0 ]]; then
        echo "pre-pr-check passed for $CRATE"
        exit 0
    else
        echo "$FAILURES gate(s) failed for $CRATE"
        exit 1
    fi
fi

# Workspace mode
FAST="${FIRST:-}"
echo "pre-pr-check: workspace mode"

# Gate 1: type-check
run_gate "cargo check (phantom-ui)" cargo check -p phantom-ui
run_gate "cargo check (phantom-app)" cargo check -p phantom-app

# Gate 2: formatting
run_gate "cargo fmt --check (phantom-ui)" cargo fmt -p phantom-ui -- --check
run_gate "cargo fmt --check (phantom-app)" cargo fmt -p phantom-app -- --check

if [[ "$FAST" != "--fast" ]]; then
    # Gate 3: clippy
    run_gate "clippy (phantom-ui)" cargo clippy -p phantom-ui -- -D warnings
    run_gate "clippy (phantom-app)" cargo clippy -p phantom-app -- -D warnings
fi

# Gate 4: layout + arbiter unit tests
run_gate "tests (phantom-ui)" cargo test -p phantom-ui

# Gate 5: coordinator integration tests (includes issue #154 acceptance test)
run_gate "tests (phantom-app coordinator)" cargo test -p phantom-app coordinator

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
    echo "All gates passed."
    exit 0
else
    echo "$FAILURES gate(s) failed."
    exit 1
fi
