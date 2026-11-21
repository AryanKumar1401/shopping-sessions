# QA findings

One line per confirmed mismatch: impl, rule, minimal input, fixture. Every expected output was checked by
hand against rules.md (oracle verdict in the last column). Transient failures caused by implementations being
rebuilt mid-run were re-run and are not listed.

| # | Impl | Rule (rules.md) | Minimal input | Fixture | Oracle |
| --- | --- | --- | --- | --- | --- |
| 1 | duck | CSV dialect + Linked IDs 3: a value may hold any UTF-8 char except `,` `"` `\n`; the name is the byte-wise smallest ID | links `u1 ,\x01`; one view by `u1 ` in window. Expected name `\x01` (0x01 < 'u'); duck outputs `u1 ` (its raw-line reader uses `\x01` as the "impossible" delimiter, so the link line is lost). Low real-data risk: the official generator never writes control bytes | `fixtures_extra/qa_01_ctrl_byte_id` | right. FIXED in duck (passes as of 23:40) |
| 2 | rust | Categories (Events 7): products.csv may use any number of categories; rules.md sets no limit | 500 products in 500 categories, one view + one purchase. Engine exits 1 "slow path not implemented" (fast path needs <= 429 categories: `MAX_CATS = u32::MAX / PRICE_MUL`). Official generator uses 20 categories, so this is a robustness gap rather than a scoring risk | `fixtures_extra/qa_02_many_categories` | right. FIXED by the slow path (23:11 build) |
| 3 | rust | Manifest: the window is "different for every dataset"; rules.md does not fix its length | 60-day window (events Oct 1 and Nov 29, click 1 h before). Engine exits 1 "slow path not implemented" (fast path needs `we - (ws - 7d) < 2^32 ms` (~42.7 days of window)). Official generator always uses 14 days | `fixtures_extra/qa_03_long_window` | right. FIXED by the slow path (23:11 build) |

## Not bugs (harness/transient, re-run clean)
- Early runs shared `out/qa/w/<seed>` between concurrent diff.py processes (fixed: per-pid dirs). Seeds 56-60 rust/oracle errors came from that.
- duck seeds 670-687 and rust_t1 seed 12490 (exit 127) hit binaries/scripts while teammates were rebuilding. diff.py now re-runs a failure once after 8 s before reporting it.

## Coverage
- Oracle mutants (`qa/mutants.py`, 34 single-rule breaks): all caught by fuzz seeds 0:400 (most by seed 0-2; slowest: header-row 297, click-window start 74, late at second precision 60, Nov-31 rollover 54).
- Medium scale: rust and duck both MATCH `data/dev-answer`; rust vs duck MATCH on `data/s7-5m` (571,890 user_daily, 28,014 campaign_daily rows). The oracle was not run on s7-5m (host swap already at 6.7 GB).
