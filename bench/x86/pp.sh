#!/bin/bash
# pp.sh BIN [N]: person-pass numbers on the Mac, min of N runs: A wall, B wall, A+B user, total wall(t)
B=$1; N=${2:-9}; D=${D:-data/s3-20m}
for i in $(seq $N); do SE_THREADS=8 SE_TIMING=1 $B $D out/x 2>&1 | awk '/people A/{aw=$8; au=$11} /people B/{bw=$8; bu=$11} /timing\] release/{t=$NF; gsub(/[^0-9.]/,"",$(NF-1)); t=$(NF-1)} END{printf "%.1f %.1f %.0f %.0f\n", aw, bw, au+bu, t}'; done | awk '{if(NR==1||$1<a)a=$1; if(NR==1||$2<b)b=$2; if(NR==1||$3<u)u=$3; if(NR==1||$4<t)t=$4} END{printf "A wall %.1f  B wall %.1f  A+B user %.0f  total %.0f ms\n", a,b,u,t}'
