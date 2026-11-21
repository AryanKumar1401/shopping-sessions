#!/usr/bin/env bash
# Regression gate: fixtures/, fixtures_extra/ (incl. qa_*), then a fixed batch of fuzz seeds
# diffed against the oracle. Exits non-zero on any failure.
#
#   qa/run_all.sh [CMD...]            default CMD: engine/target/release/shopping-engine
#   QA_SEEDS=0:400 QA_JOBS=5 qa/run_all.sh .venv/bin/python duck/pipeline.py
#   QA_SHRINK=1 ...   shrink fuzz failures to out/qa/repro/target_<seed>/ (slower)
set -u
cd "$(dirname "$0")/.."
PY=.venv/bin/python
if [ $# -eq 0 ]; then set -- engine/target/release/shopping-engine; fi
CMD=("$@")
SEEDS=${QA_SEEDS:-0:400}
JOBS=${QA_JOBS:-5}
status=0

echo "== fixtures/ (${CMD[*]})"
out=$($PY scripts/run_fixtures.py fixtures -- "${CMD[@]}" 2>&1); rc=$?
echo "$out" | grep -v '^ok '
[ $rc -eq 0 ] || status=1

echo "== fixtures_extra/"
out=$($PY scripts/run_fixtures.py fixtures_extra -- "${CMD[@]}" 2>&1); rc=$?
echo "$out" | grep -v '^ok '
[ $rc -eq 0 ] || status=1

echo "== fuzz seeds $SEEDS vs oracle"
shrink=--no-shrink
[ -n "${QA_SHRINK:-}" ] && shrink=
# quote each word of CMD into one shell-split string for diff.py
cmdstr=$(printf '%q ' "${CMD[@]}")
$PY qa/diff.py --seeds "$SEEDS" --jobs "$JOBS" --quiet $shrink --impl "target=$cmdstr" || status=1

if [ $status -eq 0 ]; then echo "GATE PASS"; else echo "GATE FAIL"; fi
exit $status
