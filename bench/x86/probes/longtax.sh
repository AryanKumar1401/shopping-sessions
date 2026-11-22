# Long-ID tax: the same engine on old-format (no 33-byte IDs) and new-format 20M data, interleaved,
# per-phase wall/user (min of 7), then the event and click kernels alone on both.
(cd bench/x86/generator-old && RUSTFLAGS= cargo build -q --release && ./target/release/generate --seed 3 --events 20000000 --clicks 5000000 --output /tmp/s3old >/dev/null)
du -sh /tmp/s3/events /tmp/s3old/events /tmp/s3/clicks /tmp/s3old/clicks
for i in 1 2 3 4 5 6 7; do for d in s3old s3; do SE_TIMING=1 /tmp/eng /tmp/$d /tmp/o 2>&1 | awk -v d=$d '/timing\] parse/{p=$5;pu=$8} /timing\] dedup/{dd=$5;du=$8} /long user/{l=$6;lu=$9} /people A/{a=$8;au=$11} /people B/{b=$8;bu=$11} /timing\] release/{t=$NF; gsub(/[^0-9.]/,"",t)} END{printf "%s %.0f %.0f %.0f %.0f %.0f %.0f %.0f %.0f %.0f %.0f %.0f\n", d,t,p,pu,dd,du,l,lu,a,au,b,bu}'; done; done > /tmp/rows
for d in s3old s3; do grep "^$d " /tmp/rows | awk -v d=$d '{for(i=2;i<=NF;i++) if(NR==1||$i<m[i]) m[i]=$i} END{printf "%-6s total %s | parse %s (user %s) dedup %s (%s) long %s (%s) A %s (%s) B %s (%s)\n", d,m[2],m[3],m[4],m[5],m[6],m[7],m[8],m[9],m[10],m[11],m[12]}'; done
for d in s3old s3; do for k in events clicks; do echo "== kernel-bench $d $k"; KB_KIND=$k KB_KERNELS=hop KB_THREADS=1,4 KB_REPS=3 /tmp/eng --kernel-bench /tmp/$d 2>&1 | tail -8; done; done
