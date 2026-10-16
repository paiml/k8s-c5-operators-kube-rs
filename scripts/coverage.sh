#!/usr/bin/env bash
# 100% line coverage by execution tracing, plus the 0% falsifier.
# src/glue.rs is the only file excluded: it is the thin layer over a real API server.
set -euo pipefail
cd "$(dirname "$0")/.."
IGNORE='tests/|src/glue\.rs'
cargo llvm-cov --fail-under-lines 100 --ignore-filename-regex "$IGNORE" --test unit --summary-only
# Falsifier: a passing test that calls nothing must measure exactly 0.00% over a non-empty line count.
report=$(cargo llvm-cov --ignore-filename-regex "$IGNORE" --test falsifier --summary-only 2>/dev/null)
total=$(printf '%s\n' "$report" | awk '$1 == "TOTAL" { print $8, $10 }')
if [ "$total" != "${total#0 }" ] || [ -z "$total" ] || [ "${total##* }" != "0.00%" ]; then
  echo "falsifier FAILED: a test that calls nothing reported lines/cover = '${total}'" >&2
  exit 1
fi
echo "falsifier OK: a test that calls nothing reports ${total##* } of ${total%% *} lines"
