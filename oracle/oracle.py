#!/usr/bin/env python3
"""
Reference ("oracle") implementation of the Shopping Sessions rules (rules.md).

Deliberately slow and literal: every rule is implemented by hand, on bytes,
following rules.md sentence by sentence. Standard library only for all logic;
pyarrow is used only to write the final Parquet files.

Inputs:  $DATA_DIR  (or argv[-2])
Outputs: $OUTPUT_DIR/user_daily/part-0.parquet, $OUTPUT_DIR/campaign_daily/part-0.parquet
"""
import datetime
import json
import os
import sys

# Very long digit strings (prices, F3 timestamps) must still be numbers.
sys.set_int_max_str_digits(0)

DIGITS = b"0123456789"
MS_PER_DAY = 86_400_000
SESSION_GAP_MS = 1_800_000            # 30 minutes
CLICK_LOOKBACK_MS = 7 * MS_PER_DAY    # 604,800,000
LATE_MS = 24 * 3600 * 1000            # 24 hours
PRICE_MAX = 10_000_000

EVENT_COLS = 7   # event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents
CLICK_COLS = 4   # click_id,user_id,campaign_id,ts
LINK_COLS = 2    # user_id_a,user_id_b
PRODUCT_COLS = 2
CAMPAIGN_COLS = 2


def log(*a):
    print(*a, file=sys.stderr)


# ---------------------------------------------------------------- CSV dialect

def split_lines(data):
    """Only b"\\n" ends a line. A trailing empty segment after the final \\n is
    not a line. A last line without a trailing \\n is still a line."""
    parts = data.split(b"\n")
    if parts and parts[-1] == b"":
        parts.pop()
    return parts


def parse_field(raw):
    """Return the field value (bytes; b"" means null) or None if malformed.
    Quotes are allowed only as a pair wrapping the whole field."""
    if raw.startswith(b'"'):
        if len(raw) < 2 or not raw.endswith(b'"'):
            return None
        inner = raw[1:-1]
        if b'"' in inner:
            return None
        return inner
    if b'"' in raw:
        return None
    return raw


def parse_line(line, ncols):
    """Return list of field values or None if malformed."""
    if line == b"":
        return None                       # an empty line is malformed
    raw_fields = line.split(b",")         # every comma separates fields
    if len(raw_fields) != ncols:
        return None
    out = []
    for raw in raw_fields:
        v = parse_field(raw)
        if v is None:
            return None
        out.append(v)
    return out


def read_rows(path, ncols):
    """Yield parsed rows of one file; first line of the file is the header,
    skipped exactly once (a later identical line is data)."""
    with open(path, "rb") as f:
        data = f.read()
    lines = split_lines(data)
    for line in lines[1:]:
        row = parse_line(line, ncols)
        if row is not None:
            yield row


def csv_files(folder):
    if not os.path.isdir(folder):
        return []
    names = sorted(n for n in os.listdir(folder) if n.endswith(".csv"))
    return [os.path.join(folder, n) for n in names if os.path.isfile(os.path.join(folder, n))]


# ---------------------------------------------------------------- timestamps

def is_digits(b):
    return len(b) > 0 and all(c in DIGITS for c in b)


def dnum(b):
    """Value of a non-empty ASCII digit string (caller checked is_digits)."""
    n = 0
    for c in b:
        n = n * 10 + (c - 48)
    return n


def is_leap(y):
    return (y % 4 == 0 and y % 100 != 0) or (y % 400 == 0)


def days_in_month(y, m):
    if m == 2:
        return 29 if is_leap(y) else 28
    if m in (4, 6, 9, 11):
        return 30
    return 31


def days_from_civil(y, m, d):
    """Days since 1970-01-01 in the proleptic Gregorian calendar (counted
    the slow obvious way relative to year 1970)."""
    days = 0
    if y >= 1970:
        for yy in range(1970, y):
            days += 366 if is_leap(yy) else 365
    else:
        for yy in range(y, 1970):
            days -= 366 if is_leap(yy) else 365
    for mm in range(1, m):
        days += days_in_month(y, mm)
    return days + (d - 1)


def parse_datetime_part(b):
    """b is the 19 bytes 'YYYY-MM-DDTHH:MM:SS'. Return local epoch ms or None."""
    if len(b) != 19:
        return None
    if b[4:5] != b"-" or b[7:8] != b"-" or b[10:11] != b"T" or b[13:14] != b":" or b[16:17] != b":":
        return None
    ys, mos, ds, hs, mis, ss = b[0:4], b[5:7], b[8:10], b[11:13], b[14:16], b[17:19]
    for part in (ys, mos, ds, hs, mis, ss):
        if not is_digits(part):
            return None
    y, mo, d, h, mi, s = map(dnum, (ys, mos, ds, hs, mis, ss))
    if not (1 <= mo <= 12):
        return None
    if not (1 <= d <= days_in_month(y, mo)):
        return None
    if not (0 <= h <= 23 and 0 <= mi <= 59 and 0 <= s <= 59):
        return None
    days = days_from_civil(y, mo, d)
    return ((days * 24 + h) * 60 + mi) * 60_000 + s * 1000


