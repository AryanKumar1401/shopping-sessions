# Partition-count sweep at 60M and 20M: total wall (ms) from the engine's own timer, min and median of N.
run() { # data reps env...
  d=$1; n=$2; shift 2
  for i in $(seq $n); do env "$@" SE_TIMING=1 /tmp/eng $d /tmp/o 2>&1 | awk '/timing\] parse/{p=$5} /timing\] dedup/{dd=$5} /people A/{a=$8} /people B/{b=$8} /timing\] release/{gsub(/[^0-9.]/,"",$(NF-1)); t=$(NF-1)} END{printf "%.0f %.0f %.0f %.0f %.0f\n", t, p, dd, a, b}'; done | sort -n | awk -v n=$n '{l[NR]=$0} END{split(l[1],m," "); split(l[int((n+1)/2)],e," "); printf "total min %s med %s | at min: parse %s dedup %s A %s B %s\n", m[1], e[1], m[2], m[3], m[4], m[5]}'
}
for round in 1 2; do
  for cfg in "9 9" "10 10" "11 10" "10 11" "11 11" "12 12"; do set -- $cfg; printf "60M round $round PBITS=$1 DBITS=$2: "; run /tmp/s5 3 SE_PBITS=$1 SE_DBITS=$2; done
done
for cfg in "8 8" "9 9" "10 9" "9 10" "10 10"; do set -- $cfg; printf "20M PBITS=$1 DBITS=$2: "; run /tmp/s3 5 SE_PBITS=$1 SE_DBITS=$2; done
