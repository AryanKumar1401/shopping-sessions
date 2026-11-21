# Algorithm research memo: Shopping Sessions engine

Owner: algorithm research. Scope: independent analysis for the Rust engine (`engine/`).
Sources: `rules.md` (authoritative), generator source `downloads/generator/generator/src/*.rs`
(read only), and the dev set `data/dev` + `data/dev-answer` (seed 1, `--events 1000000 --clicks 250000`).

Labels used throughout:
- **[M]** measured on the dev set with `awk`/`wc` (exact numbers, 1M events).
- **[G]** read from generator source (exact by construction).
- **[E]** estimate (extrapolation or a cycle-count model). Not measured on the target box; treat
  as a prior for profiling.

---

## 0. Ground truth: data shapes that drive every decision

### 0.1 ID shapes [G], [M]

| Thing | Generator form | Max bytes [M] |
| --- | --- | --- |
| user_id | `u` + base36(scramble40(person*512 + alias)), lowercase; kiosk IDs from numbers >= 2^39 | 9 |
| event_id | `e` + base36(scramble40(event_index)) | 9 |
| click_id | `k` + base36(scramble40(click_index)) | 9 |
| campaign_id | `c_1`..`c_2000`; unknown: `c_2001..c_9999`, `C_n` | 6 |
| product_id | `p1`..`p50000`; unknown: `p50001..p99999`, `P<n>`, empty | 6 |

- `scramble` is a bijection on 40 bits, so base36 output is 1 to 8 chars (36^8 > 2^40). IDs are
  never longer than 9 bytes in generated data, and always 7-bit ASCII.
- Case variants: `uppercase_last()` upper-cases the **whole field**, prefix included (`U1AK82W40`,
  `E...`, `K...`). Rate 5e-6 per id field (rows), 1/2000 per link side. Upper-case sorts before
  lower-case, so an upper-cased link side becomes the person's name (dev answer has 9-char names
  starting with `U`).
- The dev answer's person names are 6 to 9 chars [M].
- No structure links an ID to its person, event, or copy: IDs are scrambled on purpose.

### 0.2 Volumes [M] at 1M events, [E] linear scaling

| Quantity | dev 1M [M] | 20M [E] | 60M [E] |
| --- | --- | --- | --- |
| event lines (incl. dup copies) | 1,072,086 (1.072 per event) | 21.4M | 64.3M |
| event CSV bytes (77 B/line) | 82.5 MB | 1.65 GB | 4.95 GB |
| click lines (47 B/line) | 255,250 / 12.0 MB | 5.1M / 240 MB | 15.3M / 720 MB |
| link lines (21 B/line) | 44,874 / 0.93 MB | 0.90M / 18.5 MB | 2.7M / 56 MB |
| distinct IDs in links | 60,500 | 1.21M | 3.6M |
| distinct user IDs in event rows | 65,564 | ~1.3M | ~3.9M |
| event rows whose user_id is linked | 62.3% | same | same |
| duplicated event_ids (2/3/4 copies) | 12,041 / 11,900 / 12,049 (3.6% of events, +7.2% lines) | 0.72M ids | 2.2M ids |
| duplicated click_ids (2/3 copies) | 1,697 / 1,762 | | |
| surviving events | 977,930 | 19.6M | 58.7M |
| sessions | 179,366 | 3.6M | 10.8M |
| user_daily rows | 117,038 | 2.34M | 7.0M |
| campaign_daily rows | 27,400 (saturated: 2,001 x 14 = 28,014 max) | ~28K | ~28K |
| sum of `assisted_sessions` | 1,932,099 (10.8 per session) | ~39M | ~120M |
| credited non-organic / organic sessions | 109,923 / 69,443 | | |

Total input: ~1.9 GB at 20M, ~5.7 GB at 60M (matches the "2 GB / 6 GB" brief).

### 0.3 Heavy people [G], [M]

- 5 whales with 1-2% of events each: 200-400K events at 20M, 0.6-1.2M at 60M. Whales get
  **all 168 two-hour slots** as sessions (`sessions = SLOTS`), so ~170-177 sessions each [M], with
  1,000-7,000 events per session at 60M and many gaps of 0 ms (equal timestamps are common for
  whales: gaps are rescaled and floored to whole seconds).
- Whale clicks: 0.5-1.5% of all clicks each (25-75K at 20M, 75-225K at 60M).
- Kiosk ("shared device") persons: 0.4% of people across 3-6 kiosks, so ~450-900 people per kiosk
  at 20M and 1,300-2,700 at 60M. A whale can also be in a kiosk (the kiosk draw applies to
  every index). Their sessions interleave and merge.
- Hard bound used below: sessions of one person start > 30 min apart, so a person has at most
  14 x 48 = **672 sessions** in a 14-day window. Campaigns are at most 2,001 with organic.
- Chains: 3 people + 1/20,000 of the rest have 50-400 IDs linked in a random-order path, so a
  component diameter up to ~399.

### 0.4 Timestamps [G]

- Valid `ts`: F1 60% (`write_utc`), F2 10% (31 real offsets -10:00..+12:00, incl. +05:45, +08:45,
  +09:30), F3 30% (plain digits; always used when ms != 0). Rules quote ~58/10/32.
- Bad `ts`: empty, `.mmmZ`, lowercase `z`/`t`, space, no zone, `N.0`, `1.79e12`, `-k`, month 13,
  Feb 30, hour 24, second 60, Feb 29 in a non-leap year, or a *valid* form just outside the window
  (`ws-1`, `ws-1000`, `we`, up to 3 days out).
- `ingest_ts` is always F1, whole seconds; normally `ts` + 0..600 s (ceil to second), 0.5% are
  1-5 s *before* `ts`, late ones +24 h .. +5 d, some exactly +24 h (kept) and +24 h + 1 s (dropped).
- Duplicate copies: `ingest = original_ingest + U[1,600] s`, drawn independently per copy, so two
  later copies can tie with each other (then identical) but never with the original.
- 3% of sessions start at a non-whole-second ms (only F3 then).

---

## 1. Connected components for links

### 1.1 Size of the problem

~0.9M link lines, ~1.2M distinct linked IDs at 20M (2.7M / 3.6M at 60M) [E]. This is small; the
real cost is turning strings into node ids, not the union-find itself.

### 1.2 Inline key packing (use it for every ID in the program)

**Tier 1, exact and order-preserving: 7-bit ASCII packing, up to 9 bytes, into a u64.**