def parse_ts(b):
    """Return epoch ms (int, possibly huge) or None for a bad timestamp."""
    if len(b) == 0:
        return None
    if is_digits(b):                                   # F3
        return dnum(b) if len(b) <= 40 else int(b)    # int() only for speed on huge strings
    if len(b) == 20 and b[19:20] == b"Z":              # F1
        return parse_datetime_part(b[:19])
    if len(b) == 25 and b[19:20] in (b"+", b"-") and b[22:23] == b":":   # F2
        local = parse_datetime_part(b[:19])
        if local is None:
            return None
        oh, om = b[20:22], b[23:25]
        if not (is_digits(oh) and is_digits(om)):
            return None
        oh, om = dnum(oh), dnum(om)
        if not (0 <= oh <= 23 and 0 <= om <= 59):
            return None
        off = (oh * 60 + om) * 60_000
        return local - off if b[19:20] == b"+" else local + off
    return None


def parse_price(b):
    """Digits only, value 1..10,000,000. Returns int or None."""
    if not is_digits(b):
        return None
    stripped = b.lstrip(b"0")
    if len(stripped) > 8:          # > 99,999,999, certainly over the limit
        return None
    v = dnum(stripped) if stripped else 0
    if 1 <= v <= PRICE_MAX:
        return v
    return None


