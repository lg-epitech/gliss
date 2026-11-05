#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
exec taskset -c 0,2,4,6 node .hill-climb/benchmark.mjs
