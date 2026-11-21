# bench/

| script | what it does |
| --- | --- |
| `bench.py` | Interleaved A/B of engine binaries on one dataset: wall, user/sys, peak RSS, page faults, per-phase times, start+exit overhead, min and median of N; instructions/cycles on macOS. `--record` appends to `results.jsonl`. |
| `scoreboard.py` | Renders `results.jsonl` + `NOTES.md` into `SCOREBOARD.md`. |
| `vm_sync.sh LABEL` | Copies `engine/` and `bench/` to the benchmark VM (`~/ss`) and builds `~/ss/out/perf/LABEL` there. Timing belongs on that VM: `out/vm.sh 'cd ~/ss && python3 bench/bench.py --data data/s3-20m --reps 5 --record a=out/perf/A b=out/perf/B'`. |
| `gate.sh BINARY` / `linux:LABEL` / `amd64:LABEL` | The regression gate: fixtures, fixtures_extra, fuzz seeds vs oracle, the same with `SE_SLOW=1`, dev / 5M / 20M (and 60M on the VM) against references. `GATE_ENV="SE_X=1"` gates a knob. |
| `qa_diff_spawn.py` | `qa/diff.py` with the spawn start method (its forked workers hang on Linux). Used by `gate.sh` on Linux. |
| `check.sh BINARY [ENV=V]` | Quick check: both fixture sets and the dev set. |
| `linux_build.sh LABEL [PLATFORM]` | Builds a static Linux binary in a container into `out/perf/linux/LABEL` (arm64 native on the Mac, or amd64 emulated). |
| `container_bench.sh DATASET ...` | `bench.py` inside a linux/arm64 container with the scorer's docker flags; `DROP=1` drops the page cache before every run. |
| `linux_engine.sh` | Runs a Linux build on host paths through a helper container, so host harnesses can gate Linux-only code. |

References for the gate: `out/perf/ref20` (copy of `out/lead/s3-20m` on the Mac; the 939e5d0 engine's output on the VM), `out/perf/ref60` (VM only).

Engine knobs (all optional): `SE_TIMING=1`, `SE_THREADS`, `SE_SIMD=scalar|swar|sse2|avx512`, `SE_CHUNK`, `SE_SLOW=1`, `SE_STAGE=1..3`,
`SE_READ=1` (pread chunks instead of parsing the mapping), `SE_WC=0|1` (write-combining writers; default on for x86-64),
`SE_DROP=0`, `SE_RELEASE=0`, `SE_DBITS`/`SE_PBITS` (partition bits), `SE_NOTHP=1`, `SE_REPEAT=N` (N-1 rehearsal runs into `$SCRATCH_DIR` first),
`SE_POPREAD`, `SE_POPULATE`, `SE_POPULATE_ANON`, `SE_WILLNEED` (measured, no gain).
