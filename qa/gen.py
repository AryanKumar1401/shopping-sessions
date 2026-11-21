#!/usr/bin/env python3
"""
Seeded adversarial dataset generator for the Shopping Sessions rules (rules.md).

    .venv/bin/python qa/gen.py SEED OUTDIR [--events N]

Writes the /data layout (events/, clicks/, links/, products.csv, campaigns.csv,
manifest.json). Small (tens to a few thousand rows) and biased hard toward the
nasty corners of rules.md, but always within what rules.md guarantees:
  * ingest_ts of every well-formed event line is a valid F1;
  * copies of an event_id with equal ingest_ts carry identical values
    (they may be quoted differently);
  * copies of a click_id carry identical values (quoted differently);
  * products.csv / campaigns.csv are clean (unquoted, unique ids, never the
    category `unknown`, never the campaign `organic`);
  * everything is valid UTF-8, no value holds a comma, quote or newline.
Malformed lines (wrong field count, stray quotes, empty lines) may contain
anything, since step 1 drops them.

`generate(seed)` returns {relative_path: bytes} so the shrinker can work in
memory; `write(dataset, outdir)` puts it on disk.
"""
import datetime as dt
import itertools
import json
import os
import random
import sys

DAY = 86_400_000
HOUR = 3_600_000
MIN = 60_000
GAP = 1_800_000
WEEK = 7 * DAY
LATE = 24 * HOUR

EVENT_HEADER = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CLICK_HEADER = b"click_id,user_id,campaign_id,ts"
LINK_HEADER = b"user_id_a,user_id_b"

EPOCH = dt.datetime(1970, 1, 1)


# ------------------------------------------------------------------ time helpers

def civil(ms):
    """ms -> (y, mo, d, h, mi, s) for ms >= 0 whole-second instants."""
    t = EPOCH + dt.timedelta(milliseconds=ms)
    return t.year, t.month, t.day, t.hour, t.minute, t.second


def f1(ms):
    y, mo, d, h, mi, s = civil(ms - ms % 1000)
    return f"{y:04d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z"


def f2(ms, off_min):
    y, mo, d, h, mi, s = civil(ms - ms % 1000 + off_min * MIN)
    sign = "+" if off_min >= 0 else "-"
    a = abs(off_min)
    return f"{y:04d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}{sign}{a // 60:02d}:{a % 60:02d}"


def to_ms(y, mo, d, h=0, mi=0, s=0):
    return int((dt.datetime(y, mo, d, h, mi, s) - EPOCH).total_seconds()) * 1000


