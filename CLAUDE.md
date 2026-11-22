# Shopping Sessions: handoff for a fresh agent

This file is the single entry point. Read it fully, then `rules.md`, then `docs/CONTRACT.md`.

## The task

Databricks "Shopping Sessions" buildathon track. Read messy click-stream CSVs from `$DATA_DIR`, write two
exact Parquet tables to `$OUTPUT_DIR`. **Score = container wall time, start to exit; an almost-right table
scores nothing.** `rules.md` is the authoritative spec and must never be edited.

- Scorer: linux/amd64, GCP n2 (Cascade Lake, 4 cores / 8 hyperthreads, AVX-512 present), 24 GB, no network,
  read-only root, uid 1000. Practice runs: ~20M events, 2 submissions/hour. **The final is on ~60M events
  (median of 3, top 10 re-timed 7 times), 3-minute hard cap.** Optimise 60M at least as much as 20M.
- Cutoff: Sunday 22 Nov 2026, 11:00 AM EST.
- Submission = a commit SHA of the public GitHub repo with `Dockerfile` at the root. Always submit by exact
  SHA. Every submitted commit must stay in the repo, so never rewrite history on `main`.

## Score history (scorer, hidden 20M set)

| Commit | What | Score |
| --- | --- | --- |
| 939e5d0 | first correct Rust engine | 1.36 s |
| 2fe17ed | AVX2 movemask codegen fix, write-combining writers | 0.97 s |
| ca6f306 | scanner rewrite + people-pass rewrite | 0.71 s |
| f35b7cd | row-hopping event kernel (`hop.rs`) | 0.6146 s |

Leaderboard leader was reported at 0.28 s (unverified how). The user's stated goal is < 0.27 s. Nobody on the
team has found a design that gets near it; see "Honest assessment" below.

## Repository layout

| Path | What |
| --- | --- |
| `Dockerfile` | Builds `engine/` as a static binary into a `FROM scratch` image. x86-64-v3 baseline. |
| `engine/` | **The submitted Rust engine** (zero dependencies). |
| `oracle/oracle.py` | Slow, obviously-correct Python reference. Matches the published dev answer. |
| `duck/pipeline.py` | Independent DuckDB implementation (exact; 95 s at 60M). Second opinion and fallback. |
| `fixtures/` | The 36 official hand-written cases. |
| `fixtures_extra/` | 31 hand-derived adversarial cases (X01-X31) + QA reproducers (`qa_*`). |
| `qa/` | Fuzzer (`gen.py`), differential runner with shrinker (`diff.py`), gate (`run_all.sh`). |
| `bench/` | Benchmark and gate scripts, `NOTES.md` (all measurements and dead ends), `SCOREBOARD.md`. |
| `scripts/` | The organisers' `run_fixtures.py`, `compare.py`, `docker_run.sh`. |
| `docs/research.md` | Early algorithm memo (estimates, partly superseded). |

Git-ignored and local only: `data/` (datasets), `downloads/` (official starters + generator source),
`out/` (worktrees, scratch outputs, VM helper scripts), `.venv/`.

## Engine architecture (`engine/src/`)

Everything runs on `NCPU` threads over work queues; the process ends with `_exit(0)`.

1. `main.rs` / `fast.rs::load_inputs`: list every regular `*.csv`, mmap, parse manifest.
2. `fast.rs::run`: product and campaign tables; the **link table builds on 2 background threads** during
   parse and dedup (`links.rs`: partitioned hash table + lock-free union-find, root = byte-wise smallest ID).
3. **Parse** (2 MiB chunks cut at newlines):
   - events: `hop.rs` row-hopping kernel (default on x86-64 AVX2; `SE_KERNEL=scan` restores the old one).
     Walks a row left to right using fixed-shape templates; anything unusual (0.74% of rows) goes to the
     exact path (`split_row` + `eval_event`). ~316 instructions per row.
   - clicks: `hop.rs::hop_clicks`, the same walk for the four click fields (since 24f0def); links and the
     fallbacks: `scan.rs` generic scanner (SIMD masks -> delimiter flag bytes via pext/pdep).
   - IDs are packed 7 bits/char into a u64 when <= 9 bytes and ASCII; anything else is interned exactly.
   - Each good event writes `PRec{uid u64, ts u32, cat*K+price u32}` to the partition of its **user-ID hash**
     and every well-formed copy writes `DRec{eid u64, ingest u32 (seconds), prec index u32}` to the partition
     of its **event-ID hash**, through write-combining non-temporal writers (`util.rs`).
