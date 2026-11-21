# Extra fixtures (spec/oracle agent)

Same layout as `fixtures/`: `<case>/input` in the layout of `/data`, `<case>/expected/{user_daily,campaign_daily}.csv`.
Every expected row was derived by hand from `rules.md`; the derivation sits next to each case in
`oracle/build_extra_fixtures.py`, which regenerates the `X*` folders (it never runs an implementation). QA cases go under `qa_*`.

Defaults as in the official set: window 2026-10-01 to 2026-10-15 (X01-X04 and X06 use their own windows), products
`p1` shoes and `p2` books (X17 adds `p3` Zeta), campaigns `c_1` search and `c_2` social, `p9` missing, header-only links
unless noted. Run: `.venv/bin/python scripts/run_fixtures.py fixtures_extra -- <command>`.

| Case | Catches |
| --- | --- |
| X01_leap_2028 | Feb 29 2028 is valid, Feb 30 is not; an F2 offset can move an event onto Feb 29 |
| X02_nonleap_2027 | Feb 29 outside a leap year is bad; an F2 local date/hour is validated as written (no normalising through the offset) |
| X03_century_2100 | 2100 is not a leap year (divisible by 100, not 400) |
| X04_leap_2000 | 2000 is a leap year (divisible by 400); F3 for Feb 29 2000 is 951782400000 |
| X05_ts_forms | Strict F1/F2/F3 grammar: every malformed variant (bounds, case, separators, zone, sign, spaces) is bad |
| X06_nov31 | Nov 31 is bad for events and for clicks (no rollover to Dec 1); Nov 30 is fine |
| X07_f2_window_edges | F2 offsets moving ts across both window edges and across a date; a session cut by window_end |
| X08_quoting | Quote rules: whole-field only; `"a"b`, mid-field, lone, triple and unbalanced quotes, extra/missing field all malformed (even in an ignored price); `""` is null |
| X09_dedup_earliest_invalid | Dedup keeps the smallest ingest_ts even when that copy is late, mistyped, userless, productless or badly priced; a malformed earliest copy never counts; a view's junk price is fine |
| X10_click_dedup_empty_fields | A click copied with different quoting credits and assists once; empty campaign_id, empty user_id and empty click_id drop the click |
| X11_links_chain_files | Links chain across three files (one header-only, one without a final newline); the name X9 appears in no event or click; a click by another ID of the person credits |
| X12_file_layout | Header-only files (with and without newline), last line without newline, file ending in a blank line, a data row equal to the header |
| X13_bytewise_names | Names are the byte-wise (UTF-8, i.e. code point) smallest ID: 'B'<'a', 'Z'<'a'<'é'<'ｚ'<'😀' (UTF-16 order would put 😀 before ｚ) |
| X14_upper_link_split | An upper-cased link side is another ID: u2 and U2 are different, splitting one person into two; U2 names a person with no events |
| X15_interleaved_ids | One person, three IDs interleaved: gaps measured across IDs, 30:01 splits; clicks from different IDs credit/assist |
| X16_midnight_both_tables | A session crossing midnight counts all events and revenue on its start day in both tables |
| X17_top_category | top_category sums per day across sessions; ties byte-wise ('Zeta'<'books', 'shoes'<'unknown'); unknown can win; no valid purchase gives null |
| X18_assists_only_rows | A campaign that only assists gets a 0-credited row on every day it assisted; one campaign credits and assists |
| X19_click_tie_bytewise | Same-ms clicks in different forms (F1 vs F3) tie; smallest click_id byte-wise ('kB'<'ka', 'k10'<'k9'); same campaign twice assists once |
| X20_click_7d_boundaries | Exactly 7 d qualifies, 7 d + 1 ms does not, 1 ms after start does not; an F2 click exactly at the start credits |
| X21_late_boundary | Late cutoff in ms across forms: 24h kept, 24h+1ms dropped, F2 offset applied before the check, ingest days before ts kept |
| X22_price | Price: 10000000 kept, 10000001 dropped, 30 leading zeros fine, 41- and 5000-digit values dropped, 0/00/+100/1e3/trailing space/empty dropped |
| X23_f3_extremes | F3 beyond 64 bits (incl. 2^64+T, which wraps into the window) is out of window; 100 leading zeros fine; 0 and signed values bad |
| X24_chain_exact_30min | A chain of exactly-30-minute gaps is one session that crosses midnight; 30:00.001 then splits on the new day |
| X25_link_dirt | Link sides: quoted-empty dropped, trailing space and trailing \r are distinct IDs; name 'u1 ' (with space) for u2 |
| X26_purchasing_sessions | purchasing_sessions counts sessions, not purchases; sessions of two people credited to one campaign on one day add up |
| X27_equal_ts_across_ids_files | Equal-ts events from different IDs and files, read in reverse order, share a session; a gap is measured from the latest distinct ts |
| X28_click_window_vs_session | A click 7 days before the window that is valid but older than 7 days before the session does nothing; a click 1 s before the click window is dropped |
| X29_unknown_product_case | Product and campaign lookups are exact: 'P1' and 'p1 ' are unknown products; 'C_1' is an unknown campaign |
| X30_header_only_everything | Every events/clicks/links file holds only the header: both tables are empty (but must exist as Parquet) |
| X31_event_type_and_ids | event_type exact ('cart ' , 'CART', 'click' dropped); a view/cart with an empty price is fine; event_id case matters for dedup |

Mutation check (oracle with one rule broken; the official 36 pass every one of these mutants):
`is_leap = y%4==0` fails X03; dropping a last line without `\n` fails X11, X12; F3 parsed mod 2^64 fails X23; UTF-16 name order fails X13;
allowing a quote inside a quoted field fails X08; late check on whole seconds fails X21; assists counted per click fails X18, X19;
top_category by max instead of sum fails X17.
