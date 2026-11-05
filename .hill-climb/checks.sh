#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
node --test tests/palette-decoder.test.mjs
node .hill-climb/checks.mjs
