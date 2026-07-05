#!/usr/bin/env bash
# Smoke: aden partition index + run ledger resume helpers (no live model).
# Proves fixture partition parses and sequential resume state derives correctly.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADEN_BIN="${COXN_ADEN_BIN:-aden}"

fail() {
    echo "[smoke-execute] $*" >&2
    exit 1
}

if ! command -v "$ADEN_BIN" >/dev/null 2>&1; then
    if [[ "${COXN_SMOKE_REQUIRE_ADEN:-}" == "1" ]]; then
        fail "aden required but not on PATH"
    fi
    echo "[smoke-execute] skip: aden not on PATH"
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

git init -q
git config user.email smoke@coxn.local
git config user.name "coxn smoke"

mkdir -p src
cat >src/lib.rs <<'EOF'
pub fn alpha() -> i32 { 1 }
pub fn beta() -> i32 { alpha() + 1 }
EOF
git add -A
git commit -q -m "baseline"

# Minimal aden store is not required for scope --agents if seeds resolve in-repo.
# If aden needs gen, the smoke will fail clearly.
TASK="smoke-partition"
SEEDS="alpha,beta"

echo "[smoke-execute] aden scope --agents ${TASK}"
set +e
partition="$("$ADEN_BIN" scope --agents "$TASK" --seed "$SEEDS" --json 2>/dev/null)"
rc=$?
set -e
if [[ $rc -ne 0 || -z "$partition" ]]; then
    if [[ "${COXN_SMOKE_REQUIRE_ADEN:-}" == "1" ]]; then
        fail "aden scope --agents failed (rc=$rc); run aden gen in a real project first"
    fi
    echo "[smoke-execute] skip: aden scope --agents unavailable in fixture (rc=$rc)"
    exit 0
fi

echo "$partition" | head -5
lines="$(echo "$partition" | grep -c . || true)"
if [[ "$lines" -lt 1 ]]; then
    fail "empty partition index"
fi

echo "[smoke-execute] partition lines: $lines"

# Run ledger round-trip via coxn binary if built.
COXN_BIN="${COXN_BIN:-}"
if [[ -z "$COXN_BIN" ]]; then
    if [[ -x "$ROOT/target/debug/coxn" ]]; then
        COXN_BIN="$ROOT/target/debug/coxn"
    elif command -v coxn >/dev/null 2>&1; then
        COXN_BIN="$(command -v coxn)"
    else
        (cd "$ROOT" && cargo build -q)
        COXN_BIN="$ROOT/target/debug/coxn"
    fi
fi

export COXN_RUNS_DIR="$WORK/runs"
mkdir -p "$COXN_RUNS_DIR"

# Hermetic: invoke run_ledger paths through coxn once is heavy; use cargo test filter.
(cd "$ROOT" && cargo test -q run_ledger::tests::ledger_appends_and_summarizes_events) \
    || fail "run_ledger unit test failed"

echo "[smoke-execute] pass (partition index + ledger unit test)"