```
key(s) = sum_{i<len} s[i] << (7 * (8 - i))      // s[0] is the most significant 7 bits
valid iff 1 <= len <= 9 and every byte in 0x01..=0x7F; bit 63 = 0
```

- Injective: no byte is 0, so zero padding cannot collide with a real byte; lengths are implied.
- Order-preserving: unsigned compare of keys equals byte-wise compare of strings (a proper
  prefix pads with 0 < every byte, so it sorts first, as byte-wise compare requires).
- Covers 100% of generated IDs (all <= 9 bytes ASCII [M]) and the odd fixture IDs
  (` u1`, `u1 `, `U1`, `u1\r`).
- Implementation: `x = load_u64_le(p) & len_mask(min(len,8))`; `x = x.swap_bytes()`;
  `hi = _pext_u64(x, 0x7F7F_7F7F_7F7F_7F7F)` (56 bits); `key = (hi << 7) | (len==9 ? p[8] : 0)`.
  Check `x & 0x8080..80 == 0` for ASCII and `has_zero_byte` within `len`. ~6 instructions. PEXT is
  3 cycles on Intel (fine on Cascade Lake+); on AMD Zen 1/2 it is microcoded, use a shift/or
  ladder there.
- Key 0 is reserved for the empty string (null).

**Tier 2, exact fallback**: anything else (10+ bytes, non-ASCII, NUL) is interned in a global
sharded table (`[Mutex<HashMap<Box<[u8]>, u32>>; 64]`) and gets `key = 1<<63 | index`. Tier-2 keys
are *not* order-preserving, so every order comparison goes through
`cmp(a,b) = if both tier1 {a.cmp(b)} else {bytes(a).cmp(bytes(b))}`. Tier 2 never fires on
generated data, so its speed is irrelevant, but fixtures must exercise it (add a fixture with a
12-byte ID, a UTF-8 ID and an ID that is a prefix of another).

Order is only needed in two places: the person name (min over a component) and the click_id
tie-break. Everywhere else only equality matters.

### 1.3 Building node ids without a global string hash bottleneck

1. Parse link files in parallel (chunked like events, section 6). Emit `(ka, kb)` u64 pairs for
   well-formed lines with both sides non-empty. Skip `ka == kb` (a self-link changes nothing: the
   ID is a singleton named by itself either way).
2. Radix-partition endpoints by the top 8 bits of `h(key) = (key * 0x9E3779B97F4A7C15)` into 256
   buckets (per-thread buckets, so no atomics). Each item: `(key, pair_slot)`, 12-16 B.
3. Per bucket (one task each, ~4.7K keys at 20M, L1/L2-resident): open-addressing table
   `key -> local id`; global node id = `bucket_base[b] + local`. Write the node id back into the
   pair array (`pairs[pair_slot>>1].side[pair_slot&1] = id`).
4. The 256 bucket tables, laid out back to back, **are** the read-only "linked-ID table" used to
   map event/click user IDs later (section 1.6). Value slots get overwritten with the person key.

Cost [E]: 1.8M endpoints, two cache-friendly passes: ~3-6 ms on 8 threads at 20M.

### 1.4 Union-find choice

| Method | Work | Fit here |
| --- | --- | --- |
| Sequential UF (path halving, union by index or rank) over u32 parents (4.8 MB at 20M, L3) | 0.9M unions, ~2 finds each; ~10-25 ms at 20M, ~40-70 ms at 60M (L3/DRAM latency bound) [E] | Simple, obviously exact. OK at 20M. |
| **Lock-free CAS UF** (Anderson-Woll / "UF-Async" as in ConnectIt/GBBS): hook root `u` under root `v` only if `u > v` via `CAS(parent[u], u, v)`, retry on failure; find with path halving using relaxed atomic stores (always writes an ancestor, so races are benign) | Same work split over 8 threads: ~3-8 ms at 20M, ~10-20 ms at 60M [E] | **Recommended.** Result (the partition into components) is order-independent; easy to cross-check against sequential UF in debug builds. |
| Label propagation (min-label) | O(diameter x edges): chains give diameter ~399, so ~400 rounds x 1.8M = 720M edge visits | **Reject.** |
| Shiloach-Vishkin / pointer jumping | O(log n) rounds x edges, ~20 x 1.8M | Works but more code than CAS UF and slower. |

### 1.5 Picking the byte-wise smallest name cheaply

After all unions: parallel pass `root[i] = find(i)`, then `min_key[root[i]].fetch_min(key[i])`
(AtomicU64; init `u64::MAX`). With tier-1 keys `fetch_min` is exactly byte-wise min. If any tier-2
key exists among linked IDs (global flag), redo this step sequentially with the byte comparator.
Then `person_key[i] = min_key[root[i]]`, stored as the value in the linked-ID table.

This includes link-only IDs (never in an event/click) as the rules require: they are nodes.

Do **not** use "union by smaller key" as the only rule without path compression: chains of 400
would give deep trees. The separate min pass is cheap (one atomic per node, 1.2M) and decouples
naming from the tree shape.

### 1.6 Mapping event/click user IDs to persons

- **The person is represented by its name key (u64), not a dense person id.** Lookup:
  `person = linked_table.get(user_key).unwrap_or(user_key)`. IDs that appear in no link are
  their own person and never need interning anywhere (38% of event rows [M]). No global intern of
  ~1.3-3.9M user IDs, no dense-id assignment, no string storage.
- Table layout: 64-byte buckets, 4 x (u64 key, u64 person_key), bucket = `h(key) >> shift`, linear
  probe on bucket overflow (rare at load <= 0.8). One cache line per lookup, hit or miss.
  Size: 1.21M / (4 x 0.8) x 64 B = 24 MB at 20M; 72 MB at 60M. Allocate with
  `madvise(MADV_HUGEPAGE)` so the lookup is not also a TLB miss (24 MB = 6K 4K-pages vs 12 2M-pages).
- Lookups are latency-bound (~80-100 ns DRAM). Hide them with a software-prefetch pipeline: when
  row `i` is tokenized, compute `h(user_key)` and `_mm_prefetch` the bucket; resolve row `i-16`.
  With ~10 outstanding misses per physical core: ~10 ns/lookup per core, 21.4M lookups / 4 cores
  ~ 50 ms of latency at 20M, mostly overlapped with parsing [E].
