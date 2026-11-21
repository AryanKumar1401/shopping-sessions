# Scoreboard

One row per measured variant, appended by `bench/bench.py --record` (see `bench/README.md`).
All numbers are measurements on the environment named in the row; nothing here is extrapolated.
Times in ms: `min / median` of N interleaved runs. `wall` is fork/exec to exit of the engine process
(it does not include `docker run` start-up). `exit` = wall minus the engine's last phase timestamp
(process start + teardown). `instr` (G instructions, whole process) exists on macOS only.
Phases: wall min of links+tables / parse / dedup / person pass.

## Darwin/arm64 native

| date | variant | data | N | wall | user | sys | rss MB | minflt | majflt | exit | instr G | links / parse / dedup / person |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-11-21 00:49 | base_939e5d0 (Mac native) | s7-5m | 7 | 165 / 256 | 735 / 770 | 124 / 167 | 788 | 51378 | 19 | 14.5 / 18.5 | 6.74 | 7 / 82 / 7 / 34 |
| 2026-11-21 00:49 | cand1_gated (Mac native) | s7-5m | 7 | 186 / 226 | 751 / 781 | 128 / 148 | 746 | 48659 | 1 | 12.1 / 15.9 | 7.12 | 7 / 83 / 7 / 52 |
| 2026-11-21 00:49 | experimental (Mac native) | s7-5m | 7 | 196 / 223 | 736 / 765 | 148 / 170 | 796 | 51894 | 1 | 14.6 / 19.0 | 7.20 | 7 / 61 / 7 / 63 |

## Benchmark VM (the scorer-class machine): GCP n2-standard-8, Xeon 2.8 GHz Cascade Lake, 4 cores / 8 threads, 31 GB, Ubuntu 24.04, THP=madvise

Native binaries (`-C target-cpu=x86-64-v3`), warm page cache, run with `bench/bench.py` on the VM, 2026-11-21.
These rows were recorded on the VM (`~/ss/bench/results.jsonl` there; the VM was stopped before the
file was copied back, so they are transcribed here from the bench output). Times in ms, wall = min of N.
The scorer adds about 0.09 s to the VM's native wall (939e5d0: 1.36 s vs 1.28-1.31; candidate 1: 0.97 s vs 0.876).

