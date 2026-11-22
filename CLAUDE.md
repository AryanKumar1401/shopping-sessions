# Shopping Sessions: handoff for a fresh agent

This file is the single entry point. Read it fully, then `rules.md`, then `docs/CONTRACT.md`.

**State at the end of the contest (Sun 22 Nov 2026): the team's final entry is commit `8685345`
(0.6151 s on the practice scorer). All engineering is stopped. Do not push engine changes to `main`
without the full gate below.**

## The task

Databricks "Shopping Sessions" buildathon track. Read messy click-stream CSVs from `$DATA_DIR`, write two
exact Parquet tables to `$OUTPUT_DIR`. **Score = container wall time, start to exit; an almost-right table
scores nothing.** `rules.md` is the authoritative spec and must never be edited.

- Scorer: linux/amd64, GCP n2 (Cascade Lake, 4 cores / 8 hyperthreads, AVX-512 present), 24 GB, no network,
  read-only root, uid 1000. Practice runs: ~20M events, 5 submissions/hour, and the same SHA cannot be
  resubmitted within 10 minutes. **The final rebuilds the picked commit and times it on ~60M new events
  (median of 3 runs; at least the 30 fastest are re-timed 9 times on one idle machine across 3 datasets),
  3-minute hard cap.** Optimise 60M at least as much as 20M.
- **The organisers changed the generator on Saturday night.** IDs are now opaque and variable-width, 1 in 8
  carries a 32-hex-digit suffix (33 bytes), and the scramble is a keyed Feistel (generator-inversion
  entries no longer work). `rules.md`, `downloads/generator` and the dev answer were refreshed; scores
  before and after the change are not comparable. Old-format data: `data/dev` + `data/dev-answer`;
  new-format: `data/dev2` + `data/dev-answer2`. Both must keep matching.
- Cutoff: Sunday 22 Nov 2026, 11:00 AM EST.
- Submission = a commit SHA of the public GitHub repo with `Dockerfile` at the root. Always submit by exact
  SHA. Every submitted commit must stay in the repo, so never rewrite history on `main`.

## Score history (scorer, hidden 20M set)

Old data format:

| Commit | What | Score |
| --- | --- | --- |
| 939e5d0 | first correct Rust engine | 1.36 s |
| 2fe17ed | AVX2 movemask codegen fix, write-combining writers | 0.97 s |
| ca6f306 | scanner rewrite + people-pass rewrite | 0.71 s |
| f35b7cd | row-hopping event kernel (`hop.rs`) | 0.6146 s |

New data format (after the generator change; everything before `d772e76` scored 5.3-5.6 s on it because
long IDs went through an interner behind one global mutex):

| Commit | What | Score |
| --- | --- | --- |
| d772e76 | lock-free sharded interner + long-ID fast paths | 0.7522 s |
| 603b496 | long user IDs resolved once per distinct ID per partition | 0.6840 s |
| a23d699 | (no real change in speed) | 0.6872 s |
| daa8610 / 645bf67 | long-ID record streams | 0.6672 s |
| dab6092 | | 0.6418 s |
| 8586c44 | flat people sweep (`person_sort.rs`); unaligned table loads; Rust pinned | 0.6347 s, 0.6486 s |
| 4f45de2 | close input fds after mmap | 0.6588 s |
| f1938a5 | parallel exact fallback | 0.6221 s |
| **8685345** | store-bypass-aware hot loops, JCC alignment flag, hop write order | **0.6151 s (final entry)** |

The same commit scored 0.6347 and 0.6486 on two submissions: **run-to-run noise on the scorer is about
±0.015 s**, so a single score cannot confirm a gain smaller than that. Only submit what an interleaved A/B
on a scorer-class machine already shows to be faster.

When last read, the board had 360 teams; this entry was around 10th-11th, the top three were
0.4508 / 0.4581 / 0.4597 s, and a cluster sat near 0.575 s.

## Repository layout

