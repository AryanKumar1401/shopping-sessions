#!/bin/sh
# The regression gate for a kept change.
#   bench/gate.sh BINARY            native binary (macOS build)
#   bench/gate.sh linux:LABEL       Linux arm64 build out/perf/linux/LABEL, run in a container (bench/linux_engine.sh)
#   bench/gate.sh amd64:LABEL       Linux amd64 build, emulated (slow): set GATE_SKIP_20M=1 QA_SEEDS=0:60
# Steps: qa/run_all.sh (fixtures, fixtures_extra, fuzz seeds vs oracle), the same with SE_SLOW=1,
# data/dev vs data/dev-answer, data/s7-5m vs out/lead/s7-5m, data/s3-20m vs out/perf/ref20
# (a copy of out/lead/s3-20m, which was verified against DuckDB).
# Extra engine knobs for the whole gate: GATE_ENV="SE_READ=1 SE_CHUNK=4096"
set -u
cd "$(dirname "$0")/.."
PY=.venv/bin/python
case "$1" in
  linux:*) export SE_LINUX_BIN="${1#linux:}"; CMD=bench/linux_engine.sh ;;
  amd64:*) export SE_LINUX_BIN="${1#amd64:}" SE_LINUX_PLATFORM=linux/amd64; CMD=bench/linux_engine.sh ;;
  *) CMD="$1" ;;
esac
for kv in ${GATE_ENV:-}; do export "$kv"; done
W=out/perf/gate.$$; mkdir -p "$W"
st=0
echo "### gate: $1 ${GATE_ENV:-}"
gate_qa() { # label: fixtures, fixtures_extra, fuzz seeds vs oracle (qa/run_all.sh; on Linux its fuzz step via bench/qa_diff_spawn.py)
  if [ "$(uname)" = Darwin ]; then
    qa/run_all.sh "$CMD" 2>&1 | tail -6 | sed "s/^/$1: /"
  else
    for d in fixtures fixtures_extra; do $PY scripts/run_fixtures.py $d -- "$CMD" 2>&1 | grep -v "^ok" | tail -3 | sed "s/^/$1: /"; done
    $PY bench/qa_diff_spawn.py --seeds "${QA_SEEDS:-0:400}" --jobs "${QA_JOBS:-6}" --quiet --no-shrink --impl "target=$CMD" 2>&1 | tail -3 | sed "s/^/$1: /"
  fi
}
gate_qa fast
(export SE_SLOW=1; gate_qa slow)   # subshell: in sh an assignment before a function call would persist
cmp() { # name data ref
  rm -rf "$W/$1"; mkdir -p "$W/$1"
  if "$CMD" "$2" "$W/$1" 2>"$W/$1.err"; then
    r=$($PY scripts/compare.py "$W/$1" "$3" 2>&1 | tail -1); echo "$1: $r"
    case "$r" in MATCH*) ;; *) st=1 ;; esac
  else echo "$1: engine failed"; tail -3 "$W/$1.err"; st=1; fi
  rm -rf "$W/$1"
}
cmp dev data/dev data/dev-answer
[ -d out/lead/s7-5m/user_daily ] && cmp s7-5m data/s7-5m out/lead/s7-5m
[ -z "${GATE_SKIP_20M:-}" ] && cmp s3-20m data/s3-20m out/perf/ref20
# on the benchmark VM only: 60M events against the committed baseline engine's output
[ -z "${GATE_SKIP_20M:-}" ] && [ -d data/s5-60m ] && [ -d out/perf/ref60/user_daily ] && cmp s5-60m data/s5-60m out/perf/ref60
rm -rf "$W"
echo "### gate done (compare status $st; check GATE PASS lines above)"
exit $st
