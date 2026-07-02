#!/usr/bin/env bash
# Demonstrates coxn labels sandbox vs NO SANDBOX in run_command approval.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "Run coxn, ask the model to run: echo hello"
echo "Approval prompt shows [sandbox] when bwrap is installed, else [NO SANDBOX]."
exec cargo run --quiet