- Alternative if profiling shows the lookup stalling the parse: radix-partition by
  `h(user_key)` first (bucket-local table slices are L2-resident), then a second scatter by
  `h(person_key)`. Costs one extra 16 B/row scatter (~20-30 ms at 20M [E]); only worth it at 60M
  if the prefetch pipeline does not hide the misses.
- Perfect hashing (PtrHash/BBHash) over the 1.2M linked keys: still needs a stored key to reject
  unlinked IDs, so it is still one cache line per lookup; build costs ~20-60 ms. Not worth it.

### 1.7 What must never be assumed about ID formats

- Never assume prefixes (`u`/`e`/`k`/`c_`/`p`), lengths, alphabet, case, or that base36 decoding
  works (upper-case variants and fixtures break it). Packing is a representation, not a parse.
- Quotes are syntax: `"u1"` equals `u1`; `""` is empty = null. Spaces and `\r` are part of the ID.
- Case-sensitive, untrimmed; `U1` and `u1` are different people unless linked.
- An ID can occur only in links and still be the name; event/click IDs can be absent from links.
- `campaigns.csv` and `products.csv` IDs are looked up by exact bytes (hash on the packed key);
  "IDs like c_123" is not a guarantee. A dense fast path is fine only as an exact cache
  (e.g. `p<n>` with no leading zero and n < 2^20 -> `dense[n]`, built from the file; everything
  else, including `p0123` and `P12`, goes through the hash).
- The products file may use any number of categories; `unknown` is inserted into the sorted
  category-name list so that category ids sort in byte-wise name order (top_category tie-break).

---

## 2. Event dedup

### 2.1 Structure of duplicates [G], [M]

- 2-4% of event_ids have 1-3 extra copies (dev: 3.6%, uniform over 2/3/4 copies, +7.2% lines).
- Copies are shuffled into any file. Later copies: ingest +1..600 s each (independent draws), so
  the original always has the strictly smallest ingest; later copies may tie among themselves
  (identical rows).
- 2% of duplicated events have a bad `ts` only in the original: the event must be dropped even
  though later copies are valid (fixture E5 / X09).
- ~1/64 of duplicated events: all later copies carry a *different person's* user_id (next person
  index, first alias). Only if the original survived CSV corruption.
- If the original is malformed (dropped in parse), the earliest remaining copy is kept.
- Malformed fault 4 produces an empty event_id; those lines are dropped in step 1 and do not
  participate (dev shows one "event_id" with 171 occurrences: that is the empty key from
  malformed lines; exclude key 0 from dedup).

### 2.2 Validation can be computed per copy at parse time

Steps 3 (validate) and 4 (late) are pure functions of the kept copy's own fields. So at parse,
compute `survives = valid && !late` per line. After dedup: event survives iff its kept copy has
`survives`. Consequences:
- Every well-formed line with non-empty event_id emits a **dedup tuple** (needed even if the line
  is invalid: an invalid earliest copy must kill later valid copies).
- Only `survives` lines emit a **person record**. Invalid lines never enter the person phase.

### 2.3 Strategies and costs

Dedup tuple: `key u64 | ingest_rel_s i32 | line u32` = 16 B. `ingest_rel_s` = F1 seconds minus
`window_start` seconds; saturating cast with a global "escape" flag if any value leaves i32
(+-68 years). On escape, rerun dedup with i64 ingest (exact; never happens on generated data).
`line` = global line ordinal (chunk base + local index), < 2^32 even at 60M.

| Strategy | Work at 20M (21.4M tuples) [E] | Notes |
| --- | --- | --- |
| **Radix partition + L2 open addressing** (recommended) | One scatter of 343 MB (done inside the parse loop), then per partition: probe/insert into a 2^15-slot x 16 B = 512 KB table. ~5-8 ns/tuple in L2: ~15-20 ms on 8 threads | P = next_pow2(lines / 20K): 1024 at 20M, 4096 at 60M. Partition bits = top bits of `h(key)`, slot bits = next bits. |
| Global concurrent hash table | 21.4M random DRAM accesses into ~512 MB, CAS contention-free mostly; ~100 ns each / MLP; 40-80 ms | Simpler, slower, and needs atomics on 16 B entries. |
| Sort-based (LSD radix on 54-63 bit keys) | 7-8 passes x 343 MB read+write: ~5 GB of traffic, 150+ ms | Reject. |
| Swiss table / cuckoo per partition | Similar to open addressing once partitions are L2-sized; SIMD tag compare helps only at high load | Open addressing at load ~0.65 with a good multiplicative hash is enough. |
| Exploit event_id structure | Keys are scrambled indices: no locality, no ordering, copies in random files | The only useful "structure" is that keys pack into one u64 (tier 1). There is no exact shortcut that avoids touching every key once. |

Per-partition rule (exact): on collision with stored `(ing0, line0)` vs incoming `(ing1, line1)`:
if `(ing1, line1) < (ing0, line0)` mark `line0` loser and store incoming, else mark `line1`
loser. Equal ingest copies are identical by the rules, so keeping the lower line ordinal is just a
deterministic choice. Losers go to a global bitmap (1 bit per line: 2.7 MB at 20M, 8 MB at 60M)
with `fetch_or` on u64 words (or per-partition loser lists then a merge). Only ~1.5M (20M) bits
get set.

Write-combining detail: 1024 partitions x 8 threads. Each thread keeps 1024 cursors into 64 KB
blocks from a per-thread THP arena; 1024 live cache lines = 64 KB fits L2. Above ~2048 partitions,
use 64 B software write-combining buffers + non-temporal stores, or two-level partitioning.

### 2.4 Can dedup be fused with partition-by-person?

**Not exactly, without a global pass over all event keys.** Proof sketch: a later copy can name a
different person (1/64 of duplicated events, ~0.06% of events), and an invalid copy may have an
empty or unknown user. The only evidence tying the copies together is the event_id, and either copy
could be first. Any exact scheme must therefore co-locate all copies by event_id at some point.
Partition by person + local dedup is exact for same-person copies but silently double-counts the
cross-person ones (and lets a valid later copy "rescue" an event whose invalid earliest copy sits
in another partition).

What *can* be fused: the full payload never has to move for dedup. Recommended layout:
- Parse loop emits the 16 B dedup tuple into `dedup_part[h(event_key)]` **and**, for survivors,
  the person record into `person_part[h(person_key)]` (prefetched person lookup, section 1.6).