| Path | What |
| --- | --- |
| `Dockerfile` | Builds `engine/` as a static binary into a `FROM scratch` image. x86-64-v3 baseline. |
| `engine/` | **The submitted Rust engine** (zero dependencies). |
| `oracle/oracle.py` | Slow, obviously-correct Python reference. Matches the published dev answer. |
| `duck/pipeline.py` | Independent DuckDB implementation (exact; 95 s at 60M). Second opinion and fallback. |
| `fixtures/` | The 36 official hand-written cases. |
| `fixtures_extra/` | 35 hand-derived adversarial cases (up to X40, including long and hex-shaped IDs) + QA reproducers (`qa_*`). |
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
   - IDs are packed 7 bits/char into a u64 when <= 9 bytes and ASCII. A 33-byte hex-shaped ID (one prefix
     byte + 32 lowercase hex digits) is carried exactly as two u64 words. Any other ID goes through the
     lock-free sharded interner (`intern_long`, `long_bytes`, `id_less`). Rows with long IDs are written
     to their own record streams (`LRec`/`LHRec` for events, `URec`/`UHRec`/`UCRec`/`UCHRec` for users
     and clicks) partitioned on `lbits = min(dbits, 7)`; `resolve_long` then resolves each distinct long
     user ID once per partition and rejoins the short-ID flow.
   - Each good event writes `PRec{uid u64, ts u32, cat*K+price u32}` to the partition of its **user-ID hash**
     and every well-formed copy writes `DRec{eid u64, ingest u32 (seconds), prec index u32}` to the partition
     of its **event-ID hash**, through write-combining non-temporal writers (`util.rs`).
4. **Dedup**: per event-ID partition, open-addressing table; the copy with the larger `ingest_ts` loses and
   its PRec is marked dead in place. An invalid-but-well-formed copy competes and can win.
5. **People** (`person.rs`, flat sweep in `person_sort.rs`): pass A per uid-partition groups by raw ID, does one link lookup per distinct ID,
   finishes unlinked IDs in place, forwards linked IDs as runs; pass B groups runs by person. Sessions come
   from 30-minute buckets (min/max/count, neighbours chained by the exact gap test), not from sorting.
   Credit and assists in one sweep; dense `[campaign][day]` counters; `parquet.rs` writes PLAIN, uncompressed.
6. `slow.rs`: exact general path with no fast-path assumptions. Used whenever a fast-path precondition fails
   (window span >= 2^32 ms, > 429 categories, huge counter grid, `ingest_ts` not on a whole second, >= 2^31
   events, ...). Force it with `SE_SLOW=1`. It runs on `NCPU` threads (scalar line parser -> records
   partitioned by event-ID hash -> one `HashMap` dedup per partition -> per-person sort and sessions):
   19 s and 10.2 GB peak RSS at 60M events on the benchmark VM (20M: 6.7 s, 3.6 GB), so a fallback in the
   final stays far inside the 3-minute cap. `SE_SLOW=2` runs the original single-threaded reference
   (`run_simple`, 83 s at 20M) that it must agree with. **Every fast path must keep an exact fallback.**
   A fallback decided late calls `util::release_anon` first so the fast path's slabs are returned before
   the general path allocates. Worst case measured up to 120M events: 86 s, 20.1 GB (cap: 180 s, 24 GB).

Build and runtime details that are easy to undo by accident:

- **Store-bypass mitigation (SSBD).** The scorer's container appears to run with speculative store bypass
  disabled, which cost this engine about 9.5%. The hot loops are written "all loads first, then all stores"
  so a load never waits on an unresolved earlier store. Keep that shape when editing `hop.rs`,
  the writers in `util.rs` and the people passes. To reproduce on a test machine, run the engine under a
  wrapper that sets `prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, PR_SPEC_FORCE_DISABLE)`.
- **Dockerfile.** `FROM rust:1.100-bookworm` (pinned), `RUSTFLAGS="-C target-cpu=x86-64-v3
  -C llvm-args=-x86-branches-within-32B-boundaries -C target-feature=+crt-static"` (the middle flag avoids
  the Cascade Lake jump-alignment penalty), and
  `ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=33554432`.
- **Table loads in `hop.rs` are unaligned loads** (`_mm_loadu_si128` / `_mm256_loadu_si256`). An aligned
  load of a static table there was undefined behaviour that happened to work with one compiler version.
- Input file descriptors are closed right after mmap (kept only under `SE_READ`).

Useful knobs: `SE_TIMING=1` (per-phase wall/user/sys), `SE_THREADS`, `SE_KERNEL=hop|scan` (events), `SE_CKERNEL=hop|scan` (clicks), `SE_SIMD=scalar|swar|sse2|avx512`,
`SE_CHUNK`, `SE_SLOW=1`, `SE_PBITS`/`SE_DBITS`, `SE_STAGE`, `--kernel-bench` (`KB_KIND=clicks` for the click kernel). Full list in `bench/README.md`.
Known bug: `SE_SIMD=avx512` with the hop kernel segfaults (opt-in only; AVX-512 is slower anyway).

