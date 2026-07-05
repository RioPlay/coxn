#!/usr/bin/env bash
# Smoke: native Ollama /api/chat — list models, one non-streaming completion.
# Skips when ollama is not reachable unless COXN_SMOKE_REQUIRE_OLLAMA=1.
set -euo pipefail

BASE="${COXN_OLLAMA_BASE:-http://localhost:11434}"
MODEL="${COXN_OLLAMA_MODEL:-}"

require() {
    echo "[smoke-ollama] $*" >&2
    exit 1
}

if ! command -v curl >/dev/null 2>&1; then
    require "curl required"
fi

if ! curl -fsS --max-time 2 "${BASE}/api/tags" >/dev/null 2>&1; then
    if [[ "${COXN_SMOKE_REQUIRE_OLLAMA:-}" == "1" ]]; then
        require "ollama not reachable at ${BASE}"
    fi
    echo "[smoke-ollama] skip: ollama not reachable at ${BASE}"
    exit 0
fi

if [[ -z "$MODEL" ]]; then
    MODEL="$(curl -fsS "${BASE}/api/tags" | python3 -c "
import json, sys
data = json.load(sys.stdin)
models = [m.get('name','') for m in data.get('models', []) if m.get('name')]
print(models[0] if models else '')
" 2>/dev/null || true)"
fi

if [[ -z "$MODEL" ]]; then
    require "no model found at ${BASE} (set COXN_OLLAMA_MODEL)"
fi

echo "[smoke-ollama] model=${MODEL} base=${BASE}"

payload="$(python3 -c "
import json
print(json.dumps({
    'model': '$MODEL',
    'messages': [{'role': 'user', 'content': 'Reply with exactly: pong'}],
    'stream': False,
}))
")"

resp="$(curl -fsS "${BASE}/api/chat" \
    -H 'Content-Type: application/json' \
    -d "$payload")"

echo "$resp" | python3 -c "
import json, sys
data = json.load(sys.stdin)
text = (data.get('message') or {}).get('content', '')
if not text.strip():
    raise SystemExit('empty response from ollama')
print('[smoke-ollama] ok:', text.strip()[:80])
"

echo "[smoke-ollama] pass"