- Dedup phase writes the loser bitmap.
- Person phase reads records and drops `loser[line]`. Bitmap reads are random but the bitmap is
  L3-resident (2.7-8 MB); group the check per 64-record batch with prefetch. Only ~7% of lines are
  losers.

Versus the planned baseline (scatter full 32 B records by event hash, dedup, scatter survivors by
person): baseline moves ~32 + 24 = 56 B/line; this moves 16 + 20 = 36 B/line, and drops invalid
lines from the second structure. Expected saving ~15-25 ms at 20M, ~50-70 ms at 60M [E]. Either is
exact.

Variant (no bitmap): parse into per-thread sequential record buffers (no person scatter), dedup,
then each thread scans its own buffer with the bitmap *sequentially*, does the person lookup there,
and scatters survivors. Costs one extra sequential read of the records but moves the person lookup
out of the parse loop. Choose by profile.

### 2.5 Click dedup: skip it

Rules: copies of a click have identical values. Credit (latest ts, then smallest click_id) and
assists (set of campaigns with >= 1 qualifying click) are both invariant under duplicating a click:
the argmax returns the same click_id/campaign, and set membership is unchanged. Validation is a
function of the values, so all copies validate identically. Hence **no click dedup is needed for
exact output.** Saves a hash pass over 5.1M / 15.3M clicks (~5-15 ms [E]). Risk: none under the
rules ("keep any one" means output cannot depend on which copy).

---

## 3. Sessionization

### 3.1 Person record

```
person_key: u64            // tier-1/2 key of the person's name
t:          u32            // ts - (window_start - 7 d) in ms; events lie in [7d, 21d) of this base
payload:    u32            // bits 0..23 price (0 = not a valid purchase, else 1..10M < 2^24),
                           // bits 24..31 category id (byte-wise name order; fallback if > 255 cats)
line:       u32            // for the loser bitmap
```
20 B (SoA within partition blocks). Event type is not needed beyond "purchase with price" since
`events` counts all types: store price only for valid purchases. One common time base
`B = window_start - 7 d` for events and clicks makes all comparisons direct u32 compares. Check at
startup that `window_end - B < 2^32` ms (49.7 days); otherwise use the i64 generic path (fixtures
with odd windows).

Day index: `day(t) = floor((B + t) / 86_400_000) - floor(window_start / 86_400_000)`. Do not assume
`window_start` is midnight (it is in the generator, but the rules do not promise it): number of days
= days spanned, 14 or 15.

### 3.2 Sorting strategy

Person partitions: P = 1024 at 20M (~20K records, 400 KB, L2-resident), 4096 at 60M, keyed by
`h(person_key)` with bits disjoint from the dedup hash. One task per partition; schedule largest
first (LPT) because whales and kiosk giants make a few partitions 20-60x the mean.

Per partition:
1. Tiny open-addressing table `person_key -> local idx` (~650 persons per partition at 20M).
2. Counting sort by local idx (histogram + scatter of `u64 v = (t << 32) | payload`), dropping
   losers on the way.
3. Per person, sort `v` (u64 compare == sort by t, payload is a harmless tiebreak):
   - n <= 32: insertion sort / sorting network (median person has ~30 events but ~5 sessions).
   - 32 < n <= 4096: `sort_unstable` (pdqsort) on u64.
   - n > 4096 (whales, kiosk persons): LSD radix on the 31 significant bits of `t`, 3 passes of
     11 bits (payload rides along in the low 32 bits). 1.2M whale records at 60M: ~6-8 ms single
     thread [E], far below the ~100 ms phase time, so no intra-person parallelism is needed as long
     as LPT starts whales first.

Why not a global (person, ts) radix sort: 21M x 16 B x 6-8 passes is ~2-3 GB of traffic, all
DRAM-bound; the partitioned approach makes every pass L2-resident after one scatter. Why not MSD on
person directly: the local-idx counting sort *is* the MSD step, with tiny alphabets.

Cost [E]: ~25-40 ms at 20M, ~80-120 ms at 60M on 8 threads.

### 3.3 Equal timestamps, gaps, day

Walk sorted `t`: `if t - prev_t > 1_800_000 { new session }; prev_t = t`. Equal timestamps give
gap 0, so they join the session the first of them joins, exactly as the rules require. Order among
equal-ts events is irrelevant because every per-session aggregate (count, revenue sum, category
sums) is commutative. Exactly 30:00.000 stays, 30:00.001 splits (0.1% of gaps are these [G]).
Session day = `day(start_t)`; every event of the session counts on that day, even after midnight.

### 3.4 Whales and the shared-device giant

- Whale: ~170 sessions, 1-7K events each. Only the sort size matters (above).
- Kiosk person: thousands of people interleaved, so most gaps are < 30 min and sessions merge into
  long ones (possibly spanning days; a session counts entirely on its start day). Same code path.
- Hash partitioning puts each giant in one partition: LPT scheduling is the only defence needed.
  Expected imbalance: largest task ~1.2-1.5M records at 60M vs ~15K mean.

### 3.5 Aggregation within the person walk

Per session: `events`, `revenue`, `purchasing` flag, start. Per (person, day): `sessions`, `events`,
`revenue`, and category sums in a small array indexed by category id, touched only on purchases
(3% of events) and reset via a touched-list. `top_category` = argmax of sums, ties to the smallest
id (ids are in byte-wise name order), null if no purchase. Revenue per person-day can exceed 2^31
for a whale at 60M (1.2M/14 x 3% x 50K cents ~ 4e9): use u64/INT64.

---

## 4. Temporal attribution (credit)

Click record in the same person partitioning (`h(person_key)`, same P): `person_key u64,
click_key u64, t u32, campaign u16` (24 B). Only **valid** clicks are emitted (non-empty user,
valid ts in `[ws - 7 d, we)`, known campaign); invalid clicks are dropped at parse.

Per person with sessions (persons with clicks but no sessions are skipped):
1. Sort the person's clicks by `(t, click_key)`. Clicks per person are small except whales (25-225K)
   and kiosk persons; use `u64 (t << 32 | local_click_idx)` sort, and resolve ties inside equal-`t`
   runs by scanning for the minimum click_key (exact, `cmp` from 1.2 if tier 2 exists). Tier-1 keys
   compare as integers = byte order, so most of the time this is one integer compare.
