#!/usr/bin/env bash
# Fail CI/local checks if tracked files contain likely secrets or home-path leaks.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail=0

# Patterns that must not appear in tracked source (placeholders like sk-or-... are OK).
scan_files() {
  git ls-files -z \
    | rg -z -v '^(target/|\.git/)' \
    | xargs -0 rg -n -H \
        -e 'sk-[a-zA-Z0-9]{20,}' \
        -e 'ghp_[a-zA-Z0-9]{20,}' \
        -e 'xox[baprs]-[a-zA-Z0-9-]{10,}' \
        -e 'AKIA[0-9A-Z]{16}' \
        -e 'BEGIN (RSA |OPENSSH |EC )?PRIVATE KEY' \
        -e '/home/unknown' \
        2>/dev/null || true
}

hits="$(scan_files)"
if [[ -n "$hits" ]]; then
  echo "check-no-secrets: possible secret or home-path leak in tracked files:"
  echo "$hits"
  fail=1
fi

# Machine-local paths must never be tracked.
for forbidden in .aden/savings.json .aden/project.conf .aden/config.toml .codex/config.toml; do
  if git ls-files --error-unmatch "$forbidden" &>/dev/null; then
    echo "check-no-secrets: forbidden tracked file: $forbidden"
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi

echo "check-no-secrets: OK"