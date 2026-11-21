# Spec notes (oracle reading of rules.md)

Each line is: rule, then my reading, then (in quotes) the rules.md text it rests on. "AMBIG" marks the points the lead should decide on; none of them occur in generated data as far as rules.md describes it.

## Files and CSV dialect
- [ ] Read every regular `*.csv` file in `events/`, `clicks/`, `links/`; order does not matter. "Read every `*.csv` file". AMBIG: dotfiles named `.x.csv`. The oracle reads them (os.listdir). The generator never writes one.
- [ ] Line splitting: split the file bytes on `\n` only. A trailing empty segment after the final `\n` is not a line. A last line with no final `\n` is still a line. "Only `\n` ends a line." (X11, X12)
- [ ] Header: the first line of each file is skipped exactly once. A later line equal to the header is data (in links it joins `user_id_a` and `user_id_b`; in events/clicks its ts is bad). "Every CSV file has a header row." (L3, X12)
- [ ] A header-only file, with or without a final `\n`, contributes nothing. AMBIG: a 0-byte file has no header. The oracle treats it as empty. Out of spec.
- [ ] Split on every `,`. A wrong field count means the line is malformed and dropped. A trailing comma adds a field. "every comma separates fields"; "A line with the wrong number of fields ... is malformed". (X08)
- [ ] An empty line is malformed. That includes a blank line at the end of a file (`...\n\n`). "An empty line is malformed too." (X12)
- [ ] Quotes: a field is either unquoted with no `"` anywhere, or `"` + inner + `"` with no `"` in the inner part. Everything else makes the whole line malformed: `"a"b`, `a"b"`, a lone `"`, `"""`, `"a""b"` (no `""` escape), an unbalanced quote. This holds even in a field that is later ignored (a view's price). "with a quote anywhere except around a whole field, is malformed". (X08)
- [ ] `""` and an empty field both mean null. Spaces are kept (`" "` is a one-space value, not null). "An empty field means null"; "Spaces are part of a value". (X08 e16, E19)
- [ ] `\r` is data. A `\r` at the end of a link side makes it a different ID. "a `\r` is an ordinary character". (E20, X25)
- [ ] Work on bytes. Every ID comparison (equality, smallest, ties) is UTF-8 byte order, which equals code point order. UTF-16 order (Java/JS/.NET) differs above U+FFFF. Locale collation differs everywhere. "byte-wise smallest". (X13)

## Timestamps
- [ ] F1 is exactly 20 bytes `DDDD-DD-DDTDD:DD:DDZ`. F2 is exactly 25 bytes `DDDD-DD-DDTDD:DD:DD[+-]DD:DD`. F3 is one or more ASCII digits. Nothing else is valid: no fractions, no lowercase `t`/`z`, no space, no missing zone, no `+HHMM`, no `+HH`, no seconds-less form, no Unicode minus, no sign or whitespace on F3. (X05, E13, E17)
- [ ] Calendar check: month 1-12, day within the Gregorian month length (leap = divisible by 4 and not by 100, or divisible by 400), hour 0-23, minute 0-59, second 0-59. No normalising: Nov 31 does not roll over to Dec 1. (X01-X06)
- [ ] F2: the local part is validated as written, before applying the offset. `2027-02-29T01:00:00+02:00` is bad even though it would be Feb 28 23:00 UTC. `T24:00:00+01:00` is bad. The offset has hours 00-23 and minutes 00-59, and you subtract it. `-00:00` is UTC. "impossible calendar value ... an offset outside its range". (X02, E17)
- [ ] F3: arbitrary length, leading zeros allowed, the value is an exact integer. Too big for 64 bits means out of window, never wrapped. 2^64+T is a trap for a u64 parse that wraps. "too long for a 64-bit integer is not malformed, only outside the window". (E18, X23)
- [ ] Digits are ASCII `0-9` only. "`٢٥٠` and `２５０` are not digits".
- [ ] Event window: `ws <= ts < we`. Click window: `ws - 7d <= ts < we`.
- [ ] `ingest_ts` is "always valid F1". AMBIG: if it is not, the oracle parses it with the general parser and drops the line with a stderr warning. Out of spec.
- [ ] Manifest window strings are parsed with the same parser.

## Links
- [ ] Drop malformed lines and lines with either side null (including `""`). Union-find. Self-links and repeats are no-ops. Links are transitive across files and order. (L2, L3, X11, X25)
- [ ] The name is the byte-wise smallest ID in the component, even one used only in links. An upper-cased side is a different ID and can split a person (`U2` sorts before every lower-case ID). (X11, X14)
- [ ] An ID in no link line is its own person.

## Events
- [ ] Parse, then drop lines with an empty event_id. Malformed lines never take part in dedup. (X09 e6)
- [ ] Dedup on the exact event_id bytes, keeping the smallest `ingest_ts` (compared as instants). Equal-ingest copies are identical, so keep any. The kept copy's user_id decides the person (E4). (X09 e8, X31 `E1` vs `e1`)
- [ ] Validate only the kept copy. A kept copy with an empty user, a bad or out-of-window ts, a bad type, an empty product, or a bad purchase price drops the event, and later copies do not rescue it. (E5, X09)
- [ ] event_type is exactly `view`, `cart` or `purchase`. (X31)
- [ ] Purchase price: ASCII digits only, value in [1, 10,000,000] after removing leading zeros, any length. `+100`, `1_000`, ` 100`, `100 `, `0`, `00`, empty and 5000-digit values are all bad. Note that Rust `"+100".parse::<u64>()` succeeds, DuckDB `TRY_CAST(' 100' AS BIGINT)` trims spaces, and Python `int()` accepts `_`, whitespace and Unicode digits. (X22, E10, E18)
- [ ] View/cart price is ignored whatever it holds, as long as the line is not malformed. (X09 e11, X31)
- [ ] Late: drop when `ingest_ms - ts_ms > 86,400,000` (milliseconds, after converting the offset). Exactly 24 h is kept, and so is a negative difference. (E6, X21)
- [ ] Order of steps: validation and late-drop both act on the kept copy. Their relative order does not matter.

## Sessions
- [ ] Per person, sort by ts. A new session starts when `ts - prev_ts > 1,800,000`. In a sort-based implementation the equal-ts rule holds automatically, because equal timestamps have gap 0. (E1, E2, E16, X24, X27)
- [ ] Session day = UTC date of the first event. Every event and purchase counts on that day in both tables. (E3, X16, X24)
- [ ] Category lookup uses exact product_id bytes. Missing products become `unknown`. (X29)

## Clicks
- [ ] Parse, then drop an empty click_id. Dedup on click_id and keep any copy. AMBIG: if copies differed (rules say they never do), keep-any is order-dependent. The oracle keeps the first copy in sorted file order. (X10)
- [ ] Valid when user non-empty, ts valid and in the click window, and campaign_id exactly in campaigns.csv (an empty campaign is not). (A6, X10, X29)
- [ ] Credit: among the person's valid clicks with `start - 7d <= click.ts <= start`, take the max ts, and on a ts tie the smallest click_id byte-wise (`k10` < `k9`, `kB` < `ka`). Otherwise `organic`. (A2-A5, X19, X20)
- [ ] Assists: the distinct campaigns among the same qualifying set, including the crediting one, on the session's day. `organic` never assists. (A11, A12, X18, X19)
- [ ] A click that qualifies for one session can be too old for a later one. Each session is judged on its own start. (X28)

## Outputs
- [ ] user_daily is keyed by (person name, day): sessions, events, revenue, top_category. top_category is the category with the largest sum of purchase prices over the person's purchases in all of that day's sessions (sum, not max, and not per session). A tie goes to the byte-wise smallest category. Null when there are no valid purchases. `unknown` competes. (E8, E9, X17)
- [ ] campaign_daily is keyed by (campaign, day): sessions credited, purchasing_sessions (credited sessions with at least one surviving purchase, counted once per session), revenue, assisted_sessions. A row exists if the campaign was credited or assisted that day. Assist-only rows are 0/0/0/n. Channel comes from campaigns.csv, or `organic`. (A11, X18, X26)
- [ ] `day` is Parquet DATE (date32). Integers are int64. Strings are UTF-8.
- [ ] Empty results: still write one zero-row Parquet file per table with the full schema. "each one or more `*.parquet` files", and the harness errors on a folder with no parquet. (X30) AMBIG: the rules never show an empty-table case. Writing a 0-row file is the only safe reading.

## Likely traps for the fast implementations
1. CSV readers (polars, duckdb, arrow, Rust csv crate): they accept `"a""b"` as `a"b`, accept `"a"b`, treat `\r\n` as a line end, trim whitespace, infer or skip header-like rows, or treat a quoted empty as an empty string instead of null. Both readings drop the row here, so only malformed/quote handling and `\r` really matter.
2. Last line without `\n`; a blank last line; a header-only file without `\n`.
3. Timestamp libraries: lenient strptime (single-digit fields), normalising mktime/chrono-style overflow (Feb 30 becomes Mar 2, Nov 31 becomes Dec 1), lowercase `t`/`z` accepted, offsets `+0200` accepted, F2 local date checked after conversion.
4. Integer parsing: a leading `+` (Rust parse, C strtol), whitespace (DuckDB cast), 64-bit overflow or wrap for F3 and for prices, float parsing of F3.
5. Dedup after validation, or dedup that prefers the first valid copy (wrong). Also comparing ingest_ts as raw strings while quotes are still on.
6. Naming persons by union-find root instead of min ID. UTF-16 or collation-based string compare in name, click-id tie, and top_category tie.
7. Assists counted per click instead of per campaign. A missing assist-only row. A missing crediting campaign in assists.
8. top_category computed per session rather than per person-day, or by max instead of sum.
9. Late check at second precision or on local time.
10. Writing no file at all when a table is empty.
