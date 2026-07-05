#!/usr/bin/env bash
# Smoke: aden partition index + agents parse + run ledger (no live model).
# Uses the coxn repo's aden store when available; skips gracefully otherwise.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ADEN_BIN="${COXN_ADEN_BIN:-aden}"
SMOKE_DIR="${COXN_SMOKE_DIR:-$ROOT}"
TASK="coxn-smoke-partition-$$"
SEED="${COXN_SMOKE_SEED:-run}"
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

if [[ ! -d "$SMOKE_DIR/.aden" ]]; then
    if [[ "${COXN_SMOKE_REQUIRE_ADEN:-}" == "1" ]]; then
        fail "no .aden store in $SMOKE_DIR (run aden gen first)"
    fi
    echo "[smoke-execute] skip: no .aden store in $SMOKE_DIR"
    exit 0
fi

cleanup() {
    rm -f "$SMOKE_DIR/.aden/agents/${TASK}"* 2>/dev/null || true
}
trap cleanup EXIT

echo "[smoke-execute] aden scope --agents ${TASK} (dir=$SMOKE_DIR seed=$SEED)"
set +e
partition="$("$ADEN_BIN" scope --agents "$TASK" --seed "$SEED" --json "$SMOKE_DIR" 2>/dev/null)"
rc=$?
set -e
if [[ $rc -ne 0 || -z "$partition" ]]; then
    if [[ "${COXN_SMOKE_REQUIRE_ADEN:-}" == "1" ]]; then
        fail "aden scope --agents failed (rc=$rc); ensure seed '$SEED' resolves in store"
    fi
    echo "[smoke-execute] skip: aden scope --agents unavailable (rc=$rc)"
    exit 0
fi

echo "$partition" | head -5
lines="$(echo "$partition" | grep -c . || true)"
if [[ "$lines" -lt 1 ]]; then
    fail "empty partition index"
fi
echo "[smoke-execute] partition lines: $lines"

(cd "$ROOT" && cargo test -q agents && cargo test -q ledger_appends_and_summarizes_events) \
    || fail "agents/run_ledger unit tests failed"

echo "[smoke-execute] pass (partition index + agents + ledger unit tests)"