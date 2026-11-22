#!/bin/bash
# check_main.sh: if origin/main moved past the last verified commit, gate it locally (Mac) and on real x86.
# State: out/perf/verified (last verified sha). Prints "NO CHANGE" or a verdict block.
cd "$(dirname "$0")/../.." || exit 1
git fetch -q origin
OLD=$(cat out/perf/verified); NEW=$(git rev-parse --short=7 origin/main)
[ "$OLD" = "$NEW" ] && { echo "NO CHANGE (main still $NEW)"; exit 0; }
echo "main moved: $OLD -> $NEW"; git log --format='  %h %an: %s' $OLD..origin/main | cut -c1-160
git diff --stat $OLD origin/main -- engine Dockerfile | tail -1
rm -rf /tmp/claude-501/chk && git worktree prune && git worktree add -q --detach /tmp/claude-501/chk origin/main
(cd /tmp/claude-501/chk/engine && CARGO_TARGET_DIR=/tmp/claude-501/chk-t cargo build -q --release 2>&1 | grep -E "^error" -A5)
rm -f out/perf/cand && cp /tmp/claude-501/chk-t/release/shopping-engine out/perf/cand
echo "local: $(QA_JOBS=10 qa/run_all.sh out/perf/cand 2>&1 | tail -1)"
for d in s3-20m:out/perf/ref20 dev:data/dev-answer s249-1m:out/lead/s249 s7-5m:out/lead/s7-5m; do rm -rf out/x; out/perf/cand data/${d%%:*} out/x && echo "local ${d%%:*}: $(.venv/bin/python scripts/compare.py out/x ${d##*:} | tail -1)"; done
git worktree remove --force /tmp/claude-501/chk
gh workflow run bench-x86 --ref bench-x86 -f old=$OLD -f new=$NEW >/dev/null; sleep 15
R=$(gh run list --workflow bench-x86 --limit 1 --json databaseId --jq '.[0].databaseId')
until [ "$(gh run view $R --json status --jq .status)" = completed ]; do sleep 20; done
echo "x86 run $R: $(gh run view $R --json conclusion --jq .conclusion)"
J=$(gh run view $R --json jobs --jq '.jobs[0].databaseId'); gh api repos/AryanKumar1401/shopping-sessions/actions/jobs/$J/logs > out/perf/x86-check-$NEW.log
cut -c30- out/perf/x86-check-$NEW.log | grep -E "Model name|fixtures passed|failures:|rc=|vs old:|vs DuckDB:|^== |wall  min|parse events|dedup|people" | grep -v "36;1m"
echo "$NEW" > out/perf/verified.pending