class Gen:
    def __init__(self, seed, n_events=None):
        self.seed = seed
        self.r = random.Random(seed)
        r = self.r
        if n_events is None:
            x = r.random()
            n_events = r.randint(3, 60) if x < 0.55 else r.randint(60, 400) if x < 0.9 else r.randint(400, 3000)
        self.n_events = n_events
        # per-seed dirt knobs (some seeds clean-ish, some filthy)
        self.dirt = r.choice([0.0, 0.02, 0.05, 0.1, 0.2, 0.35])
        self.quote_rate = r.choice([0.0, 0.1, 0.2, 0.5, 0.9])
        self.dup_rate = r.choice([0.0, 0.05, 0.15, 0.4])
        self.bad_ts_rate = r.choice([0.0, 0.01, 0.05, 0.15])
        self.late_rate = r.choice([0.0, 0.03, 0.1, 0.25])
        self.huge_rate = r.choice([0.0, 0.0, 0.0, 0.01, 0.05])       # giant fields (SIMD/piece boundaries)
        self.maxprice_rate = r.choice([0.0, 0.0, 0.0, 0.3, 0.95])    # revenue sums past 2^32
        self.extreme_ingest = r.choice([0.0, 0.0, 0.0, 0.02, 0.1])   # ingest_ts years from ts
        self.many_files = r.random() < 0.08
        self.form_w = r.choice([(58, 10, 32), (100, 0, 0), (0, 100, 0), (0, 0, 100), (34, 33, 33)])
        self.window()
        self.id_pools()
        self.catalog()

    # -------------------------------------------------------------- window

    def window(self):
        r = self.r
        x = r.random()
        if x < 0.25:   # windows holding Feb 29 2028 (or touching it)
            start = dt.date(2028, 2, 14) + dt.timedelta(days=r.randint(0, 17))
        elif x < 0.35:  # month/year ends
            start = r.choice([dt.date(2025, 12, 25), dt.date(2026, 12, 31), dt.date(2027, 2, 20),
                              dt.date(2027, 12, 20), dt.date(2028, 12, 25), dt.date(2025, 1, 1)])
        else:
            start = dt.date(2025, 1, 1) + dt.timedelta(days=r.randint(0, 4 * 365))
        length = 14 if r.random() < 0.85 else r.choice([1, 2, 3, 7, 20, 30])
        self.ws = to_ms(start.year, start.month, start.day)
        self.we = self.ws + length * DAY
        self.cs = self.ws - WEEK

    # -------------------------------------------------------------- id pools

    def id_pools(self):
        r = self.r
        base = ["u1", "u2", "u3", "u10", "u9", "a", "b", "z", "A", "U1", "u1 ", " u1", "u2\r", "é", "ｚ",
                "😀", "ü", "\t1", "x\x01y", "_", "0", "~", "u", "uu", "u1u1"]
        longp = ["L" * 15, "L" * 16, "L" * 17, "L" * 16 + "a", "L" * 16 + "b", "L" * 31 + "a", "L" * 32,
                 "L" * 32 + "a", "L" * 32 + "b", "L" * 33, "u" * 40, "user_long_prefix_00000000000001",
                 "user_long_prefix_00000000000002", "user_long_prefix_0000000000000A", "é" * 8 + "x",
                 "é" * 8 + "y", "é" * 9, "😀" * 4 + "a", "😀" * 5, "abcdefghijklmnop", "abcdefghijklmnoq",
                 "abcdefghijklmnopq", "abcdefghijklmnop\r", "Q" * 64, "Q" * 63 + "R"]
        k = r.choice([2, 3, 4, 6, 10, 20, 40])
        pool = r.sample(base, min(len(base), r.randint(1, 12))) + r.sample(longp, r.randint(0, 8))
        while len(pool) < k:
            pool.append(f"u{r.randint(1, 3 * k)}" if r.random() < 0.6 else r.choice(longp + base))
        if self.n_events > 3000:   # large mode: realistic-ish id counts so the oracle stays fast
            pool += [f"u{r.randint(0, 10 ** 9):x}" for _ in range(self.n_events // 25)]
        if r.random() < 0.05:
            pool += ["u1\x00", "\x00", "u1"]   # NUL is legal UTF-8: 'u1' < 'u1\0'
        if self.huge_rate:
            pool += ["H" * 5000, "H" * 4999 + "a", "H" * 70, "é" * 3000]
        self.users = list(dict.fromkeys(pool))
        # event / click id alphabets: tiny so that collisions are dense
        ne = max(2, int(self.n_events * r.choice([0.2, 0.5, 0.8, 1.0, 3.0])))
        self.eid_pool = [f"e{i}" for i in range(ne)] + ["E1", "e1 ", " e1", "e1\r", "é1", "e" * 17,
                                                        "e" * 33, "event_id"]
        self.cid_pool = ["k1", "k2", "k9", "k10", "kB", "ka", "K1", "k1 ", "é", "z", "k" * 17, "k" * 16 + "a",
                         "k" * 16 + "B", "k" * 33, "😀"] + [f"k{i}" for i in range(11, 11 + r.randint(0, 60))]
        if self.n_events > 3000:
            self.cid_pool += [f"k{r.randint(0, 10 ** 9):x}" for _ in range(self.n_events // 2)]

    def catalog(self):
        r = self.r
        cats = r.sample(["shoes", "books", "Zeta", "été", "ｚcat", "😀cat", "unknowm", "Unknown", "a", "b b",
                         "toys", "unknown ", " unknown"], r.randint(1, 6))
        prods = r.sample(["p1", "p2", "p3", "P1", "p10", "pé", "p" * 20, "p😀", "q"], r.randint(1, 7))
        self.products = {p: r.choice(cats) for p in prods}
        self.unknown_products = ["p9", "P2", "p1 ", " p1", "pp", "p" * 21, "p1\r", "unknown"]
        camps = r.sample(["c_1", "c_2", "c_3", "c_10", "c_9", "C_1", "c_é", "c_" + "1" * 30], r.randint(1, 6))
        chans = ["search", "social", "email", "display", "affiliate", "organic ", "Organic", "ñ"]
        self.campaigns = {c: r.choice(chans) for c in camps}
        self.unknown_campaigns = ["c_99", "C_2", "c_1 ", " c_1", "organic", "c_1\r", "c"]

    # -------------------------------------------------------------- timestamps

    def render_ts(self, ms):
        """A valid rendering of instant ms."""
        r = self.r
        if ms < 0:
            return str(ms)  # not valid; only used for anchors far off (never happens in practice)
        if ms % 1000:
            form = 3
        else:
            form = r.choices([1, 2, 3], weights=self.form_w)[0]
        if form == 1:
            return f1(ms)
        if form == 2:
            x = r.random()
            if x < 0.15:
                off = 0
            elif x < 0.7:
                off = r.randint(-600, 720)
            else:
                off = r.choice([-23 * 60 - 59, 23 * 60 + 59, -30, 30, 330, 345, -570, 840, -1439])
            s = f2(ms, off)
            if off == 0 and r.random() < 0.5:
                s = s[:-6] + "-00:00"
            return s
        s = str(ms)
        if r.random() < 0.15:
            s = "0" * r.choice([1, 2, 3, 4, 5, 6, 30, 100]) + s
        return s

    def bad_ts(self, ms):
        """A bad timestamp near instant ms (so a lenient parser would land in the window)."""
        r = self.r
        sec = ms - ms % 1000
        y, mo, d, h, mi, s = civil(sec)
        good1 = f1(sec)
        opts = [
            lambda: "",
            lambda: good1[:-1] + ".000Z",
            lambda: good1[:-1] + "z",
            lambda: good1.replace("T", "t"),
            lambda: good1.replace("T", " "),
            lambda: good1[:-1],
            lambda: good1[:-1] + "+0200",
            lambda: good1[:-1] + "+02",
            lambda: good1[:-1] + "+24:00",
            lambda: good1[:-1] + "+05:60",
            lambda: good1[:-1] + "-24:00",
            lambda: good1[:-1] + "+00:00Z",
            lambda: good1 + " ",
            lambda: " " + good1,
            lambda: good1 + "\r",
            lambda: str(ms) + ".0",
            lambda: str(ms) + "\r",
            lambda: f"{ms / 1000:.2f}e3",
            lambda: "-" + str(ms),
            lambda: "+" + str(ms),
            lambda: " " + str(ms),
            lambda: str(ms) + " ",
            lambda: "1.79e12",
            lambda: str(ms).replace("1", "١"),
            lambda: str(ms).replace("2", "２"),
            lambda: good1.replace("2", "２", 1),
            lambda: f"{y:04d}-{mo:02d}-{d:02d}T24:00:00Z",
            lambda: f"{y:04d}-{mo:02d}-{d:02d}T{h:02d}:60:00Z",
            lambda: f"{y:04d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}:60Z",
            lambda: f"{y:04d}-13-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-00-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-{mo:02d}-00T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-{mo:02d}-32T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-02-30T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-02-31T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-04-31T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-11-31T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"2027-02-29T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"2027-02-29T01:00:00+02:00",
            lambda: f"2100-02-29T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:04d}-{mo}-{d}T{h}:{mi}:{s}Z",
            lambda: f"{y:04d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}Z",
            lambda: f"{y:04d}{mo:02d}{d:02d}T{h:02d}{mi:02d}{s:02d}Z",
            lambda: f"{y:04d}/{mo:02d}/{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"+{y:04d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f"{y:05d}-{mo:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z",
            lambda: f2(sec, 60)[:-6] + "+1:00",
            lambda: f2(sec, 60)[:-6] + "+01:0",
            lambda: f2(sec, 60)[:-6] + "−01:00",
            lambda: f2(sec, 60).replace("+", " "),
            lambda: f2(sec, 60) + "Z",
            lambda: str(2 ** 64 + ms),
            lambda: str(2 ** 63 + ms),
            lambda: str(2 ** 64 * 3 + ms),
            lambda: str(10 ** 25 + ms),
            lambda: "9" * 30,
            lambda: "0",
            lambda: "T",
            lambda: "Z",
            lambda: "NaN",
            lambda: "ts",
            lambda: "86400000",
            lambda: "1970-01-01T00:00:00Z",
            lambda: "0000-01-01T00:00:00Z",
            lambda: "9999-12-31T23:59:59Z",
            lambda: "9999-12-31T23:59:59-23:59",
            lambda: "0" * 9000 + str(2 ** 64 + ms),
            lambda: str(ms) + "0" * 3,
            lambda: str(ms // 1000),
        ]
        return r.choice(opts)()

    def ts_field(self, ms, bad_rate=None):
        rate = self.bad_ts_rate if bad_rate is None else bad_rate
        if self.r.random() < rate:
            return self.bad_ts(ms)
        return self.render_ts(ms)

    # -------------------------------------------------------------- field dirt

    def quote(self, v, force=None):
        """Render one value as a field: maybe wrap it in quotes (syntax only)."""
        q = self.r.random() < self.quote_rate if force is None else force
        return f'"{v}"' if q else v

    def stray_quote(self, v):
        r = self.r
        return r.choice([f'"{v}"x', f'x"{v}"', f'"{v}', f'{v}"', f'{v[:1]}"{v[1:]}', '"""', f'"{v}""',
                         f'""{v}""', f'"{v[:1]}""{v[1:]}"', '"', f' "{v}"', f'"{v}" '])

    def line(self, values, ncols, crlf_ok=True):
        """Render a row; with probability `dirt` make it malformed."""
        r = self.r
        fields = [self.quote(v) for v in values]
        if r.random() < self.dirt:
            kind = r.randrange(7)
            if kind == 5 and not crlf_ok:
                kind = 0
            if kind == 0:
                i = r.randrange(len(fields))
                fields[i] = self.stray_quote(values[i])
            elif kind == 1:
                fields.append(r.choice(["", "x", '""']))
            elif kind == 2:
                del fields[r.randrange(len(fields))]
            elif kind == 3:
                return ""  # empty line
            elif kind == 4:
                i = r.randrange(len(fields))
                fields[i] = values[i] + "," + r.choice(["", "x"])  # embedded comma = extra field
            elif kind == 5:
                return ",".join(fields) + "\r"  # CRLF: \r joins the last field (a value change, not malformed)
            else:
                return ",".join(fields[: max(1, ncols - 3)])
        return ",".join(fields)

    # -------------------------------------------------------------- persons

    def persons(self):
        """Partition (part of) the user pool into planned persons, plus links."""
        r = self.r
        ids = self.users[:]
        r.shuffle(ids)
        groups = []
        i = 0
        while i < len(ids):
            n = r.choices([1, 2, 3, 5, 8], weights=[45, 30, 13, 8, 4])[0]
            groups.append(ids[i:i + n])
            i += n
        links = []
        extra_link_only = ["!", " ", "A0", "AA", "\x01", "0link", "zzz", "L" * 16 + "\x00"[:0] + "0"]
        for g in groups:
            for j in range(1, len(g)):
                links.append((g[j], g[r.randrange(j)]))  # tree: link to an earlier one
            if r.random() < 0.1:
                g_extra = r.choice(extra_link_only)
                links.append((g[0], g_extra) if r.random() < 0.5 else (g_extra, g[0]))
        # long chain (the "chains hundreds of links long" person)
        if r.random() < 0.15:
            n = r.randint(20, 400)
            chain = [f"ch{r.randint(0, 10 ** 6)}_{j}" for j in range(n)]
            r.shuffle(chain)
            for j in range(1, n):
                links.append((chain[j - 1], chain[j]) if r.random() < 0.5 else (chain[j], chain[j - 1]))
            groups.append(chain)
        # giant merged person: a shared device links many persons
        if r.random() < 0.15 and len(groups) > 1:
            dev = r.choice(["device", "D", "dev\r", "😀dev", "Ddevice" * 5])
            for g in r.sample(groups, r.randint(2, len(groups))):
                links.append((dev, r.choice(g)) if r.random() < 0.5 else (r.choice(g), dev))
        # repeats, reversals, self links, upper variants, never-seen ids
        extra = []
        for a, b in links:
            if r.random() < 0.15:
                extra.append((b, a) if r.random() < 0.5 else (a, b))
        links += extra
        for _ in range(r.randint(0, 3)):
            u = r.choice(self.users)
            links.append((u, u))
        for _ in range(r.randint(0, 2)):
            a, b = r.choice(links) if links else (r.choice(self.users), r.choice(self.users))
            links.append((a.upper(), b) if r.random() < 0.5 else (a, b.upper()))
        for _ in range(r.randint(0, 2)):
            links.append((r.choice(self.users), f"ghost{r.randint(0, 9)}"))
        if r.random() < 0.2:  # random cross links (merge planned persons)
            for _ in range(r.randint(1, 3)):
                links.append((r.choice(self.users), r.choice(self.users)))
        if r.random() < 0.3:
            links.append(("user_id_a", "user_id_b"))  # a data row equal to the header
            links.append((r.choice(self.users), r.choice(["user_id_a", "user_id_b"])))
        r.shuffle(links)
        return groups, links

    # -------------------------------------------------------------- timelines

    def session_start(self):
        r = self.r
        x = r.random()
        if x < 0.08:
            return self.ws
        if x < 0.12:
            return self.ws - r.choice([1, 1000, GAP, GAP + 1])
        if x < 0.18:
            return self.we - r.choice([1, 1000, GAP, GAP + 1, 60 * MIN])
        if x < 0.21:
            return self.we + r.choice([0, 1, 1000])
        if x < 0.4:  # just before some midnight
            d = r.randrange(max(1, (self.we - self.ws) // DAY)) + 1
            return self.ws + d * DAY - r.choice([1, 1000, 60_000, GAP, GAP - 1, GAP + 1, 5 * MIN])
        t = self.ws + r.randrange(self.we - self.ws)
        if r.random() < 0.5:
            t -= t % 1000
        return t

    def gap(self):
        r = self.r
        x = r.random()
        if x < 0.15:
            return 0
        if x < 0.27:
            return GAP
        if x < 0.37:
            return GAP + 1
        if x < 0.42:
            return GAP - 1
        if x < 0.45:
            return GAP + 1000
        if x < 0.8:
            return r.choice([1, 999, 1000, 1001]) if r.random() < 0.2 else r.randint(1, 10 * MIN)
        return r.randint(GAP, 6 * HOUR)

    # -------------------------------------------------------------- events

    def price(self):
        r = self.r
        if r.random() < self.maxprice_rate:
            return r.choice(["10000000", "9999999", "0" * r.choice([1, 9000]) + "10000000"])
        if r.random() < self.huge_rate:
            return "0" * r.randint(60, 20000) + str(r.randint(1, 50000))   # valid, very long
        if r.random() < 0.85:
            v = r.choice([r.randint(1, 50000), r.randint(1, 50000), 1, 10_000_000, r.randint(100, 999)])
            s = str(v)
            if r.random() < 0.08:
                s = "0" * r.choice([1, 3, 30]) + s
            return s
        return r.choice(["0", "00", "", "-5", "+100", "19.99", "1e3", " 100", "100 ", "10000001", "99999999",
                         "1" * 25, "0" * 40 + "5", "1" + "0" * 40, "２５０", "٢٥٠", "1_000", "abc", "0x10",
                         "10000000.0", "1000\r"])

    def events(self, groups):
        r = self.r
        rows = []          # (event_id, values tuple of 7 strings, ingest_ms)  -- final rendering later
        anchors = []       # (group index, session start ms) for click planning
        budget = self.n_events
        active = [gi for gi, g in enumerate(groups) if g]
        weights = [r.paretovariate(1.2) for _ in active]
        cum = list(itertools.accumulate(weights))
        while budget > 0 and active:
            gi = r.choices(active, cum_weights=cum)[0]
            g = groups[gi]
            t = self.session_start()
            anchors.append((gi, t))
            nev = r.choice([1, 1, 2, 3, 5, 10, 30])
            if self.n_events > 3000 and r.random() < 0.01:
                nev = r.randint(1000, 20000)   # whale session
            for _ in range(nev):
                if budget <= 0:
                    break
                budget -= 1
                rows.append(self.event_row(g, t))
                if r.random() < 0.08:
                    anchors.append((gi, t))
                t += self.gap()
        return rows, anchors

    def user_for(self, g):
        r = self.r
        x = r.random()
        if x < 0.02:
            return ""
        if x < 0.03:
            return r.choice(g).upper()
        if x < 0.04:
            return r.choice(self.users)
        return g[0] if r.random() < 0.5 else r.choice(g)

    def event_row(self, g, t):
        r = self.r
        uid = self.user_for(g)
        et = r.choices(["view", "cart", "purchase", "bad"], weights=[55, 20, 22, 3])[0]
        if et == "bad":
            et = r.choice(["Purchase", "cart ", "VIEW", "click", "", " view", "purchase\r", "views"])
        x = r.random()
        if x < 0.06:
            pid = r.choice(self.unknown_products)
        elif x < 0.08:
            pid = ""
        else:
            pid = r.choice(list(self.products))
        if et == "purchase" or r.random() < 0.3:
            price = self.price()
        else:
            price = r.choice(["", "", "-5", "abc", "19.99", "0", "1000"])
        ts = self.ts_field(t)
        ingest = self.ingest_for(t)
        return [uid, ts, ingest, et, pid, price], t

    def ingest_for(self, t):
        r = self.r
        x = r.random()
        if x < self.extreme_ingest:
            return r.choice([to_ms(1, 1, 2), to_ms(1970, 1, 1), to_ms(2000, 2, 29, 12), to_ms(9999, 12, 30, 23, 59, 59),
                             t - (t % 1000) - 400 * DAY, t - (t % 1000) + 400 * DAY])
        if x < self.late_rate:
            d = r.choice([LATE, LATE + 1000, LATE + 1, LATE - 1, LATE + 999, LATE - 999, r.randint(LATE, 5 * DAY)])
            mode = r.choice([0, 1])
        elif x < self.late_rate + 0.05:
            d = -r.randint(0, 3 * DAY)
            mode = 0
        else:
            d = r.choice([0, 0, r.randint(0, 10 * MIN), r.randint(0, 6 * HOUR)])
            mode = 0
        v = t + d
        v = v - v % 1000 if mode == 0 else v + (-v) % 1000   # floor or ceil to whole seconds
        return v

    # -------------------------------------------------------------- build all

    def build(self):
        r = self.r
        groups, links = self.persons()
        raw_events, anchors = self.events(groups)

        # assign event ids and duplicates, keeping the equal-ingest-identical guarantee
        by_eid = {}     # eid -> {ingest_ms: values}
        out_events = []  # list of (values with eid, ingest_ms, malformable)

        def emit(eid, vals, ingest_ms):
            seen = by_eid.setdefault(eid, {})
            if ingest_ms in seen:
                vals = seen[ingest_ms]  # must be identical
            else:
                seen[ingest_ms] = vals
            out_events.append([eid] + vals[:2] + [f1(ingest_ms)] + vals[3:])

        for vals, t in raw_events:
            eid = r.choice(self.eid_pool)
            ingest_ms = vals[2]
            emit(eid, vals, ingest_ms)
            if r.random() < self.dup_rate:
                ncopies = r.randint(1, 3)
                kind = r.random()
                for c in range(ncopies):
                    v2 = list(vals)
                    later = ingest_ms + r.choice([0, 1000, r.randint(1, 600) * 1000])
                    if kind < 0.25:   # the earliest copy (the original) is broken; later copies are fine
                        pass
                    if kind < 0.4 and r.random() < 0.5:
                        v2[0] = r.choice(self.users + [""])   # later copy names another user
                    if r.random() < 0.3:
                        v2[1] = self.render_ts(t)             # same instant, different form
                    if r.random() < 0.2:
                        v2[1] = self.ts_field(t, 0.5)
                    if r.random() < 0.2:
                        v2[3] = r.choice(["view", "cart", "purchase", "Purchase"])
                    if later == ingest_ms:
                        v2 = list(vals)
                    emit(eid, v2, later)
                if kind < 0.25 and r.random() < 0.5:
                    # an earlier copy that is invalid / late / another user
                    v0 = list(vals)
                    which = r.randrange(5)
                    if which == 0:
                        v0[1] = self.bad_ts(t)
                    elif which == 1:
                        v0[0] = r.choice(self.users + [""])
                    elif which == 2:
                        v0[3] = "Purchase"
                    elif which == 3:
                        v0[5] = "-1"
                        v0[3] = "purchase"
                    else:
                        v0[1] = self.render_ts(t - LATE - 1000)   # makes the earliest copy late
                    emit(eid, v0, ingest_ms - r.choice([1000, 60_000]))
            if r.random() < 0.03:
                # an earlier copy that is malformed (never the kept one)
                bad = [eid] + vals[:2] + [f1(ingest_ms - 1000)] + vals[3:]
                bad[1] = self.stray_quote(bad[1] or "x")
                out_events.append(("RAW", ",".join(bad)))

        ev_lines = []
        count = {}
        for row in out_events:
            if isinstance(row, list):
                count[(row[0], row[3])] = count.get((row[0], row[3]), 0) + 1
        for row in out_events:
            if isinstance(row, tuple):
                ev_lines.append(row[1])
            else:
                # a CRLF changes the last value: only allowed when no equal-ingest copy exists
                ev_lines.append(self.line(row, 7, crlf_ok=count[(row[0], row[3])] == 1))
        # (no data row equal to the header in events: its ingest_ts would not be valid F1)
        r.shuffle(ev_lines)

        # clicks
        click_vals = {}
        for gi, t in anchors:
            g = groups[gi]
            for _ in range(r.choice([0, 0, 1, 1, 2, 3, 5])):
                x = r.random()
                if x < 0.12:
                    ct = t - WEEK
                elif x < 0.22:
                    ct = t - WEEK - 1
                elif x < 0.27:
                    ct = t - WEEK + 1
                elif x < 0.4:
                    ct = t
                elif x < 0.5:
                    ct = t + 1
                elif x < 0.55:
                    ct = t - 1
                elif x < 0.6:
                    ct = r.choice([self.cs, self.cs - 1, self.cs + 1, self.we - 1, self.we, self.ws])
                else:
                    ct = t - r.randint(0, WEEK)
                    if r.random() < 0.5:
                        ct -= ct % 1000
                n_same = 2 if r.random() < 0.15 else 1
                for _ in range(n_same):     # same-ms pairs
                    cid = r.choice(self.cid_pool)
                    if cid in click_vals:
                        continue
                    xu = r.random()
                    uid = "" if xu < 0.02 else r.choice(g).upper() if xu < 0.03 else r.choice(g)
                    xc = r.random()
                    camp = r.choice(self.unknown_campaigns) if xc < 0.06 else "" if xc < 0.08 else \
                        r.choice(list(self.campaigns))
                    click_vals[cid] = [cid, uid, camp, self.ts_field(ct)]
        cl_lines = []
        for cid, vals in click_vals.items():
            for _ in range(1 + (r.random() < self.dup_rate) * r.randint(1, 3)):
                fields = [self.quote(v) for v in vals]
                if r.random() < self.dirt:
                    cl_lines.append(self.line(vals, 4, crlf_ok=False))   # malformed copy
                else:
                    cl_lines.append(",".join(fields))
        if r.random() < 0.1:
            cl_lines.append(CLICK_HEADER.decode())
        if r.random() < 0.1:
            cl_lines.append(",,c_1,2026-10-03T10:00:00Z")  # empty click_id
        r.shuffle(cl_lines)

        lk_lines = []
        for a, b in links:
            x = r.random()
            if x < 0.03:
                a = ""
            elif x < 0.06:
                b = ""
            lk_lines.append(self.line([a, b], 2))

        files = {}
        files.update(self.split("events", EVENT_HEADER, ev_lines))
        files.update(self.split("clicks", CLICK_HEADER, cl_lines))
        files.update(self.split("links", LINK_HEADER, lk_lines))
        files["products.csv"] = ("product_id,category\n" + "".join(
            f"{p},{c}\n" for p, c in self.products.items())).encode()
        files["campaigns.csv"] = ("campaign_id,channel\n" + "".join(
            f"{c},{ch}\n" for c, ch in self.campaigns.items())).encode()
        files["manifest.json"] = json.dumps({"window_start": f1(self.ws), "window_end": f1(self.we)},
                                            indent=2).encode() + b"\n"
        return files

    def split(self, folder, header, lines):
        r = self.r
        nfiles = r.choice([1, 1, 2, 3, 5, 8]) if not self.many_files else r.randint(20, 160)
        parts = [[] for _ in range(nfiles)]
        for ln in lines:
            parts[r.randrange(nfiles) if r.random() < 0.7 else 0].append(ln)
        for _ in range(r.choice([0, 0, 1, 2])):
            parts.append([])  # header-only files
        out = {}
        for i, p in enumerate(parts):
            body = header.decode()
            if p:
                body += "\n" + "\n".join(p)
            x = r.random()
            if x < 0.15:
                pass                      # no final newline
            elif x < 0.25:
                body += "\n" + "\n" * r.randint(1, 3)   # trailing blank lines (empty = malformed)
            else:
                body += "\n"
            out[f"{folder}/part-{i:05d}.csv"] = body.encode("utf-8")
        if r.random() < 0.06:
            # decoys: only *.csv files are read. Well-formed rows that would change the result if read.
            ncols = header.count(b",") + 1
            good = [ln for ln in lines if ln and '"' not in ln and "\r" not in ln and ln.count(",") == ncols - 1]
            body = header.decode() + "\n"
            for ln in r.sample(good, min(len(good), 5)):
                f = ln.split(",")
                f[0] = "!txt" if folder == "links" else "txt_" + f[0]
                body += ",".join(f) + "\n"
            out[f"{folder}/{r.choice(['README.txt', 'part-99999.csv.bak', 'part-00000.CSV', 'x.csvx'])}"] = body.encode()
        return out


def generate(seed, n_events=None):
    files = Gen(seed, n_events).build()
    if os.environ.get("QA_GEN_NOCTRL"):
        # optional: no C0 control bytes other than \r (\t, \x01 are legal UTF-8 but exotic)
        files = {k: v.replace(b"\x01", b"^A").replace(b"\t", b"^T").replace(b"\x00", b"^@")
                 for k, v in files.items()}
    return files


def write(files, outdir):
    for rel, data in files.items():
        p = os.path.join(outdir, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(data)
    for d in ("events", "clicks", "links"):
        os.makedirs(os.path.join(outdir, d), exist_ok=True)


def main():
    args = sys.argv[1:]
    n = None
    if "--events" in args:
        i = args.index("--events")
        n = int(args[i + 1])
        del args[i:i + 2]
    seed, out = int(args[0]), args[1]
    write(generate(seed, n), out)


if __name__ == "__main__":
    main()
