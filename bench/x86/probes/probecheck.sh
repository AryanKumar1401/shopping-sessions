# Check the scorer-probe build on Linux: same output as main, and the cold/warm detection.
set -u
drop() { sync; sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }
git worktree add -q --detach /tmp/old 149402e
(cd /tmp/old/engine && CARGO_TARGET_DIR=/tmp/t-old cargo build -q --release --locked --target $T 2>/dev/null && cp /tmp/t-old/$T/release/shopping-engine /tmp/old-eng)
python3 -m venv /tmp/venv && /tmp/venv/bin/pip -q install polars pyarrow >/dev/null 2>&1
for D in /tmp/s3 /tmp/s5; do
  /tmp/old-eng $D /tmp/ref >/dev/null 2>&1
  for mode in warm cold; do
    [ $mode = cold ] && drop
    s=$(date +%s%N); SE_THREADS=8 /tmp/eng $D /tmp/o 2>/tmp/err; rc=$?; e=$(date +%s%N)
    echo "$D $mode: rc=$rc wall $(( (e-s)/1000000 )) ms | $(grep probe /tmp/err) | $(/tmp/venv/bin/python scripts/compare.py /tmp/o /tmp/ref 2>&1 | tail -1)"
  done
done
drop; s=$(date +%s%N); /tmp/eng /tmp/s3 /tmp/o 2>/tmp/err; e=$(date +%s%N); echo "/tmp/s3 cold, default threads: wall $(( (e-s)/1000000 )) ms | $(grep probe /tmp/err)"
ls -ln /tmp/s3/events | head -3