2. Linear merge with session starts `s_0 < s_1 < ...` (strictly increasing, > 30 min apart):
   ```
   j = 0; best = NONE
   for each session s:
       while j < n && c[j].t <= s:
           if best == NONE || c[j].t != c[best].t: best = j   // first click of a new, later ts group
           j += 1                                              // (first of group = smallest key)
       credit = (best != NONE && s - c[best].t <= 604_800_000) ? c[best].campaign : ORGANIC
   ```
   With clicks sorted by `(t, key)` ascending, the first element of the last ts-group with
   `t <= s` is exactly "latest ts, then smallest click_id". The 7-day check is inclusive (exactly
   7 days counts, 7 d + 1 ms does not); `t <= s` includes the click exactly at the start and excludes
   +1 ms. Duplicate click copies are identical, so they never change `best`'s campaign.
3. Cost O(S + C) per person after the sort.

Exactness checks to put in fixtures: same-ms pair with different campaigns and keys where the
smaller key is the later-read one; tie where one key is a prefix of the other (`k1` vs `k10`);
upper-case key (`K...` sorts before `k...`).

---

## 5. Assists

Definition: campaign c assists session s iff the person has a valid click of c with
`s - 7 d <= t <= s`. Organic never assists. Count per (c, day(s)), once per session.

### 5.1 Method A: sliding window active set (recommended default)

Same sweep as credit. Keep `cnt[c]` (u32 per campaign, per thread, reused), an active list
(dense array of campaigns with cnt > 0, plus `pos[c]` for O(1) swap-remove), and two pointers
`add` and `rem` into the person's ts-sorted clicks:
```
for each session s (increasing):
    while add < n && c[add].t <= s:            { if cnt[c]++ == 0 { active.push(c) }; add += 1 }
    while rem < add && c[rem].t < s - 7d:     { if --cnt[c] == 0 { active.remove(c) }; rem += 1 }
    for c in active: assisted[c][day(s)] += 1
```
Exactness: the window `[s - 7d, s]` has both endpoints non-decreasing as s increases, so each
click enters once and leaves once, and after the two loops `cnt[c]` = number of c's clicks in the
window. `c` is active iff count >= 1, so each qualifying campaign is counted exactly once per session,
however many clicks qualify (duplicates only raise `cnt`). Cleanup: after a person, pop the active
list and zero `cnt` (no full reset).

Cost: O(C + sum over sessions of |active(s)|). The second term equals the total output increments,
`sum(assisted_sessions)`: 1.93M at 1M events [M], so ~39M at 20M and ~120M at 60M [E]. Each
increment is one L1/L2 hit into a per-thread `[campaign][day]` u32 array (2,001 x 15 x 4 B = 120 KB),
~1-2 ns: ~10 ms at 20M, ~30 ms at 60M on 8 threads [E]. Per-person worst case is bounded:
672 sessions x 2,001 campaigns = 1.35M increments (~2 ms), so no person can blow it up.

### 5.2 Method B: per-(person, campaign) interval union + counting session starts per day

For each campaign c of the person, take its clicks sorted by t; each click covers starts in the
closed interval `[t, t + 7d]`. Merge overlapping intervals (`next.t <= cur_end` -> extend). For each
merged interval `[a, b]`: `lo = lower_bound(starts, a)`, `hi = upper_bound(starts, b)`; then split
`[lo, hi)` by day using the person's per-day first-session index (`day_first[d]`, <= 15 entries) and
add `count` per day to `assisted[c][d]`.

Exactness: s is assisted by c iff `s` lies in the union of c's closed intervals. Merged intervals are
disjoint, so a session is counted at most once per campaign; every session in the union is in exactly
one merged interval, so it is counted exactly once. Touching but non-overlapping integer intervals
may be left unmerged: they are disjoint, so no double count.

Cost: O(C log C + M (log S + D)) where M = merged intervals (<= distinct-click runs) and D <= 8 days
spanned by a 7-day interval. Independent of |active| x S. Needs clicks grouped by (person, campaign):
a sort by `(campaign, t)` per person.

### 5.3 Comparison

| | A: sliding active set | B: interval union |
| --- | --- | --- |
| Work | C + output increments (~2 per session-campaign pair, unit increments) | C log C + M log S + M x D (weighted increments) |
| Heavy persons | <= 672 x 2,001 per person; whale at 60M ~ 170 x ~1,500 = 0.25M | ~ C log C: whale at 60M ~ 200K x 17 = 3.4M ops |
| Ordinary persons (~6 sessions, ~11 clicks, ~7 active) | ~40 increments | ~8 intervals x (bsearch + 1-2 day adds) |
| Shares the credit sweep | yes (same two pointers, same ts sort) | no (needs a second sort by campaign) |
| Exactness argument | monotone window, counter > 0 | disjoint closed-interval union |

Conclusion: because the session bound (672) caps |sessions| per person, the feared
O(sessions x active campaigns) is bounded by the actual output size and is cheap. Use A everywhere;
B is a safe drop-in for persons with `S x distinct_campaigns > 64 x C` if profiling ever shows a hot
spot (it will not on generator data [E]). Both are exact; keep both in the test harness as
cross-checks (they compute the same numbers by different arguments).

### 5.4 Credited counters

Per thread: `credited[c][day] = {sessions u32, purchasing u32, revenue u64}` plus `assisted u32`,
2,002 x 15 x 24 B = 720 KB per thread. Merge 8 of them at the end (~0.1 ms). campaign_daily rows =
non-zero cells (~28K).

---

## 6. Parsing and I/O

### 6.1 The dialect makes CSV easy

No value ever contains `,`, `"` or `\n`. So every comma is a separator and every newline ends a
line, **quoted or not**. There is no quote state to carry: the simdjson prefix-XOR quote masking is
unnecessary. Quotes only need validating per field.

Per field `[s, e)` (between separators), with `q` = quote bits in the field:
- `q == 0`: plain value `[s, e)`.
- `e - s >= 2 && q == bit(s) | bit(e - 1)`: quoted value `[s + 1, e - 1)` (`""` -> empty -> null).
- otherwise: malformed line.

Cheap per-line form: `popcount(quote bits in line) == 2 x #(fields with byte[s] == '"')` and every
such field ends with `"` and has length >= 2. Each quoted field supplies exactly its two boundary
quotes, so equality proves there are no others. Field count = commas + 1 must equal 7 / 4 / 2. An
empty line has one field, so it is malformed automatically. The header line is the first line of
each file: skip it. EOF without a trailing `\n` ends the last line.

### 6.2 Tokenizer: bitmask stage 1 + per-row stage 2

