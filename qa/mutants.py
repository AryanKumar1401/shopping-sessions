#!/usr/bin/env python3
"""
Fuzzer power check: make oracle mutants (one rule broken each) under out/qa/mut/
and count how many fuzz seeds it takes qa/diff.py to catch each one.

    .venv/bin/python qa/mutants.py [--seeds 0:300] [--jobs 4]
"""
import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = (ROOT / "oracle/oracle.py").read_text()

MUTANTS = {
    "gap_ge": ("e[0] - prev_ts > SESSION_GAP_MS", "e[0] - prev_ts >= SESSION_GAP_MS"),
    "click_7d_strict": ("start - c[0] <= CLICK_LOOKBACK_MS]", "start - c[0] < CLICK_LOOKBACK_MS]"),
    "click_at_start_excluded": ("if c[0] <= start and", "if c[0] < start and"),
    "late_ge": ("if ingest - ts > LATE_MS:", "if ingest - ts >= LATE_MS:"),
    "late_seconds": ("if ingest - ts > LATE_MS:", "if ingest // 1000 - ts // 1000 > LATE_MS // 1000:"),
    "click_tie_max": ("(c[0] == best[0] and c[1] < best[1])", "(c[0] == best[0] and c[1] > best[1])"),
    "assist_per_click": ("for camp in set(c[2] for c in qualifying):", "for camp in [c[2] for c in qualifying]:"),
    "assist_excludes_credited": ("for camp in set(c[2] for c in qualifying):",
                                 "for camp in set(c[2] for c in qualifying) - {credited}:"),
    "topcat_tie_max": ("(v == best[1] and c < best[0])", "(v == best[1] and c > best[0])"),
    "name_not_min": ("            if rb < ra:\n                ra, rb = rb, ra\n", ""),
    "dedup_keep_last": ("if prev is None or ingest < prev[0]:", "if prev is None or ingest >= prev[0]:"),
    "quote_escape_ok": ("        if b'\"' in inner:\n            return None\n", ""),
    "stray_quote_ok": ("    if b'\"' in raw:\n        return None\n    return raw", "    return raw.replace(b'\"', b'')"),
    "f3_wrap": ("return dnum(b) if len(b) <= 40 else int(b)", "return int(b) % 2**64"),
    "no_last_line": ("    if parts and parts[-1] == b\"\":\n        parts.pop()", "    parts.pop()"),
    "price_plus": ("    if not is_digits(b):\n        return None\n    stripped",
                   "    b = b.lstrip(b'+')\n    if not is_digits(b):\n        return None\n    stripped"),
    "price_max_excl": ("if 1 <= v <= PRICE_MAX:", "if 1 <= v < PRICE_MAX:"),
    "offset_sign": ("return local - off if b[19:20] == b\"+\" else local + off",
                    "return local + off if b[19:20] == b\"+\" else local - off"),
    "offset_hour_24": ("if not (0 <= oh <= 23 and 0 <= om <= 59):", "if not (0 <= oh <= 24 and 0 <= om <= 59):"),
    "window_end_incl": ("not (win_start <= ts < win_end)", "not (win_start <= ts <= win_end)"),
    "click_window_start_excl": ("not (click_start <= ts < win_end)", "not (click_start < ts < win_end)"),
    "strip_cr": ("    parts = data.split(b\"\\n\")\n", "    parts = data.replace(b\"\\r\", b\"\").split(b\"\\n\")\n"),
    "empty_line_ignored_ok": ("    if line == b\"\":\n        return None", "    if line == b\"\":\n        return None\n    line = line.strip(b' ')"),
    "case_fold_type": ("if etype not in (b\"view\", b\"cart\", b\"purchase\"):",
                       "if etype.lower().strip() not in (b\"view\", b\"cart\", b\"purchase\"):"),
    "unknown_product_empty_ok": ("        if pid == b\"\":\n            continue\n", ""),
    "click_unknown_campaign_kept": ("        if camp not in channel:\n            continue\n",
                                    "        if camp not in channel and camp:\n            channel[camp] = b'?'\n"),
    "day_of_last_event": ("            day = ms_to_date(start)", "            day = ms_to_date(s[-1][0])"),
    "nonleap": ("return (y % 4 == 0 and y % 100 != 0) or (y % 400 == 0)", "return False"),
    "no_rollover_check": ("    if not (1 <= d <= days_in_month(y, mo)):\n        return None\n",
                          "    if not (1 <= d <= 31):\n        return None\n"),
    "purchasing_counts_purchases": ("cd[1] += 1 if purchasing else 0", "cd[1] += sum(1 for e in s if e[1])"),
    "dedup_after_validate": None,  # special-cased below
    "read_non_csv": ("names = sorted(n for n in os.listdir(folder) if n.endswith(\".csv\"))",
                     "names = sorted(n for n in os.listdir(folder))"),
    "self_link_breaks": ("            uf.union(a, b)", "            if a != b: uf.union(a, b)\n            else: uf.parent.pop(a, None)"),
    "header_row_skipped": ("    for line in lines[1:]:\n        row = parse_line(line, ncols)",
                           "    for line in lines[1:]:\n        if line == lines[0]: continue\n        row = parse_line(line, ncols)"),
}


def make(name, rep):
    src = SRC
    if name == "dedup_after_validate":
        # keep the earliest *valid-ts* copy instead of the earliest copy
        old = "            prev = kept.get(eid)\n"
        new = ("            pt = parse_ts(row[2])\n"
               "            if pt is None or not (win_start <= pt < win_end):\n"
               "                continue\n" + old)
        assert old in src
        return src.replace(old, new, 1)
    old, new = rep
    assert old in src, name
    return src.replace(old, new, 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", default="0:300")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--only", default=None)
    args = ap.parse_args()
    d = ROOT / "out/qa/mut"
    d.mkdir(parents=True, exist_ok=True)
    py = str(ROOT / ".venv/bin/python")
    res = {}
    for name, rep in MUTANTS.items():
        if args.only and name not in args.only.split(","):
            continue
        p = d / f"{name}.py"
        p.write_text(make(name, rep))
        r = subprocess.run([py, str(ROOT / "qa/diff.py"), "--seeds", args.seeds, "--jobs", str(args.jobs),
                            "--no-shrink", "--quiet", "--stop-after", "1", "--impl", f"m={py} {p}"],
                           capture_output=True, text=True)
        caught = [ln for ln in r.stdout.splitlines() if ln.startswith("seed ")]
        res[name] = caught[0].split(":")[0] if caught else "NOT CAUGHT"
        print(f"{name:30s} {res[name]}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