## Where the time went at f35b7cd (benchmark VM, old data format; historical)

These numbers predate the long-ID work and have not been re-measured in this form for `8685345`; run
`SE_TIMING=1` for current ones. The proportions (parse dominant, people second) still hold.

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
DATA_DIR=data/dev2 OUTPUT_DIR=out/y $E && .venv/bin/python scripts/compare.py out/y data/dev-answer2 # new ID format, must print MATCH
cd engine && cargo test --release
```

Plus, on real x86: 20M and 60M outputs, **in both the old and the new ID format**, must MATCH the DuckDB outputs (`duck/pipeline.py`), and an interleaved
A/B (`bench/bench.py`) against the previous commit. On Linux use `bench/gate.sh` or `bench/qa_diff_spawn.py`
instead of `qa/run_all.sh`. On an arm64 Mac the hop kernel is off by default: also run the gate with
`SE_KERNEL=hop SE_CKERNEL=hop` to exercise the portable versions of both kernels.

`8685345` passed a final-scale QA on top of the gate: 206 differential runs, about 11,500 fuzz seeds,
60M / 90M / 120M datasets, every fallback trigger, the real container (uid 1000, read-only root, no
network) and the store-bypass-disabled mode, with no mismatch. The record is `bench/FINAL_QA.md` on
branch `qa-final`.

Datasets come from the organisers' generator (`downloads/generator`, std-only Rust):
`generate --seed 1 --events 1000000 --clicks 250000 --output data/dev` (answer in `data/dev-answer`),
seed 3 / 20M / 5M clicks -> `s3-20m`, seed 5 / 60M / 15M clicks -> `s5-60m`. Same seed = byte-identical data.

## Benchmark machines

Timing is only meaningful on a scorer-class x86 box (GCP `n2-standard-8`, Cascade Lake). The Mac used for
development is arm64 with 8 GB: correctness only. Three benchmark VMs were used (two Cascade Lake, one
Ice Lake; the same binary runs about 20% faster on Ice Lake, so never mix their numbers). **All three are
stopped**, not deleted. Connection details, helper scripts (`out/vm*.sh`, `out/vmcp*.sh`) and the
shared-VM rules are in the git-ignored `out/vmbench*.md` and `out/HANDOFF-private.md` on the development
machine. Rules that were learned the hard way:

- never let two memory-heavy runs overlap (the VM hung on out-of-memory twice); keep `HOP_FUZZ_ROUNDS` <= 100;
- never `pkill -f` a pattern that also matches your own ssh command line (use `pgrep -f "[x]pattern"`);

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
- Resubmitting an unchanged commit hoping for a better draw: the noise is symmetric and it burns the
  submission budget. The cold-disk theory for the 5-6 s scores was wrong; the cause was the generator change.
- A global-mutex interner for non-packable IDs: fine when such IDs were rare, catastrophic (5+ s) once
  1 in 8 IDs was long.
- Other public entries were surveyed for ideas only (no code copied): the useful finds were the
  store-bypass mitigation and the jump-alignment flag, both now in.

## Open ideas, roughly by expected value

This list was written at f35b7cd and only partly revisited. Items 1-4 were explored further on experiment
branches (`out/wt-*` worktrees on the development machine) and what proved a measured gain is already in
`8685345`; treat the rest as untested leads, not promises.

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

From 1.36 s to 0.61 s on the old data took a compiler-bug fix and three rewrites; on the new data the engine
went from 5.6 s to 0.6151 s. The successive targets (0.55, 0.50, 0.45, 0.27 s) were **not reached**: the
best entries on the board are about 0.45 s, roughly 27% faster than this one, and the team did not find
what they do differently. Each idea tried in the last day was worth 1-3%, at or below scorer noise.
What is solid is correctness: the final entry has an exact fallback for every fast-path assumption and
stays far inside the time and memory caps at twice the announced final size.

Do not try to win by gaming the harness (for example caching results between the three timed runs): it is
not what is being measured and risks disqualification.

## Working rules

- Correctness first, always. Preserve every failing case as a fixture.
- Measure, do not guess; report what was not verified.
- One writer per file set; experiments live on branches / worktrees (`out/wt-*`), `main` only gets gated commits.
- Check `git status` before committing (a build directory was committed once by accident).
- The user's private details (cloud project, email) never go into this public repo.