- Stage 1 (per 64 B): three `vpcmpeqb` (`\n`, `,`, `"`) -> three u64 masks. AVX2: two 32 B halves +
  `vpmovmskb`; AVX-512BW: one `vpcmpb` -> k-mask each. ~0.1 cycle/byte.
- Stage 2: walk separators with `tzcnt`/`blsr` on `comma | newline`; for each line, slice its 6/3/1
  comma positions, validate quotes as above, then call fixed-schema field decoders. Either decode
  directly while walking (best: no index buffer) or flatten positions into a small u32 index buffer
  per 64 KB block (simdjson style, L1-resident) and run the decoders from it.
- memchr per field: ~7 calls per row with call/setup overhead and branch mispredicts on short fields
  (~9 B average). Expect 2-3x slower than the bitmask walk on this data [E]. Use only for the
  scalar tail.
- AVX-512 vs AVX2: on Cascade Lake, 512-bit ops push the core to a lower frequency license; for
  compare-and-movemask work AVX2 is usually equal. The Dockerfile builds with
  `target-cpu=x86-64-v3` (AVX2+BMI2, **no AVX-512**), so AVX-512 needs `#[target_feature]` +
  `is_x86_feature_detected!` dispatch. Recommend AVX2 baseline, AVX-512BW variant only if measured
  faster.
- Safe overread: decoders load 16-32 B blindly. Inside a chunk that is fine; within 64 B of the end of
  a file, copy the tail into a zero-padded stack buffer (mmap past EOF can SIGBUS on the last page).

### 6.3 Field decoders (per event row, budget ~100-150 cycles)

- **IDs**: tier-1 packing (1.2), ~6 instructions + length check.
- **event_type**: compare by length: 4 -> `u32 == "view"` or `"cart"`; 8 -> `u64 == "purchase"`;
  anything else invalid (`Purchase`, `cart `, `views`, empty).
- **product**: exact dense fast path or packed-key hash (50K entries, ~1 MB, L2/L3), returns category
  id or `unknown`; empty -> invalid.
- **price** (purchase only): 1..=20 digits after optional leading zeros; any non-digit -> invalid;
  value 1..=10,000,000. SWAR 8-digit check+convert (below).
- **ts** dispatch on length and byte 4:
  - len 20 and `p[4]=='-'`: F1 template `DDDD-DD-DDTDD:DD:DDZ`.
  - len 25 and `p[4]=='-'`: F2 template `DDDD-DD-DDTDD:DD:DD[+-]DD:DD`.
  - all bytes digits (any length >= 1): F3.
  - everything else bad.
- **Template check (F1/F2)**: one 32 B load; `d = v - '0'`; digit lanes must satisfy `d <= 9`
  (`vpminub(d, 9) == d`), separator lanes must equal the template bytes (`vpcmpeqb` against a constant,
  masked by a lane mask); `T` and `Z` are uppercase only, `z`/`t`/space fail automatically. Then
  `vpmaddubsw(d, [10,1,...])` gives two-digit pairs (YY, YY, MM, DD, hh, mm, ss), `vpmaddwd`
  `[100,1]` gives the year. ~15 instructions.
- **SWAR alternative** (scalar, also fine): for 8 ASCII bytes `x`, all digits iff
  `(x & 0xF0F0..F0) == 0x3030..30 && ((x + 0x0606..06) & 0xF0F0..F0) == 0x3030..30`; value via the
  3-multiply trick (`x - 0x30..30`, then `*10+` pairs, `*100+`, `*10000+`).
- **Calendar validation**: month 1..12, hour <= 23, minute <= 59, second <= 59,
  `day <= DIM[leap(y)][month]` with `leap = y%4==0 && (y%100!=0 || y%400==0)` (fixtures X01-X04 cover
  2028, 2027, 2100, 2000). Offset hours 00..23, minutes 00..59, sign `+`/`-`; `-00:00` = UTC.
- **Days**: table `month_start_day[(y - y0) * 12 + m]` for years `window_year - 2 ..= window_year + 2`
  (covers every in-window value even with a +-14 h offset); outside the table use Hinnant
  `days_from_civil` (exact, same as generator `text.rs`), which then fails the window check.
  `ms = (days * 86400 + h*3600 + mi*60 + s) * 1000 - offset_ms`.
- **F3**: skip leading zeros; more than 19 remaining digits or `i64` overflow -> outside the window
  (rules: not malformed, just out of window). 13 digits typical: SWAR 8 + 5.
- **Window**: event `ws <= ms < we`; click `ws - 604_800_000 <= ms < we`.
- **ingest_ts**: rules guarantee valid F1 (generator: `write_utc`), so a fast unvalidated F1 decode
  is acceptable; still keep the template check (it is nearly free) and treat a failure as malformed
  defensively. `late = ingest_ms - ts_ms > 86_400_000`; ingest before ts is kept.

The timestamp parser is the single highest-risk exactness component: fuzz it (random byte strings
around valid forms, every bad form listed in `text.rs::bad_timestamp`) against the Python oracle.

### 6.4 Chunking and scheduling

- One work queue of byte ranges over all files: ~4-8 MB chunks (1.65 GB / 6 MB ~ 280 event chunks).
  Largest file holds ~1/5 of rows [G] (dev: 20.8 MB of 82.5 MB [M]), so per-file splitting is
  required for balance. Chunk owns lines starting in `(start, end]` after the first `\n` at or after
  `start` (chunk 0 of a file starts after the header line).
- Order: links (all threads, ~2 ms) -> CC (~5 ms) -> events and clicks in one queue, biggest chunks
  first. Clicks need the person map too, so they also wait for CC. Optional overlap: parse event
  chunks without the person lookup into sequential buffers while CC runs (section 2.4 variant).
- Threads: 8 vCPU is almost certainly 4 physical cores x 2 HT, single NUMA node. Use 8 worker threads
  (HT hides DRAM latency for the lookup/prefetch-heavy loop; expect +15-30% over 4 threads [E]). Honour
  `$NCPU`; Rust's `available_parallelism` already respects cgroup quotas. Spawn threads once at start
  (scoped threads or a fixed pool); do not use rayon's lazy global pool init in the hot path.

### 6.5 mmap vs read, page cache

