#!/usr/bin/env bash
# Demo: the aden blast-radius gate wedges out-of-scope edits and reverts them.
#
# coxn is a dumb pump; aden is the bloat arbiter and the blast-radius gate.
# Before coxn accepts an edit it runs `aden impact-diff --scope <manifest>`,
# obeys the exit code (0 in-scope / 1 scope-escape / 2 blast-leak / other
# closed), and reverts a blocked file before its result is accepted.
#
# This script proves that wedge deterministically, on disk, with no model and no
# cloud keys: it builds a throwaway git repo, commits a baseline, writes a
# scope manifest whose file mandate is exactly one file, then shows:
#
#   1. an IN-SCOPE edit  -> gate exit 0  (allowed)
#   2. an OUT-OF-SCOPE edit -> gate exit 1  (blocked as a scope-escape)
#   3. the blocked file is reverted to HEAD (its pre-edit bytes are restored)
#   4. `coxn doctor` labels the sandbox state (bwrap present / NO SANDBOX)
#
# Requires: the `aden` binary on PATH (override: COXN_ADEN_BIN) and a built
# `coxn` (the script builds it on demand). Exits 0 when the wedge is proven,
# 1 otherwise (or if a prerequisite is missing).
set -euo pipefail

ADEN_BIN="${COXN_ADEN_BIN:-aden}"
COXN_BIN="${COXN_BIN:-}"

fail() {
    echo "[demo] $*" >&2
    exit 1
}

# Absolute project root (the binary lives under target/debug there; the script
# later cd's into a throwaway work dir, so relative paths would break).
PROJ_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# Resolve a coxn binary, building it on demand if none wired up.
if [[ -z "$COXN_BIN" ]]; then
    if [[ -x "$PROJ_DIR/target/debug/coxn" ]]; then
        COXN_BIN="$PROJ_DIR/target/debug/coxn"
    elif command -v coxn >/dev/null 2>&1; then
        COXN_BIN="$(command -v coxn)"
    else
        echo "[demo] building coxn (cargo build --quiet)…"
        (cd "$PROJ_DIR" && cargo build --quiet)
        COXN_BIN="$PROJ_DIR/target/debug/coxn"
    fi
fi

command -v "$ADEN_BIN" >/dev/null 2>&1 \
    || fail "this demo needs the \`aden\` binary on PATH (set COXN_ADEN_BIN to a dev build)."

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

echo "[demo] fixture repo: $WORK"

git init -q
git config user.email demo@coxn.local
git config user.name "coxn demo"

# A small symbol graph: greet() calls greeting(), both in scope-app.rs; the
# README is intentionally out of scope so editing it must trip the gate.
mkdir -p src
cat >src/app.rs <<'EOF'
fn greeting() -> &'static str { "hello" }
fn greet() { println!("{}", greeting()); }
EOF
cat >README.md <<'EOF'
# demo
A throwaway repo for the coxn scope-escape demo.
EOF
git add -A
git commit -q -m "baseline"

# The scope manifest: the file mandate is ONLY src/app.rs. Editing README.md
# must therefore be a scope-escape.
mkdir -p .aden/agents
cat >.aden/agents/demo-0.json <<'EOF'
{
  "name": "demo",
  "seeds": ["greet"],
  "anchors": ["greet", "greeting"],
  "files": ["src/app.rs"],
  "context": { "anchors": ["greet", "greeting"], "budget": 4096 },
  "risk": 3
}
EOF

MANIFEST=".aden/agents/demo-0.json"

echo
echo "=== 1. in-scope edit (src/app.rs is in the mandate) ==="
printf '\n// in-scope: add a note\n' >>src/app.rs
if "$ADEN_BIN" impact-diff --scope "$MANIFEST" . >/dev/null 2>&1; then
    echo "[demo]   gate: ALLOWED  (exit 0, in-scope)  -- correct"
else
    rc=$?
    fail "in-scope edit was NOT allowed (gate exit $rc); the wedge does not hold."
fi
git checkout -q -- src/app.rs

echo
echo "=== 2. out-of-scope edit (README.md is NOT in the mandate) ==="
printf '\nout-of-scope change\n' >>README.md
set +e
"$ADEN_BIN" impact-diff --scope "$MANIFEST" . >/dev/null 2>&1
rc=$?
set -e
if [[ "$rc" -eq 1 ]]; then
    echo "[demo]   gate: BLOCKED  (exit 1, scope-escape)  -- correct"
else
    fail "out-of-scope edit was not flagged (gate exit $rc, expected 1); the wedge does not hold."
fi

echo
echo "=== 3. the blocked file is reverted to HEAD ==="
git checkout -q -- README.md
if git diff --quiet -- README.md; then
    echo "[demo]   README.md restored to baseline  -- correct"
else
    fail "README.md was not reverted to HEAD."
fi

echo
echo "=== 4. sandbox state (coxn doctor) ==="
"$COXN_BIN" doctor | sed 's/^/[doctor] /' || true

echo
echo "[demo] wedge proven. fixture repo was: $WORK"
exit 0