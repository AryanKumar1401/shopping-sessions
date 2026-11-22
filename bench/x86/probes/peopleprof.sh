# People-pass section profile ([prof] lines, thread-ms summed) of /tmp/eng, and of $OLDSHA if set, interleaved.
# Per line: the min over runs. Usage: gh workflow run bench-x86 --ref <branch> -f new=<sha> -f probe=peopleprof
set -u
OLDSHA=${OLDSHA:-}
ENGS="/tmp/eng"
if [ -n "$OLDSHA" ]; then
  git worktree add -q --detach /tmp/wt-o $OLDSHA
  (cd /tmp/wt-o/engine && CARGO_TARGET_DIR=/tmp/t-o cargo build -q --release --locked --target $T 2>/dev/null && cp /tmp/t-o/$T/release/shopping-engine /tmp/eng-old)
  ENGS="/tmp/eng-old /tmp/eng"
fi
for D in s3:9 s5:5; do
  d=${D%%:*}; n=${D##*:}
  for i in $(seq $n); do for e in $ENGS; do SE_TIMING=1 $e /tmp/$d /tmp/o 2>&1 | grep -E "\[prof\]|people|timing\] release" | sed "s|^|$e |" >> /tmp/prof-$d; done; done
  for e in $ENGS; do
    echo "== $d $e (min of $n)"
    grep "^$e \[prof\]" /tmp/prof-$d | awk '{k=""; for(i=3;i<=NF-5;i++) k=k" "$i; v=$(NF-4); if(!(k in m)||v<m[k]){m[k]=v}; if(!(k in o)){o[k]=++n; ks[n]=k}} END{for(i=1;i<=n;i++) printf "  %-18s %8.1f\n", ks[i], m[ks[i]]}'
    grep "^$e .*people A" /tmp/prof-$d | awk '{print $9}' | sort -n | head -1 | sed 's/^/  people A wall min /'
    grep "^$e .*people B" /tmp/prof-$d | awk '{print $9}' | sort -n | head -1 | sed 's/^/  people B wall min /'
    grep "^$e .*timing\] release" /tmp/prof-$d | awk '{t=$(NF-1); gsub(/[^0-9.]/,"",t); print t}' | sort -n | head -1 | sed 's/^/  total min /'
  done
done