| variant | data | N | wall min / med | user | sys | rss MB | minflt | exit | links / parse / dedup / person |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 939e5d0 (committed baseline, AVX2) | s3-20m | 5 | 1307 / 1314 | 9760 | 296 | 3078 | 35078 | 15.4 | 45 / 1022 / 38 / 178 |
| 939e5d0 `SE_SIMD=avx512` (run through the candidate-1 tree before the mask fix) | s3-20m | 3 | 958 / 959 | 7076 | 319 | 1304 | 35790 | 5.3 | 40 / 693 / 34 / 172 |
| 939e5d0 + drop/release/16B DRec only (`cand3`) | s3-20m | 3 | 1306 / 1309 | 9847 | 347 | 1302 | 35259 | 5.2 | 41 / 1039 / 34 / 172 |
| `cand3` `SE_THREADS=4` | s3-20m | 3 | 1526 / 1528 | 5769 | 250 | 1212 | 26795 | 5.4 | 53 / 1192 / 41 / 222 |
| + AVX2 mask codegen fix (`x1`) | s3-20m | 3 | 917 / 927 | 6710 | 333 | - | - | - | - / 651 / - / - |
| `x1` `SE_SIMD=avx512` | s3-20m | 3 | 958 / 962 | 7083 | 343 | - | - | - | - / 693 / - / - |
| **candidate 1 = `c4`, GATED (snapshot `out/perf/snap-c4`, lead's commit 2fe17ed): mask fix + NT write combining + bucketed links + 16B DRec + drop/release** | s3-20m | 5 | **876 / 892** | 6425 | 343 | 1305 | 35833 | 5.3 | 41 / 607 / 35 / 175 |
| 939e5d0 | s5-60m | 3 | 3874 / 3909 | 29357 | 913 | 8778 | 76123 | 39.5 | 150 / 3030 / 112 / 524 |
| **candidate 1 = `c4`** | s5-60m | 3 | **2584 / 2590** | 19313 | 920 | 3649 | 80179 | 11.0 | 132 / 1796 / 109 / 512 |
| `c4` + inlined row callbacks + PEXT id packing (`x5`, not gated) | s3-20m | 5 | 884 / 892 (c4 in the same run: 891 / 893) | 6494 | 336 | 1305 | 36116 | 5.6 | - / 614 / - / 176 |
| `x5` + branch-free newline pick + unconditional person hash (`x6`, not gated) | s3-20m | 5 | 863 / 872 (x5 in the same run: 876 / 884) | 6321 | 320 | - | - | - | - / 594 / - / - |

Parse-stage attribution on the VM at 20M (`SE_STAGE`, parse wall ms; clicks are always processed in full):

| stage | 8 threads (`x2`) | 1 thread (`x6`) |
| --- | --- | --- |
| 1: scan rows only | 260 | 951 |
| 2: + evaluate fields | 387 | 1561 |
| 3: + pack event_id, hash | 400-423 | 1666 |
| 5: + prefetched link lookup (no scatter), ring 8 | 513 | 2275 (ring 1: 2933, ring 64: 2283) |
| 4: link lookup without prefetch, no scatter | 634 | 3762 |
| full (direct scatter) | 648 | - |
| full (write combining) | 593 | 2799 (2 threads 1421, 4 threads 719, 8 threads 594) |

Other VM measurements:
- Partition-count sweep (`SE_DBITS`/`SE_PBITS`, 20M, parse user ms): 6/6 4583, 7/7 4571, 8/8 4580, 9/9 (default) 4623, 10/10 4659;
  dedup wall 100 / 67 / 46 / 39 / 32 ms and person pass 244 / 215 / 204 / 182 / 196 ms. Default kept.
- Raw read throughput of the 20M dataset from the page cache: `xargs -P8 wc -l` over all event and click files 84 ms. Memory bandwidth is not the limit.
- THP: `AnonHugePages` 1376 of 1383 MB anonymous during a 60M run; `thp_fault_fallback` 0.
- `perf stat` has no hardware counters in this VM (cycles, instructions, cache and TLB misses "not supported"); `perf record` samples on cpu-clock only.
- Bucketed, branch-free link lookup (`x3`) against `x2`: stage 5 minus stage 3 was 124 vs 113 ms (no gain; kept, it is not slower in the full run within noise).

## Docker Desktop VM on the Mac (linux/arm64, 8 vCPU, 3.8 GB, kernel 6.10, THP=always), scorer flags, s7-5m, 2026-11-20/21

Host was loaded by other agents; use the minimum. `v1`/`v2` = 939e5d0 + knobs. Times in ms.

| variant | N | wall min / med | user min | sys min | rss MB | minflt | exit min | parse wall / parse sys |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 939e5d0 mmap | 15 | 156 / 197 | 970 | 92 | 819 | 28074 | 10.0 | 89 / 34 |
| mmap, `SE_CHUNK=262144` | 15 | 160 / 215 | 975 | 110 | 824 | 27953 | 10.1 | 93 / 47 |
| `SE_READ=1` (pread) 128K chunks | 15 | 167 / 197 | 949 | 218 | 426 | 21923 | 5.2 | 105 / 163 |
| `SE_READ=1` 256K | 15 | 160 / 198 | 896 | 194 | 428 | 22077 | 5.4 | 94 / 131 |
| `SE_READ=1` 512K | 15 | 160 / 198 | 889 | 201 | 431 | 22591 | 4.5 | 98 / 136 |
| `SE_READ=1` 1M | 15 | 167 / 209 | 938 | 215 | 430 | 23352 | 4.8 | 106 / 149 |
| `SE_READ=1` 2M | 11 | 202 / 257 | 956 | 398 | 435 | 22388 | 5.2 | 130 / 293 |
| `SE_POPREAD=1` | 11 | 226 / 337 | 1326 | 188 | 819 | 27886 | 12.2 | 133 / 67 |
| `SE_POPULATE=1` | 11 | 241 / 322 | 1269 | 182 | 814 | 27753 | 11.4 | 134 / 48 |
| `SE_POPULATE_ANON=1` | 11 | 252 / 382 | 1242 | 252 | 931 | 56233 | 15.2 | 136 / 86 |
| `SE_THREADS=4` | 11 | 316 / 380 | 1045 | 132 | 774 | 21346 | 10.8 | 210 / 55 |
| `SE_NOTHP=1` (no huge pages) | 15 | 159 / 189 | 902 | 149 | 778 | 95378 | 14.1 | 91 / 88 |
| `SE_DROP=1` (drop parsed chunks) | 15 | 165 / 203 | 968 | 109 | 421 | 28818 | 4.7 | 102 / 44 |
| `SE_RELEASE=1` (parallel arena release) | 15 | 155 / 179 | 914 | 96 | 818 | 27991 | 8.2 (+1.5 release) | 87 / 35 |
| `SE_DROP=1 SE_RELEASE=1` | 15 | 141 / 177 | 844 | 90 | 424 | 29014 | 3.8 (+1.3) | 79 / 34 |
| `SE_DROP=1 SE_RELEASE=1 SE_NOTHP=1` | 15 | 178 / 212 | 988 | 209 | 383 | 96273 | 3.5 (+6.5) | 105 / 135 |
| same + `SE_POPULATE_ANON=1` | 15 | 179 / 441 | 861 | 144 | 547 | 134482 | 3.8 (+8.5) | 107 / 81 |

Cold page cache (dropped before every run, N=5, wall min): mmap 1075; `SE_WILLNEED=1` 815; `SE_POPREAD=1` 821;
`SE_POPULATE=1` 1259; pread 256K 1098; pread 1M 920. All are bound by the virtual disk (about 0.5 GB/s); only relevant
if the scorer's cache were cold, which its times rule out.

## Mac native (Apple M2, 4P+4E, 8 GB, macOS), s7-5m

Rows for this environment are in the generated table above (`Darwin/arm64 native`). Earlier unrecorded A/B runs
(min user ms of the phase, whole-process instructions):
- assist counters u32 instead of in the 32-byte accumulator: person pass 163.9 -> 161.1, 6.833 -> 6.794 G instr (noise level; kept).
- 16-byte DRec: parse 450.5 -> 469.0 user, 6.831 -> 6.965 G instr, rss 786 -> 744 MB (kept for the footprint).
- write combining on arm64 (plain copy, no NT stores): 6.951 -> 7.398 G instr, cycles 2.62 -> 2.91 G (off by default on arm64).
- `SE_STAGE` instruction split (G instr, whole process, 5M): stage 1 2.88, stage 2 4.24, stage 3 4.63, full 6.83.
- person pass sections (ms summed over threads, 5M): group by person 46-70, scatter 12-24, clicks 20-38, sort purchases 2-7, sort timestamps 51-61.