| | warm page cache | cold |
| --- | --- | --- |
| mmap whole files | 1.9 GB = 470K 4 KB PTEs; fault-around (64 KB) ~30K faults x 1-2 us = 30-60 ms CPU spread over threads, plus exit-time unmap of all PTEs (single-threaded, ~10-30 ms) [E] | readahead per fault; MADV_SEQUENTIAL widens it |
| mmap per chunk + MAP_POPULATE, munmap after parsing (by the parsing thread) | PTE setup and teardown are both parallel and overlapped with parsing; nothing left for exit | same |
| pread into per-thread reused 1-2 MB buffers (L2/L3) | one memcpy from page cache (~10+ GB/s per core) but no PTE work and the parse then hits cache; no exit cost | each pread blocks; use 8 threads x different files for queue depth |

Recommendation: implement chunk-mmap+MAP_POPULATE and pread-into-reused-buffer behind a flag and
measure on the target; my prior is that they are within ~10-20 ms of each other at 20M and both beat
whole-file mmap. At startup, `posix_fadvise(WILLNEED)` on every input file is cheap insurance for a
cold cache (async readahead with deep queue); measure that it is not costly when warm.

Floor: reading 1.9 GB from DRAM at ~20-25 GB/s (plausible for an 8-vCPU slice; not measured) is
~80-95 ms; 5.7 GB is ~230-290 ms. Cold-cache disk reads would dominate everything (2 GB at 1 GB/s =
2 s), and the 0.75 s leaderboard top implies the scorer runs warm.

### 6.6 Memory, allocator, huge pages, exit

- Every fresh anonymous byte costs a kernel zero + fault: 1 GB of 4 KB pages = 262K faults, ~100-250 ms
  of CPU [E]. Minimise touched scratch (compact records, section 2.4) and reuse arenas across phases
  (dedup-tuple arena becomes the session/output arena).
- Allocate big arenas with `mmap` + `madvise(MADV_HUGEPAGE)` (works when host THP mode is `madvise`
  or `always`; cannot be changed from inside the container, do not try). 2 MB pages cut faults 512x
  and TLB misses for the random lookups. `MAP_HUGETLB` needs reserved hugetlbfs pages: do not rely on it.
- Pre-fault each thread's arena from that thread (parallel zeroing) early, overlapped with link parsing.
- Allocator: almost irrelevant with no per-row allocation. Static glibc (current Dockerfile:
  `x86_64-unknown-linux-gnu` + `crt-static`) is fine; avoid musl's malloc and memcpy. mimalloc optional.
- Exit: write and close outputs, then `libc::_exit(0)`. No destructors, no freeing. Kernel teardown of a
  THP-backed address space is fast; of 4 KB-backed GBs it is tens of ms, and it counts toward wall time
  (the container ends after `exit_mmap`).
- Container: `FROM scratch` + static binary (already in `Dockerfile`) minimises image pull/unpack and
  exec; no dynamic loader. Remaining container start/stop overhead is runtime-side and outside our
  control; it is likely a large share of the 0.75 s leaderboard number [E].

---

## 7. Output

### 7.1 Sizes [E] (scaled from [M] row counts)

| Table | 20M rows | 60M rows | Plain bytes/row | Size 20M / 60M |
| --- | --- | --- | --- | --- |
| user_daily | 2.34M | 7.0M | user_id 4+9, day 4, sessions 4, events 4, revenue 8, top_category ~1-3 | ~80 MB / ~240 MB |
| campaign_daily | ~28K | ~28K | ~40 | ~1 MB |

Writing to the page cache costs about memcpy speed: ~10-20 ms single-threaded at 20M, ~3 ms split over
8 files. Integer widths: INT32 for `sessions`, `events`, `purchasing_sessions`, `assisted_sessions`;
INT64 for both `revenue_cents` (can exceed 2^31 at 60M).

### 7.2 Minimal valid Parquet layout

```
"PAR1"
for each column: [PageHeader][page data] (one or more data pages v1, UNCOMPRESSED, PLAIN)
FileMetaData (Thrift compact)
u32 LE footer length
"PAR1"
```
- Schema: root `SchemaElement{name:"schema", num_children:6}` then leaves:
  `user_id` BYTE_ARRAY REQUIRED converted_type UTF8 (0) + logicalType STRING;
  `day` INT32 REQUIRED converted_type DATE (6) + logicalType DATE; `sessions`/`events` INT32 REQUIRED;
  `revenue_cents` INT64 REQUIRED; `top_category` BYTE_ARRAY **OPTIONAL** UTF8.
- Thrift field ids (verify against `parquet.thrift` while coding):
  FileMetaData{1 version=1, 2 schema, 3 num_rows, 4 row_groups}; SchemaElement{1 type, 3 repetition_type
  (0 REQ, 1 OPT), 4 name, 5 num_children, 6 converted_type, 10 logicalType{1 STRING{} , 6 DATE{}}};
  RowGroup{1 columns, 2 total_byte_size, 3 num_rows}; ColumnChunk{2 file_offset, 3 meta_data};
  ColumnMetaData{1 type, 2 encodings [PLAIN=0, RLE=3], 3 path_in_schema, 4 codec=0, 5 num_values,
  6 total_uncompressed_size, 7 total_compressed_size, 9 data_page_offset};
  PageHeader{1 type DATA_PAGE=0, 2 uncompressed_page_size, 3 compressed_page_size,
  5 DataPageHeader{1 num_values, 2 encoding=0, 3 def_level_encoding=3, 4 rep_level_encoding=3}}.
  Types: INT32=1, INT64=2, BYTE_ARRAY=6.
- PLAIN BYTE_ARRAY = u32 LE length + bytes per value. REQUIRED columns have no level data in a v1 page.
  OPTIONAL `top_category`: page starts with `u32 LE byte length` + RLE/bit-packed hybrid definition
  levels (bit width 1), then PLAIN values for non-null rows only; `num_values` counts nulls too.
  Simplest exact level encoding: RLE runs `varint(run_len << 1)` + 1 byte value (0 or 1).
- DATE = INT32 days since 1970-01-01.
- Page size: keep pages <= ~1-8 MB (some readers dislike huge pages); multiple pages per column chunk
  are fine, `data_page_offset` points at the first.
- One file per worker thread (e.g. `user_daily/part-<t>.parquet`), each one row group: no merging,
  fully parallel. `campaign_daily`: one file from the merged counters.
- Optional size cuts (not speed-critical): dictionary-encode `top_category` and `day`. Statistics are
  optional; omit.
