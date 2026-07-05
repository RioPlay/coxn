#!/usr/bin/env bash
# CI-friendly wrapper: proves the blast-radius wedge (no model, no cloud keys).
# Skips gracefully when aden is absent unless COXN_SMOKE_REQUIRE_ADEN=1.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v "${COXN_ADEN_BIN:-aden}" >/dev/null 2>&1; then
    if [[ "${COXN_SMOKE_REQUIRE_ADEN:-}" == "1" ]]; then
        echo "[smoke-gate] aden required but not on PATH" >&2
        exit 1
    fi
    echo "[smoke-gate] skip: aden not on PATH"
    exit 0
fi

echo "[smoke-gate] running demo-scope-escape.sh"
exec bash "$ROOT/scripts/demo-scope-escape.sh"