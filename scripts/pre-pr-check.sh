#!/usr/bin/env bash
# pre-pr-check.sh — lightweight gate for implementation agents to run before opening a PR.
# Checks changed Rust files for bare `pub` fields, `.unwrap()` outside test blocks,
# and runs `cargo check` against the affected crate(s).
#
# Usage: ./scripts/pre-pr-check.sh [<crate-name>]
#   e.g. ./scripts/pre-pr-check.sh phantom-brain
#
# Exit code 0 = all checks passed.  Non-zero = at least one check failed.

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

EXPLICIT_CRATE="${1:-}"
FAIL=0

# ── Helpers ──────────────────────────────────────────────────────────────────

red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n'   "$*"; }

banner() { echo; bold "── $* ──"; }

# Collect changed .rs files relative to main (or the merge-base).
changed_rs_files() {
    local base
    base=$(git merge-base HEAD "$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's|refs/remotes/origin/||' || echo main)" 2>/dev/null || echo "HEAD~1")
    git diff --name-only "$base" HEAD -- '*.rs'
}

# Resolve crate name(s) from a list of file paths.
# Returns unique `name` fields from each matching crates/*/Cargo.toml.
crates_for_files() {
    local files=("$@")
    local found=()
    for f in "${files[@]}"; do
        # Walk up to find the nearest Cargo.toml inside crates/
        local dir
        dir=$(dirname "$f")
        while [[ "$dir" != "." && "$dir" != "/" ]]; do
            local cargo="$dir/Cargo.toml"
            if [[ -f "$cargo" ]] && grep -q '^\[package\]' "$cargo" 2>/dev/null; then
                local name
                name=$(grep -m1 '^name' "$cargo" | sed 's/.*= *"\(.*\)".*/\1/')
                if [[ -n "$name" ]]; then
                    found+=("$name")
                fi
                break
            fi
            dir=$(dirname "$dir")
        done
    done
    # Deduplicate
    printf '%s\n' "${found[@]}" | sort -u
}

# ── Gather changed files ──────────────────────────────────────────────────────

banner "Collecting changed Rust files"

mapfile -t CHANGED < <(changed_rs_files)

if [[ ${#CHANGED[@]} -eq 0 ]]; then
    green "No changed .rs files detected — nothing to check."
    exit 0
fi

echo "Changed files (${#CHANGED[@]}):"
printf '  %s\n' "${CHANGED[@]}"

# ── Check 1: bare `pub` fields ────────────────────────────────────────────────
# Matches lines like `    pub field_name:` but NOT `pub(crate)`, `pub(super)`,
# `pub fn`, `pub struct`, `pub enum`, `pub mod`, `pub use`, `pub type`, `pub trait`, `pub impl`.

banner "Check 1: bare pub fields (pub without visibility qualifier on struct fields)"

PUB_FIELD_HITS=()
for f in "${CHANGED[@]}"; do
    [[ -f "$f" ]] || continue
    while IFS= read -r line; do
        PUB_FIELD_HITS+=("$f: $line")
    done < <(grep -nP '^\s+pub\s+(?![(fn struct enum mod use type trait impl])[a-z_]' "$f" 2>/dev/null || true)
done

if [[ ${#PUB_FIELD_HITS[@]} -gt 0 ]]; then
    red "FAIL — bare pub fields found (use pub(crate) instead):"
    printf '  %s\n' "${PUB_FIELD_HITS[@]}"
    FAIL=1
else
    green "PASS — no bare pub fields."
fi

# ── Check 2: .unwrap() outside #[cfg(test)] blocks ───────────────────────────

banner "Check 2: .unwrap() calls outside #[cfg(test)] blocks"

UNWRAP_HITS=()
for f in "${CHANGED[@]}"; do
    [[ -f "$f" ]] || continue

    # Strip test blocks, then scan for .unwrap().
    # Strategy: use awk to suppress lines between #[cfg(test)] ... end of matching brace block,
    # then grep the remainder.
    stripped=$(awk '
        /^[[:space:]]*#\[cfg\(test\)\]/ { in_test=1; depth=0; next }
        in_test {
            for (i=1; i<=length($0); i++) {
                c = substr($0,i,1)
                if (c == "{") depth++
                if (c == "}") { depth--; if (depth <= 0) { in_test=0; next } }
            }
            next
        }
        { print NR": "$0 }
    ' "$f")

    while IFS= read -r line; do
        UNWRAP_HITS+=("$f: $line")
    done < <(echo "$stripped" | grep -P '\.unwrap\(\)' || true)
done

if [[ ${#UNWRAP_HITS[@]} -gt 0 ]]; then
    red "FAIL — .unwrap() calls outside test blocks:"
    printf '  %s\n' "${UNWRAP_HITS[@]}"
    FAIL=1
else
    green "PASS — no .unwrap() calls outside test blocks."
fi

# ── Check 3: cargo check ──────────────────────────────────────────────────────

banner "Check 3: cargo check"

if [[ -n "$EXPLICIT_CRATE" ]]; then
    CRATES_TO_CHECK=("$EXPLICIT_CRATE")
else
    mapfile -t CRATES_TO_CHECK < <(crates_for_files "${CHANGED[@]}")
fi

if [[ ${#CRATES_TO_CHECK[@]} -eq 0 ]]; then
    echo "Could not detect affected crate(s); running workspace-wide cargo check."
    if ! cargo check --workspace --quiet 2>&1; then
        red "FAIL — cargo check (workspace) failed."
        FAIL=1
    else
        green "PASS — cargo check (workspace)."
    fi
else
    echo "Crate(s) to check: ${CRATES_TO_CHECK[*]}"
    for crate in "${CRATES_TO_CHECK[@]}"; do
        echo
        echo "  cargo check -p $crate ..."
        if ! cargo check -p "$crate" --quiet 2>&1; then
            red "FAIL — cargo check -p $crate failed."
            FAIL=1
        else
            green "PASS — cargo check -p $crate."
        fi
    done
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo
if [[ $FAIL -ne 0 ]]; then
    red "pre-pr-check FAILED — fix the issues above before opening a PR."
    exit 1
else
    green "pre-pr-check PASSED — ready to open a PR."
    exit 0
fi