- Validate the writer with pyarrow and DuckDB (`scripts/compare.py`) including an all-null and a
  no-null `top_category` file, and an empty table (0 rows: still write a valid file or none? The rules
  require one or more files per folder; write a valid 0-row file).

---

## 8. End-to-end budget and ranking

### 8.1 Time budget [E] (8 threads, warm cache, cycle model ~3 GHz, 4 cores + HT)

| Phase | 20M | 60M | Notes |
| --- | --- | --- | --- |
| exec, args, manifest, products (50K), campaigns, dir listing | 2-3 ms | 2-3 ms | |
| arena pre-fault (THP), overlapped | ~0 | ~0 | |
| links parse + node ids + CAS UF + names | 6-10 ms | 15-30 ms | |
| **events parse** (21.4M / 64M lines, ~120 cycles/line) + tuple + record emission | 160-200 ms | 480-600 ms | dominant |
| clicks parse (5.1M / 15.3M lines) | 20-30 ms | 60-90 ms | |
| dedup per partition | 15-20 ms | 50-70 ms | |
| person phase: loser filter, local group, sorts, sessionize, aggregates | 25-40 ms | 80-120 ms | |
| credit + assists | 10-15 ms | 30-45 ms | |
| Parquet write | 5-10 ms | 15-30 ms | |
| exit teardown | 5-15 ms | 15-40 ms | THP helps |
| **Total engine** | **~250-350 ms** | **~0.75-1.05 s** | plus container start/stop |

The 3-minute cap at 60M is not a concern; memory is not a concern (below). Parsing is 60-70% of the
engine time at both scales.

Theoretical floor: max(DRAM read of the input, tokenize+decode at ~0.6 cycle/byte) ~ 80-100 ms at 20M
and ~250-300 ms at 60M. A realistic tuned target is 2.5-3x the floor.

### 8.2 Memory [E]

| Structure | B/unit | 20M | 60M |
| --- | --- | --- | --- |
| input (page cache, not anon) | 77 B/event line, 47 B/click line | 1.9 GB | 5.7 GB |
| dedup tuples | 16 B / event line | 343 MB | 1.03 GB |
| person records | 20 B / surviving line (incl. valid dup copies, ~0.98 x lines) | 420 MB | 1.26 GB |
| loser bitmap | 1 bit / line | 2.7 MB | 8 MB |
| click records | 24 B / valid click line | 120 MB | 360 MB |
| linked-ID table + link pairs + parents | ~40 B / linked ID | 50 MB | 150 MB |
| per-partition sort scratch (per thread, L2) | | <10 MB | <10 MB |
| user_daily rows + Parquet buffers | ~70 B / row | 165 MB | 490 MB |
| **anon total** | **~53 B / event** | **~1.1 GB** | **~3.3 GB** |

The planned baseline (32 B record scattered by event hash, then 24 B by person) is ~65-70 B/event
(~1.4 GB / ~4.2 GB). Both fit 24 GB easily; the smaller one is faster because of fault/zero/traffic
costs, not because of the limit.

### 8.3 Top 10 optimizations, ranked by expected gain [E]

Gains are versus a straightforward Rust implementation (memchr splitting, chrono-style timestamp
parsing, `HashMap<String,_>` for IDs, full-record shuffles), at 20M.

| # | Optimization | Expected gain | Exactness risk |
| --- | --- | --- | --- |
| 1 | Bitmask tokenizer exploiting "no `,` `"` `\n` inside values" (no quote state; quote validation by per-line popcount; fixed field counts) | 150-300 ms | Low: quote rules are local; covered by fixtures X08 and malformed-line cases |
| 2 | Template/SWAR timestamp parser + table-driven calendar + days lookup, F3 with leading-zero and overflow handling | 80-150 ms | **Medium**: the most edge cases (leap, offsets, F3 overflow, `\r`); fuzz against oracle |
| 3 | 7-bit order-preserving u64 ID packing, tier-2 intern fallback; no strings anywhere after parse | 60-120 ms | Low-medium: fallback path must be tested (long, UTF-8, NUL, prefix IDs) |
| 4 | Person = name key; read-only bucketized linked-ID table on huge pages with prefetch pipeline; unlinked IDs map to themselves (no interning of all user IDs) | 40-80 ms | Low |
| 5 | Dedup on 16 B tuples in L2-sized radix partitions; loser bitmap; full records never move for dedup; invalid lines never reach the person phase | 30-60 ms | Low: invalid kept copy still kills later copies (fixture E5/X09) |
| 6 | Memory hygiene: compact records, THP arenas pre-faulted in parallel, arena reuse, `_exit(0)`, chunk-local mmap or pread into reused buffers | 30-80 ms | None |
| 7 | Partitioned person phase: local person table, counting sort, insertion/pdq/radix by size, LPT scheduling for whales and kiosk giants | 20-50 ms | Low |
| 8 | Skip click dedup entirely | 5-15 ms | Very low (rules guarantee identical copies) |
| 9 | One sweep per person for credit (first-of-last-ts-group) and assists (sliding active set), per-thread dense `[campaign][day]` counters | 10-30 ms | Low; cross-check with interval-union method B |
| 10 | Hand-written uncompressed PLAIN Parquet, one file per thread, INT32/INT64 widths | 10-40 ms (vs arrow-rs) | Medium: format validity; test with pyarrow and DuckDB |

Honourable mentions: CAS union-find instead of sequential (5-15 ms at 20M, more at 60M; low risk);
`posix_fadvise(WILLNEED)` for a cold cache (large gain if cold, none if warm); AVX-512 tokenizer
variant (0-20 ms, measure: frequency license may cancel it).

### 8.4 Exactness checklist for the implementer

- Day of a session = UTC date of its start, computed from `window_start`, not assuming midnight.
- Late check uses the *kept* copy's own `ts` and `ingest_ts`; exactly 86,400,000 ms is kept.
- Price only matters for `purchase`; `view`/`cart` ignore whatever `price_cents` holds.
- A product not in `products.csv` (including case variants) is `unknown`; an empty product invalidates.
- Click window is `[ws - 7 d, we)`; `ws - 7 d` exactly is valid.
- Credit and assists use the same qualifying set `[s - 7d, s]`, inclusive both ends.
- campaign_daily includes assist-only rows with zeros in the credited columns; organic's
  `assisted_sessions` is 0.
- Self-check before exit (cheap, debug and release): summed `sessions` and `revenue_cents` equal across
  the two tables; summed `assisted_sessions` >= summed credited non-organic `sessions`.
