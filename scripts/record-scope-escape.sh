#!/usr/bin/env bash
# Record the scope-escape wedge for README embedding.
# Uses asciinema when installed; otherwise prints manual steps.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/docs/demo-scope-escape.cast}"

if command -v asciinema >/dev/null 2>&1; then
    echo "[record] writing $OUT"
    asciinema rec --overwrite -c "bash $ROOT/scripts/demo-scope-escape.sh" "$OUT"
    GIF="${OUT%.cast}.gif"
    if command -v agg >/dev/null 2>&1; then
        echo "[record] writing $GIF (for README embed)"
        agg "$OUT" "$GIF"
    else
        echo "[record] install agg for README GIF: cargo install --git https://github.com/asciinema/agg"
    fi
    echo "[record] optional upload: asciinema upload -t 'coxn scope-escape' $OUT"
    exit 0
fi

if command -v vhs >/dev/null 2>&1; then
    TAPE="$ROOT/docs/demo-scope-escape.tape"
    cat >"$TAPE" <<EOF
Output $ROOT/docs/demo-scope-escape.gif
Set FontSize 14
Set Width 1200
Set Height 600
Type "bash scripts/demo-scope-escape.sh"
Enter
Sleep 2s
EOF
    vhs "$TAPE"
    echo "[record] wrote docs/demo-scope-escape.gif"
    exit 0
fi

echo "[record] neither asciinema nor vhs on PATH"
echo "  install: pacman -S asciinema  OR  pipx install asciinema  OR  go install github.com/charmbracelet/vhs@latest"
echo "  then re-run: bash scripts/record-scope-escape.sh"
echo ""
echo "  manual: run bash scripts/demo-scope-escape.sh and capture terminal output"