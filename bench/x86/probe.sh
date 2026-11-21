#!/bin/bash
# Run on the x86 runner after the binaries (out/perf/<name>, /tmp/<name>-dbg) and data/s3-20m exist.
set -u
D=data/s3-20m
best() { sort -n | head -1; }
echo "### parse stages (wall ms, user ms; min of 5)"
for n in $(cut -d: -f1 bench/x86/revs); do
  for s in 1 2 3 6 7 0; do
    printf "%-6s stage %s  " $n $s
    for i in 1 2 3 4 5; do SE_STAGE=$s SE_TIMING=1 out/perf/$n $D /tmp/o 2>&1 | grep 'parse events' | awk '{print $5, $8}'; done | best
  done
done
n=$(head -1 bench/x86/revs | cut -d: -f1)
echo "### perf: $n"
sudo perf record -q -F 4999 -o /tmp/perf.data /tmp/$n-dbg $D /tmp/o >/dev/null 2>&1
sudo perf report -i /tmp/perf.data --no-children --sort sym --stdio 2>/dev/null | grep -E "^ +[0-9]" | head -15
for sym in shopping_engine::fast::event_row shopping_engine::fast::main_d; do
  echo "### hottest instructions in $sym"
  sudo perf annotate -i /tmp/perf.data --stdio -l "$sym" 2>/dev/null | grep -E "^\s+[0-9]+\.[0-9]+ " | sort -rn | head -45
done
