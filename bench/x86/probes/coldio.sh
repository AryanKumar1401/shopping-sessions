# Cold page cache: how fast can the input be read at all, and which engine I/O knobs do best.
drop() { sync; sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }
ms() { s=$(date +%s%N); "$@" >/dev/null 2>&1; e=$(date +%s%N); echo $(( (e-s)/1000000 )); }
df -h /tmp | tail -1; lsblk -o NAME,SIZE,ROTA,RA,MODEL 2>/dev/null | head -5
for D in /tmp/s3 /tmp/s5; do
  echo "### $D ($(du -sh $D | cut -f1))"
  drop; echo "floor: cat all files, 1 stream:  $(ms sh -c "cat $D/events/*.csv $D/clicks/*.csv $D/links/*.csv") ms"
  drop; echo "floor: cat, 8 parallel streams:  $(ms sh -c "ls $D/events/*.csv $D/clicks/*.csv | xargs -P8 -n4 cat") ms"
  drop; echo "floor: cat, 32 parallel streams: $(ms sh -c "ls $D/events/*.csv $D/clicks/*.csv | xargs -P32 -n1 cat") ms"
  echo "warm engine: $(ms /tmp/eng $D /tmp/o) ms"
  for kn in "" "SE_WILLNEED=1" "SE_POPREAD=1" "SE_POPULATE=1" "SE_READ=1" "SE_READ=1 SE_CHUNK=8388608" "SE_THREADS=16" "SE_THREADS=32" "SE_THREADS=32 SE_WILLNEED=1" "SE_DROP=0"; do
    r=""; for i in 1 2 3; do drop; r="$r $(ms env $kn /tmp/eng $D /tmp/o)"; done
    printf "cold engine %-30s:%s ms\n" "${kn:-default}" "$r"
  done
done