4. **Dedup**: per event-ID partition, open-addressing table; the copy with the larger `ingest_ts` loses and
   its PRec is marked dead in place. An invalid-but-well-formed copy competes and can win.
5. **People** (`person.rs`): pass A per uid-partition groups by raw ID, does one link lookup per distinct ID,
   finishes unlinked IDs in place, forwards linked IDs as runs; pass B groups runs by person. Sessions come
   from 30-minute buckets (min/max/count, neighbours chained by the exact gap test), not from sorting.
   Credit and assists in one sweep; dense `[campaign][day]` counters; `parquet.rs` writes PLAIN, uncompressed.
6. `slow.rs`: exact single-threaded general path. Used whenever a fast-path precondition fails
   (window span >= 2^32 ms, > 429 categories, huge counter grid, `ingest_ts` not on a whole second, >= 2^31
   events, ...). Force it with `SE_SLOW=1`. **Every fast path must keep an exact fallback.**

Useful knobs: `SE_TIMING=1` (per-phase wall/user/sys), `SE_THREADS`, `SE_KERNEL=hop|scan` (events), `SE_CKERNEL=hop|scan` (clicks), `SE_SIMD=scalar|swar|sse2|avx512`,
`SE_CHUNK`, `SE_SLOW=1`, `SE_PBITS`/`SE_DBITS`, `SE_STAGE`, `--kernel-bench` (`KB_KIND=clicks` for the click kernel). Full list in `bench/README.md`.
Known bug: `SE_SIMD=avx512` with the hop kernel segfaults (opt-in only; AVX-512 is slower anyway).

## Where the time goes now (benchmark VM, commit f35b7cd)

| Phase | 20M | 60M |
| --- | --- | --- |
| parse events + clicks (link build overlapped) | ~394 ms | ~1176 ms |
| dedup | ~43 ms | ~125 ms |
| people A + B (+ user_daily) | ~194 ms | ~580 ms |
| **total** | **~672 ms** | **~1963 ms** |

The scorer runs the same commit ~9% faster than that VM instance (0.6146 s). Peak RSS 1.3 GB / 3.7 GB.
Inside event parsing (253 ms at 8 threads), the two record writes are ~94 ms and memory-bound; clicks are
~80 ms and still on the generic scanner.

## How to verify anything (the gate)

Nothing goes to `main` without all of this on the exact commit:

```sh
cd engine && cargo build --release && cd ..
E=engine/target/release/shopping-engine
qa/run_all.sh $E                          # fixtures, fixtures_extra, 400 fuzz seeds vs the oracle (macOS only: it hangs on Linux)
SE_SLOW=1 qa/run_all.sh $E                # the same through the general path
SE_CHUNK=7 SE_THREADS=1 .venv/bin/python scripts/run_fixtures.py fixtures_extra -- $E
DATA_DIR=data/dev OUTPUT_DIR=out/x $E && .venv/bin/python scripts/compare.py out/x data/dev-answer   # must print MATCH
```

Plus, on real x86: 20M and 60M outputs must MATCH the DuckDB outputs (`duck/pipeline.py`), and an interleaved
A/B (`bench/bench.py`) against the previous commit. On Linux use `bench/gate.sh` or `bench/qa_diff_spawn.py`
instead of `qa/run_all.sh`. On an arm64 Mac the hop kernel is off by default: also run the gate with
`SE_KERNEL=hop SE_CKERNEL=hop` to exercise the portable versions of both kernels.

Datasets come from the organisers' generator (`downloads/generator`, std-only Rust):
`generate --seed 1 --events 1000000 --clicks 250000 --output data/dev` (answer in `data/dev-answer`),
seed 3 / 20M / 5M clicks -> `s3-20m`, seed 5 / 60M / 15M clicks -> `s5-60m`. Same seed = byte-identical data.

## Benchmark machines

Timing is only meaningful on a scorer-class x86 box (GCP `n2-standard-8`, Cascade Lake). The Mac used for
development is arm64 with 8 GB: correctness only. Connection details, helper scripts (`out/vm.sh`,
`out/vmcp.sh`) and the shared-VM rules are in the git-ignored `out/vmbench.md` and `out/HANDOFF-private.md`
on the development machine. Rules that were learned the hard way:

- every build and timing run inside `flock /tmp/vm.lock timeout N ...`; one engine process at a time;
- wrap every remote call in `timeout`; the cloud login expires about hourly and needs the user;
- VM instances differ by up to ~20%: only compare numbers taken interleaved in one session;
- the VM has no hardware counters; callgrind works and gives exact instructions per source line.

A collaborator (GitHub `hrishabhayush`) also pushes to this repo and benchmarks on GitHub Actions runners
(branch `bench-x86`, AMD EPYC 4 vCPU). Fetch before pushing; merge, never force-push.

## What was tried and did not help (do not repeat)

Details and numbers are in `bench/NOTES.md`.

- I/O: pread instead of mmap, `MAP_POPULATE`, `MADV_POPULATE_READ`, `WILLNEED`, arena pre-fault, smaller chunks.
- 512-bit AVX-512 (downclocks: slower); AVX-512VL at 256 bit (no instruction saving).
- Profile-guided optimisation: 748 vs 746 ms, nothing.
- Fewer threads; different partition counts (parse and dedup/people trade off, net flat).
- Per-row link lookup inside the parser (+90-110 ms); shared lock-free dedup table; bitmap prefilter for dedup.
- Batched structure-of-arrays event kernel (branch `rad-y`): exact, +3.5% only. AVX2 gathers cost 8.1 ns per
  8 lanes on this hardware vs 2.6 ns for loads + shuffles, so gather-based designs lose.
- Batched click kernel (branch `rad-y-clicks`): exact, no gain.
- Two row cursors per chunk in the hop kernel: no gain.
- People phases: prefetching, smaller tables, lower load factor, small-person bitmask sweep (branch
  `sweep-z`, exact, zero gain: small people are 90% of people but 53% of sessions and their click logic is
  ~11 ms). These phases are latency/branch-bound, not instruction-bound.
- Dedup with 4-byte slots in 8-slot buckets (branch `rad-z`): 3 ms / 8 ms; RLE_DICTIONARY Parquet output:
  3x smaller files, no time saved, **deliberately not merged** (reader-compatibility risk with the checker).
- Building the small tables on a thread during file mapping (collaborator's `startup` branch): 9 ms slower on Linux.

## Open ideas, roughly by expected value

1. Record writes in the event kernel (~94 of 253 ms, memory-bound): WC unit/stream count, first-touch cost,
   or one 32-byte record per event into the event-ID partition with dedup forwarding survivors as PRecs to
   user-ID partitions (removes one stream and the dead-marking). In progress when this was written.
2. ~~A hop-style kernel for click rows~~ Done in 24f0def (`hop.rs::hop_clicks`, generic scanner as the exact fallback):
   GitHub x86 runner, interleaved, 20M 899 -> 878 ms, 60M 2668 -> 2603 ms (-2.4% both); not yet timed on the VM or the scorer.
3. Two cheap hop cuts: quote flags from masks already loaded; midnight in ms in the date memo.
4. People: the general sweep for people with 17-256 sessions (40% of sessions) and row formatting
   (~180 thread-ms); bucket aggregation directly in pass A's event scan (est. 12-17 ms).
5. System time is ~0.5 thread-seconds (~9% of CPU): page-cache mapping faults and arena first touch.

## Honest assessment

From 1.36 s to 0.61 s took a compiler-bug fix and three rewrites. Each remaining idea is worth 1-5%. Reaching
0.27 s would need the whole engine to run in ~0.2 s, about twice the 0.084 s it takes just to count newlines
in the input with 8 threads; no measured evidence suggests an exact pipeline of this shape can do that on
this hardware. A realistic target with everything above landing is ~0.50 s on the scorer.

Do not try to win by gaming the harness (for example caching results between the three timed runs): it is
not what is being measured and risks disqualification.

## Working rules

- Correctness first, always. Preserve every failing case as a fixture.
- Measure, do not guess; report what was not verified.
- One writer per file set; experiments live on branches / worktrees (`out/wt-*`), `main` only gets gated commits.
- Check `git status` before committing (a build directory was committed once by accident).
- The user's private details (cloud project, email) never go into this public repo.
