#!/bin/sh
# Runs a Linux build of the engine on host paths, so the host-side harnesses
# (scripts/run_fixtures.py, qa/run_all.sh, scripts/compare.py) can gate Linux-only code paths:
#   SE_LINUX_BIN=cand1 qa/run_all.sh bench/linux_engine.sh
# A helper container (se-gate-<platform>) stays up between calls; the repo and $TMPDIR are
# mounted at their host paths. SE_LINUX_BIN: a file in out/perf/linux/ (bench/linux_build.sh).
# SE_LINUX_PLATFORM: linux/arm64 (default) or linux/amd64 (emulated here). SE_* and NCPU pass through.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PLATFORM="${SE_LINUX_PLATFORM:-linux/arm64}"
NAME="se-gate-$(echo "$PLATFORM" | tr / -)"
TMP=$(cd "${TMPDIR:-/tmp}" && pwd -P)
if [ -z "$(docker ps -q -f name="^$NAME\$")" ]; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" --platform "$PLATFORM" --network none \
    -v "$ROOT":"$ROOT" -v "$TMP":"$TMP" -v /tmp:/tmp rust:1-bookworm sleep 86400 >/dev/null 2>&1 || true
  sleep 1
fi
n=$#; i=0; ARGS=""
for a in "$@"; do
  i=$((i+1))
  if [ $i -ge $((n-1)) ]; then a=$(cd "$a" && pwd -P); fi
  ARGS="$ARGS '$a'"
done
ENVS=""
for v in $(env | grep -E '^(SE_[A-Z_]*|NCPU)=' | cut -d= -f1); do
  case "$v" in SE_LINUX_*) ;; *) ENVS="$ENVS -e $v" ;; esac
done
eval exec docker exec $ENVS "$NAME" "$ROOT/out/perf/linux/${SE_LINUX_BIN:-cand}" $ARGS
