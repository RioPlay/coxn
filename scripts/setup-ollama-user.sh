#!/usr/bin/env bash
# Install Ollama to ~/.local (no sudo) and print start instructions.
# Prefer `sudo pacman -S ollama-cuda` on Arch when sudo is available.
set -euo pipefail

VERSION="${OLLAMA_VERSION:-v0.31.1}"
BIN_DIR="${HOME}/.local/bin"
LIB_DIR="${HOME}/.local/lib/ollama"
TMP="${TMPDIR:-/tmp}/ollama-setup-$$"
ARCHIVE="$TMP/ollama-linux-amd64.tar.zst"
URL="https://github.com/ollama/ollama/releases/download/${VERSION}/ollama-linux-amd64.tar.zst"

fail() {
    echo "[setup-ollama] $*" >&2
    exit 1
}

command -v curl >/dev/null || fail "curl required"
command -v tar >/dev/null || fail "tar required"
tar --help 2>&1 | grep -q zstd || fail "tar with zstd support required"

mkdir -p "$TMP" "$BIN_DIR" "${HOME}/.local/lib"
trap 'rm -rf "$TMP"' EXIT

echo "[setup-ollama] downloading $URL"
curl -fsSL "$URL" -o "$ARCHIVE"
tar -I zstd -xf "$ARCHIVE" -C "$TMP"

install -m 755 "$TMP/bin/ollama" "$BIN_DIR/ollama"
rm -rf "$LIB_DIR"
cp -a "$TMP/lib/ollama" "$LIB_DIR"

echo "[setup-ollama] installed:"
echo "  binary: $BIN_DIR/ollama"
echo "  libs:   $LIB_DIR"
echo ""
echo "Ensure ~/.local/bin is on PATH, then in one terminal:"
echo "  ollama serve"
echo "In another:"
echo "  ollama pull tinyllama    # or qwen2.5-coder"
echo "  bash scripts/smoke-ollama.sh"
echo ""
echo "Arch with sudo: sudo pacman -S ollama-cuda && systemctl enable --now ollama"