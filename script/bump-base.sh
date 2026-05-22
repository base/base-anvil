#!/usr/bin/env bash
# bump-base.sh — retarget base-anvil's pinned base/base commit in
# crates/evm/networks/Cargo.toml and optionally rebuild.
#
# Usage:
#   ./script/bump-base.sh                    # pin to current github.com/base/base main HEAD
#   ./script/bump-base.sh <branch>           # pin to branch HEAD (e.g. sk/tangor)
#   ./script/bump-base.sh <commit-sha>       # pin to exact commit
#   ./script/bump-base.sh --no-build <ref>   # update Cargo.toml only, skip rebuild
#
# Resolves the ref via `git ls-remote` (so any branch / sha / tag on the
# remote works without needing a local base/base clone). After updating the
# Cargo.toml, rebuilds `anvil` + `forge` unless --no-build is passed.
#
# Exit codes:
#   0   ref resolved, Cargo.toml updated, (rebuild succeeded if attempted)
#   1   rebuild failed (Cargo.toml is still updated)
#   2   ref couldn't be resolved (Cargo.toml untouched)

set -euo pipefail

REMOTE="https://github.com/base/base.git"
CARGO_TOML="crates/evm/networks/Cargo.toml"

# ── Parse args ────────────────────────────────────────────────────────────────

REBUILD=1
REF=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build) REBUILD=0; shift ;;
        --help|-h)
            sed -n '2,18p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        -*) echo "bump-base.sh: unknown flag: $1" >&2; exit 2 ;;
        *)
            if [[ -n "$REF" ]]; then
                echo "bump-base.sh: too many positional args (already have '$REF', got '$1')" >&2
                exit 2
            fi
            REF="$1"; shift
            ;;
    esac
done

REF="${REF:-main}"

# ── Locate workspace root ─────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CARGO_TOML_PATH="$WORKSPACE_ROOT/$CARGO_TOML"

if [[ ! -f "$CARGO_TOML_PATH" ]]; then
    echo "bump-base.sh: $CARGO_TOML not found at $CARGO_TOML_PATH" >&2
    exit 2
fi

# ── Resolve ref to SHA ────────────────────────────────────────────────────────

echo "bump-base: resolving '$REF' against $REMOTE..." >&2

# If REF looks like a full SHA (40 hex chars), use as-is.
# Otherwise, ls-remote against branches + tags.
if [[ "$REF" =~ ^[0-9a-f]{40}$ ]]; then
    SHA="$REF"
else
    # Try branch refs first, then tags, then bare ref.
    SHA=$(git ls-remote "$REMOTE" "refs/heads/$REF" 2>/dev/null | awk '{print $1}' | head -1)
    if [[ -z "$SHA" ]]; then
        SHA=$(git ls-remote "$REMOTE" "refs/tags/$REF" 2>/dev/null | awk '{print $1}' | head -1)
    fi
    if [[ -z "$SHA" ]]; then
        SHA=$(git ls-remote "$REMOTE" "$REF" 2>/dev/null | awk '{print $1}' | head -1)
    fi
    # Last resort: short SHAs (7+ hex chars) — accept verbatim, cargo will validate.
    if [[ -z "$SHA" ]] && [[ "$REF" =~ ^[0-9a-f]{7,40}$ ]]; then
        SHA="$REF"
        echo "bump-base: WARNING: '$REF' looks like a short SHA; using verbatim (cargo will validate)" >&2
    fi
fi

if [[ -z "$SHA" ]]; then
    echo "bump-base: ERROR: could not resolve '$REF' on $REMOTE" >&2
    exit 2
fi

echo "bump-base: $REF -> $SHA" >&2

# ── Capture old SHA for the summary ───────────────────────────────────────────

OLD_SHA=$(grep -oE 'rev = "[0-9a-f]{7,40}"' "$CARGO_TOML_PATH" | head -1 | sed -E 's/rev = "([0-9a-f]+)"/\1/')

if [[ "$OLD_SHA" == "$SHA" ]]; then
    echo "bump-base: already pinned to $SHA — nothing to do" >&2
    [[ $REBUILD -eq 1 ]] || exit 0
fi

# ── Patch the Cargo.toml ──────────────────────────────────────────────────────

# Address-scoped substitution: only rewrite `rev = "..."` on lines that also
# match the base/base git URL, so other git deps (e.g. rexpect) aren't touched.
# sed -i syntax differs between BSD (macOS) and GNU; detect and adapt.
SED_INPLACE=(-i)
if [[ "$OSTYPE" == "darwin"* ]]; then
    SED_INPLACE=(-i '')
fi

sed "${SED_INPLACE[@]}" -E \
    "/git = \"https:\/\/github\.com\/base\/base\.git\"/ s/rev = \"[0-9a-f]+\"/rev = \"$SHA\"/g" \
    "$CARGO_TOML_PATH"

echo "bump-base: updated $CARGO_TOML: $OLD_SHA -> $SHA" >&2

# ── Optional rebuild ──────────────────────────────────────────────────────────

if [[ $REBUILD -eq 0 ]]; then
    echo "bump-base: skipping rebuild (--no-build); run \`cargo build --release -p anvil -p forge\` manually" >&2
    exit 0
fi

echo "bump-base: rebuilding anvil + forge (cargo build --release -p anvil -p forge)..." >&2
cd "$WORKSPACE_ROOT"
if cargo build --release -p anvil -p forge; then
    echo "bump-base: done. Pinned to $SHA, binaries in target/release/{anvil,forge}." >&2
    exit 0
else
    echo "bump-base: rebuild FAILED. Cargo.toml is updated to $SHA; revert or fix as needed." >&2
    exit 1
fi
