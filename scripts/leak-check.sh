#!/usr/bin/env bash
# Fails when private-production text is committed. Brackets keep this file from matching itself.
set -euo pipefail
cd "$(dirname "$0")/.."
if git grep -nE 'cop-inbo[x]|nvme-raid[0]|tie[r][:-]'; then echo "leak-check: FAIL"; exit 1; fi
echo "leak-check: OK"
