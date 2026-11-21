#!/bin/sh
# Quick correctness check of a binary (not the full gate; see bench/gate.sh):
# official + extra fixtures, the dev set against its published answer.  bench/check.sh BINARY [ENV=V...]
cd "$(dirname "$0")/.."
E="$1"; shift
PY=.venv/bin/python
for d in fixtures fixtures_extra; do
  env "$@" $PY scripts/run_fixtures.py $d -- "$E" 2>&1 | grep -v "^ok" | tail -4
done
rm -rf out/perf/chk-dev; env "$@" "$E" data/dev out/perf/chk-dev && $PY scripts/compare.py out/perf/chk-dev data/dev-answer | tail -1
