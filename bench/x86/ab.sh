#!/bin/bash
# ab.sh OLD NEW [full]: A/B two pushed commits on the real-x86 GitHub runner and print the verdict lines.
# Quick mode (default, ~8 min): fixtures, 20M and 60M outputs vs OLD, unit tests, interleaved benches.
# "full" (~20 min): also 400+100 oracle fuzz seeds, DuckDB, the Docker image.
# Both commits must already be pushed to origin. Runner CPUs differ between runs: only compare within one run.
set -u
OLD=$1; NEW=$2; MODE=${3:-quick}
REPO=AryanKumar1401/shopping-sessions
before=$(gh run list -R $REPO --workflow bench-x86 --limit 1 --json databaseId --jq '.[0].databaseId')
if [ "$MODE" = full ]; then gh workflow run bench-x86 -R $REPO --ref bench-x86 -f old=$OLD -f new=$NEW >/dev/null; else gh workflow run bench-x86 -R $REPO --ref bench-x86 -f old=$OLD -f new=$NEW -f quick=1 >/dev/null; fi
# find the run this call started: the newest one whose title carries nothing we can match, so poll until a new id appears
for i in $(seq 30); do sleep 5; R=$(gh run list -R $REPO --workflow bench-x86 --limit 5 --json databaseId,createdAt --jq 'sort_by(.createdAt) | reverse | .[0].databaseId'); [ "$R" != "$before" ] && break; done
echo "run $R (https://github.com/$REPO/actions/runs/$R)"
until [ "$(gh run view -R $REPO $R --json status --jq .status)" = completed ]; do sleep 20; done
echo "conclusion: $(gh run view -R $REPO $R --json conclusion --jq .conclusion)"
J=$(gh run view -R $REPO $R --json jobs --jq '.jobs[] | select(.conclusion!="skipped") | .databaseId' | head -1)
gh api repos/$REPO/actions/jobs/$J/logs | cut -c30- | grep -E "Model name|checking|fixtures passed|failures:|rc=|vs old:|vs DuckDB:|test result|FAILED|panicked|^== |wall  min|parse events|dedup|people" | grep -v "36;1m"