def ms_to_date(ms):
    return datetime.date(1970, 1, 1) + datetime.timedelta(days=ms // MS_PER_DAY)


# ---------------------------------------------------------------- union-find

class UnionFind:
    def __init__(self):
        self.parent = {}

    def find(self, x):
        p = self.parent
        if x not in p:
            p[x] = x
            return x
        root = x
        while p[root] != root:
            root = p[root]
        while p[x] != root:
            p[x], x = root, p[x]
        return root

    def union(self, a, b):
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            p = self.parent
            # keep the byte-wise smallest ID as root, so root == person name
            if rb < ra:
                ra, rb = rb, ra
            p[rb] = ra


# ---------------------------------------------------------------- pipeline

def main():
    data_dir = os.environ.get("DATA_DIR") or (sys.argv[-2] if len(sys.argv) >= 3 else "/data")
    out_dir = os.environ.get("OUTPUT_DIR") or (sys.argv[-1] if len(sys.argv) >= 3 else "/output")

    with open(os.path.join(data_dir, "manifest.json"), "rb") as f:
        manifest = json.load(f)
    win_start = parse_ts(manifest["window_start"].encode())
    win_end = parse_ts(manifest["window_end"].encode())
    assert win_start is not None and win_end is not None, "bad manifest"
    click_start = win_start - CLICK_LOOKBACK_MS

    # products / campaigns (clean, but parsed with the same dialect)
    category = {}
    for path in [os.path.join(data_dir, "products.csv")]:
        for row in read_rows(path, PRODUCT_COLS):
            category[row[0]] = row[1]
    channel = {}
    for path in [os.path.join(data_dir, "campaigns.csv")]:
        for row in read_rows(path, CAMPAIGN_COLS):
            channel[row[0]] = row[1]

    # ---- links
    uf = UnionFind()
    for path in csv_files(os.path.join(data_dir, "links")):
        for a, b in read_rows(path, LINK_COLS):
            if a == b"" or b == b"":
                continue
            uf.union(a, b)

    def person(uid):
        if uid in uf.parent:
            return uf.find(uid)
        return uid

    # ---- events: parse, dedup (smallest ingest_ts), validate, late-drop
    kept = {}   # event_id -> (ingest_ms, row)
    for path in csv_files(os.path.join(data_dir, "events")):
        for row in read_rows(path, EVENT_COLS):
            eid = row[0]
            if eid == b"":
                continue
            ingest = parse_ts(row[3])
            if ingest is None:
                # rules: ingest_ts is always valid F1. Never happens in generated data.
                log("WARNING: unparseable ingest_ts, line dropped:", row)
                continue
            prev = kept.get(eid)
            if prev is None or ingest < prev[0]:
                kept[eid] = (ingest, row)

    # person -> list of (ts, is_purchase, price, category)
    per_person = {}
    for eid, (ingest, row) in kept.items():
        _, uid, ts_raw, _, etype, pid, price_raw = row
        if uid == b"":
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (win_start <= ts < win_end):
            continue
        if etype not in (b"view", b"cart", b"purchase"):
            continue
        if pid == b"":
            continue
        price = 0
        is_purchase = etype == b"purchase"
        if is_purchase:
            price = parse_price(price_raw)
            if price is None:
                continue
        if ingest - ts > LATE_MS:
            continue
        cat = category.get(pid, b"unknown")
        per_person.setdefault(person(uid), []).append((ts, is_purchase, price, cat))
    del kept

    # ---- clicks: parse, dedup (any copy), validate
    clicks_seen = {}
    for path in csv_files(os.path.join(data_dir, "clicks")):
        for row in read_rows(path, CLICK_COLS):
            if row[0] == b"":
                continue
            if row[0] not in clicks_seen:
                clicks_seen[row[0]] = row
    clicks_by_person = {}   # person -> list of (ts, click_id, campaign_id)
    for cid, (_, uid, camp, ts_raw) in clicks_seen.items():
        if uid == b"":
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (click_start <= ts < win_end):
            continue
        if camp not in channel:
            continue
        clicks_by_person.setdefault(person(uid), []).append((ts, cid, camp))
    del clicks_seen

    # ---- sessionize + attribute
    user_daily = {}       # (person, date) -> [sessions, events, revenue, {cat: sum}]
    campaign_daily = {}   # (campaign, date) -> [sessions, purchasing, revenue, assisted]

    for p, evs in per_person.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        cur = None
        prev_ts = None
        for e in evs:
            if cur is None or e[0] - prev_ts > SESSION_GAP_MS:
                cur = []
                sessions.append(cur)
            cur.append(e)
            prev_ts = e[0]
        pclicks = clicks_by_person.get(p, [])
        for s in sessions:
            start = s[0][0]
            day = ms_to_date(start)
            n_events = len(s)
            revenue = sum(e[2] for e in s if e[1])
            purchasing = any(e[1] for e in s)

            ud = user_daily.setdefault((p, day), [0, 0, 0, {}])
            ud[0] += 1
            ud[1] += n_events
            ud[2] += revenue
            for e in s:
                if e[1]:
                    ud[3][e[3]] = ud[3].get(e[3], 0) + e[2]

            qualifying = [c for c in pclicks if c[0] <= start and start - c[0] <= CLICK_LOOKBACK_MS]
            best = None
            for c in qualifying:
                if best is None or c[0] > best[0] or (c[0] == best[0] and c[1] < best[1]):
                    best = c
            credited = best[2] if best is not None else b"organic"
            cd = campaign_daily.setdefault((credited, day), [0, 0, 0, 0])
            cd[0] += 1
            cd[1] += 1 if purchasing else 0
            cd[2] += revenue
            for camp in set(c[2] for c in qualifying):
                campaign_daily.setdefault((camp, day), [0, 0, 0, 0])[3] += 1

    # ---- write
    import pyarrow as pa
    import pyarrow.parquet as pq

    def dec(b):
        return b.decode("utf-8")

    def top_cat(cats):
        if not cats:
            return None
        best = None
        for c, v in cats.items():
            if best is None or v > best[1] or (v == best[1] and c < best[0]):
                best = (c, v)
        return dec(best[0])

    keys = sorted(user_daily)
    ud_table = pa.table({
        "user_id": pa.array([dec(k[0]) for k in keys], pa.string()),
        "day": pa.array([k[1] for k in keys], pa.date32()),
        "sessions": pa.array([user_daily[k][0] for k in keys], pa.int64()),
        "events": pa.array([user_daily[k][1] for k in keys], pa.int64()),
        "revenue_cents": pa.array([user_daily[k][2] for k in keys], pa.int64()),
        "top_category": pa.array([top_cat(user_daily[k][3]) for k in keys], pa.string()),
    })
    keys = sorted(campaign_daily)
    cd_table = pa.table({
        "campaign_id": pa.array([dec(k[0]) for k in keys], pa.string()),
        "channel": pa.array(["organic" if k[0] == b"organic" else dec(channel[k[0]]) for k in keys], pa.string()),
        "day": pa.array([k[1] for k in keys], pa.date32()),
        "sessions": pa.array([campaign_daily[k][0] for k in keys], pa.int64()),
        "purchasing_sessions": pa.array([campaign_daily[k][1] for k in keys], pa.int64()),
        "revenue_cents": pa.array([campaign_daily[k][2] for k in keys], pa.int64()),
        "assisted_sessions": pa.array([campaign_daily[k][3] for k in keys], pa.int64()),
    })
    for name, table in (("user_daily", ud_table), ("campaign_daily", cd_table)):
        d = os.path.join(out_dir, name)
        os.makedirs(d, exist_ok=True)
        pq.write_table(table, os.path.join(d, "part-0.parquet"))
    log(f"oracle: {ud_table.num_rows} user_daily rows, {cd_table.num_rows} campaign_daily rows")


if __name__ == "__main__":
    main()
