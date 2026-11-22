# People-pass section profile ([prof] lines, thread-ms summed), interleaved variants. Per line: the min over runs.
# Usage: gh workflow run bench-x86 --ref probe-people3 -f new=<sha> -f probe=peopleprof
# VARIANTS: "label:binary:ENV=V,ENV=V" (binary old = $OLDSHA built here, new = the probe's commit)
set -u
OLDSHA=${OLDSHA:-f1938a5}
VARIANTS=${VARIANTS:-"old:old: new:new: b4:new:SE_BFM=4 b8:new:SE_BFM=8 b4l:new:SE_BFM=4,SE_LAHEAD=16"}
git worktree add -q --detach /tmp/wt-o $OLDSHA
(cd /tmp/wt-o/engine && CARGO_TARGET_DIR=/tmp/t-o cargo build -q --release --locked --target $T 2>/dev/null && cp /tmp/t-o/$T/release/shopping-engine /tmp/eng-old)
cp /tmp/eng /tmp/eng-new
for D in s3:9 s5:5; do
  d=${D%%:*}; n=${D##*:}
  for i in $(seq $n); do for v in $VARIANTS; do
    l=${v%%:*}; r=${v#*:}; b=${r%%:*}; e=${r#*:}
    env $(echo $e | tr ',' ' ') SE_TIMING=1 /tmp/eng-$b /tmp/$d /tmp/o 2>&1 | grep -E "\[prof\]|people|timing\] release" | sed "s|^|$l |" >> /tmp/prof-$d
  done; done
  for v in $VARIANTS; do
    l=${v%%:*}
    echo "== $d $v (min of $n)"
    grep "^$l \[prof\]" /tmp/prof-$d | awk '{k=""; for(i=3;i<=NF-5;i++) k=k" "$i; v=$(NF-4); if(!(k in m)||v<m[k]){m[k]=v}; if(!(k in o)){o[k]=++n; ks[n]=k}} END{for(i=1;i<=n;i++) printf "  %-18s %8.1f\n", ks[i], m[ks[i]]}'
    grep "^$l .*people A" /tmp/prof-$d | awk '{print $9}' | sort -n | head -1 | sed 's/^/  people A wall min /'
    grep "^$l .*people B" /tmp/prof-$d | awk '{print $9}' | sort -n | head -1 | sed 's/^/  people B wall min /'
    grep "^$l .*timing\] release" /tmp/prof-$d | awk '{t=$(NF-1); gsub(/[^0-9.]/,"",t); print t}' | sort -n | head -1 | sed 's/^/  total min /'
  done
done
