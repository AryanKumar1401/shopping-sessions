# Event and click kernels alone, old-format vs new-format 20M, 1 and 4 threads.
(cd bench/x86/generator-old && RUSTFLAGS= cargo build -q --release && ./target/release/generate --seed 3 --events 20000000 --clicks 5000000 --output /tmp/s3old >/dev/null)
for d in s3old s3; do for k in events clicks; do echo "== kernel-bench $d $k"; KB_KIND=$k KB_KERNELS=hop KB_THREADS=1,4 KB_REPS=3 /tmp/eng --kernel-bench /tmp/$d /tmp/o 2>&1 | tail -8; done; done
