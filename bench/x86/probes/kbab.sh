# Event kernel alone, interleaved A/B: the workflow's `new` (/tmp/eng) and every commit in
# kbab.list, old-format and new-format seed-3 20M, 1 and 4 threads, 3 reps per process.
# Prints min / median per (data, threads, binary) over the reps (first rep of each process
# dropped), and the distinct record lines.
set -u
rm -rf /tmp/s5   # (the probe job made the 60M set: keep the page cache for these)
(cd bench/x86/generator-old && RUSTFLAGS= cargo build -q --release && ./target/release/generate --seed 3 --events 20000000 --clicks 5000000 --output /tmp/s3old >/dev/null)
BINS="new=/tmp/eng"
for s in $(cat bench/x86/probes/kbab.list); do
  git worktree add -q --detach /tmp/wt-$s $s
  (cd /tmp/wt-$s/engine && CARGO_TARGET_DIR=/tmp/t-$s cargo build -q --release --locked --target $T 2>/dev/null && cp /tmp/t-$s/$T/release/shopping-engine /tmp/b-$s)
  BINS="$s=/tmp/b-$s $BINS"
done
R=/tmp/kbab.txt; : > $R
for round in 1 2 3 4 5; do
  order=$BINS; [ $((round % 2)) = 0 ] && order=$(echo $BINS | tr ' ' '\n' | tac | tr '\n' ' ')
  for d in s3old s3; do for th in 1 4; do for b in $order; do
    n=${b%%=*}; e=${b#*=}
    KB_KIND=events KB_KERNELS=hop KB_THREADS=$th KB_REPS=3 $e --kernel-bench /tmp/$d /tmp/o 2>&1 | grep kernel= | sed "s/^/$d t$th $n /" >> $R
  done; done; done
done
python3 - $R <<'EOF'
import sys, collections
ms = collections.defaultdict(list); recs = set(); seen = collections.Counter()
for line in open(sys.argv[1]):
    f = line.split(); k = tuple(f[:3]); seen[k] += 1
    kv = dict(x.split('=', 1) for x in f[3:] if '=' in x)
    recs.add((f[0], f[2], kv['drecs'], kv['good'], kv['slow'], kv['hash'], kv['long']))
    if seen[k] % 3 != 1:
        ms[k].append(float(kv['ms']))
print("== summary (ms)")
for k in sorted(ms):
    v = sorted(ms[k]); print(*k, "min=%.1f med=%.1f n=%d" % (v[0], v[len(v) // 2], len(v)))
print("== records")
for r in sorted(recs): print(*r)
EOF
