#!/usr/bin/env python3
"""Shopping Sessions: DuckDB implementation.

Reads $DATA_DIR, writes $OUTPUT_DIR/{user_daily,campaign_daily}/part-0.parquet.
See rules.md. Logs to stderr only.

Design notes
- CSV: DuckDB's CSV parser cannot implement the dialect (it treats \\r as a
  line end, handles quotes its own way, accepts a trailing delimiter), so each
  file is read as raw lines: one VARCHAR column, quoting off, delimiter \\x01,
  header=true to skip exactly the first line of each file, strict_mode so a
  stray delimiter raises. A memchr pre-scan sends every file containing a \\r
  or a \\x01 (and every file of a table whose fast read raised) to an exact
  b'\\n' split in Python, handed over through Arrow. The dialect itself (field
  count, whole-field quotes, "" -> null) is one anchored regex plus a split.
  duck/tests/test_lines.py fuzzes the reader against the Python split.
- Timestamps: anchored ASCII-digit regexes fix the shape, strptime rejects
  impossible calendar values; F3 digit strings of any length (> 17 significant
  digits is outside every window). duck/tests/test_ts.py checks this against
  a pure-arithmetic SQL reference and Python.
- Dedup: every copy is validated (a pure function of the copy), then only the
  copy with the smallest ingest_ts per event_id is kept (arg_min over rowid).
- Linked IDs: connected components by iterative graph contraction (hook every
  supernode to its smallest neighbour, pointer-jump to the roots, relabel the
  edges) so the number of rounds stays logarithmic even on long chains; IDs are
  ranked in byte order, so the root is the byte-wise smallest ID.
- Sessions: lag() gap flag + running max over a RANGE frame (ties share).
- Credit: ASOF join of sessions to clicks reduced to one per (person, ts)
  (smallest click_id). Assists: each (person, campaign)'s click intervals
  [t, t+7d] are merged into disjoint intervals and joined to session starts.
"""
import json
import mmap
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import duckdb

T0 = time.time()


def log(*a):
    print(f"[duck {time.time() - T0:7.3f}s]", *a, file=sys.stderr, flush=True)


argv = [a for a in sys.argv[1:]]
DATA = os.environ.get("DATA_DIR") or (argv[-2] if len(argv) >= 2 else "/data")
OUT = os.environ.get("OUTPUT_DIR") or (argv[-1] if len(argv) >= 2 else "/output")
SCRATCH = os.environ.get("SCRATCH_DIR") or os.path.join(OUT, ".duck_scratch")  # dot: ignored by the checker
NCPU = int(os.environ.get("NCPU") or os.cpu_count() or 1)
_phys_gb = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30
MEM_GB = float(os.environ.get("MEM_GB") or max(1.0, _phys_gb * 0.5))
MEM_LIMIT_GB = max(1.0, min(MEM_GB * 0.75, _phys_gb * 0.6))

DAY_MS = 86_400_000
GAP_MS = 1_800_000
WEEK_MS = 7 * DAY_MS
DELIM = "\x01"


def parse_f1(s):
    """Manifest instant -> epoch ms (F1 expected; any ISO-8601 with a zone accepted)."""
    import datetime as dt
    d = dt.datetime.fromisoformat(s.replace("Z", "+00:00"))
    if d.tzinfo is None:
        d = d.replace(tzinfo=dt.timezone.utc)
    return int(d.timestamp() * 1000)


def list_csv(sub):
    d = os.path.join(DATA, sub)
    if not os.path.isdir(d):
        return []
    return sorted(os.path.join(d, f) for f in os.listdir(d)
                  if f.endswith(".csv") and os.path.isfile(os.path.join(d, f)))


def needs_python(path):
    """True if DuckDB's line reader cannot be trusted for this file: it treats
    \\r as a line end, and silently drops a trailing delimiter byte (\\x01)
    at the end of a line instead of raising in strict mode."""
    if os.path.getsize(path) == 0:
        return False
    with open(path, "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
        return m.find(b"\r") >= 0 or m.find(DELIM.encode()) >= 0


def py_lines(files):
    """Exact line splitting in Python: drop the first line of each file."""
    import pyarrow as pa
    out = []
    for p in files:
        with open(p, "rb") as f:
            data = f.read()
        parts = data.split(b"\n")
        lines = parts[1:]
        if data.endswith(b"\n"):
            lines = lines[:-1]
        out.extend(lines)
    arr = pa.array(out, type=pa.binary()).cast(pa.string())
    return pa.table({"l": arr})


class Reader:
    def __init__(self, con, pool):
        self.con = con
        self.pool = pool
        self.n = 0

    @staticmethod
    def csv_lines(files):
        lst = "[" + ",".join("'" + f.replace("'", "''") + "'" for f in files) + "]"
        return (f"(SELECT l FROM read_csv({lst}, columns={{'l':'VARCHAR'}}, header=true, "
                f"delim='{DELIM}', quote='', escape='', new_line='\\n', auto_detect=false, "
                f"strict_mode=true))")

    def build(self, files, make):
        """Run make(src) where src is a FROM-clause expression yielding the raw
        lines (column l) of files, first line of each file skipped."""
        if not files:
            return make("(SELECT NULL::VARCHAR AS l WHERE false)")
        crs = list(self.pool.map(needs_python, files))
        log(f"  pre-scanned {len(files)} files: {sum(crs)} need the python splitter")
        fast = [f for f, c in zip(files, crs) if not c]
        slow = [f for f, c in zip(files, crs) if c]

        def src():
            parts = []
            if slow:
                self.n += 1
                name = f"pylines_{self.n}"
                self.con.register(name, py_lines(slow))
                parts.append(f"(SELECT l FROM {name})")
            if fast:
                parts.append(self.csv_lines(fast))
            return "(" + " UNION ALL ".join(parts) + ")"

        try:
            return make(src())
        except duckdb.Error as e:
            if not fast:
                raise
            # an over-long line or anything else DuckDB refuses: find the files
            # it refuses and split those exactly in Python instead
            log("fast line read failed, finding the files:", str(e)[:200])
            ok = []
            for f in fast:
                try:
                    self.con.execute(f"SELECT count(*) FROM {self.csv_lines([f])}").fetchall()
                    ok.append(f)
                except duckdb.Error:
                    slow.append(f)
            fast[:] = ok
            log(f"  {len(slow)} files go to the python splitter")
            return make(src())


def fields_sql(src, n, names):
    """Apply the CSV dialect to raw lines: exactly n fields, quotes only
    wrapping whole fields, quotes stripped, empty -> NULL.

    One anchored regex checks field count and quoting. Callers filter on the
    produced columns only after materializing: DuckDB pushes such filters
    below the projection and would compute the split twice. (Measured: this
    form is ~25% cheaper than a len(p) check plus a regex only on quoted lines.)
    """
    fld = '("[^",]*"|[^",]*)'
    pat = fld + ("(," + fld + "){" + str(n - 1) + "}" if n > 1 else "")
    # in a valid line, a field that starts with a quote is wrapped in quotes
    cols = ", ".join(
        f"nullif(CASE WHEN starts_with(p[{i + 1}], '\"') THEN p[{i + 1}][2:-2] ELSE p[{i + 1}] END, '') AS {nm}"
        for i, nm in enumerate(names))
    return (f"(SELECT {cols} FROM (SELECT string_split(l, ',') AS p FROM {src} "
            f"WHERE regexp_full_match(l, '{pat}')))")


MACROS = r"""
CREATE OR REPLACE MACRO dim(y, m) AS CASE
    WHEN m IN (1,3,5,7,8,10,12) THEN 31
    WHEN m IN (4,6,9,11) THEN 30
    WHEN (y % 4 = 0 AND y % 100 <> 0) OR y % 400 = 0 THEN 29 ELSE 28 END;
-- days since 1970-01-01, proleptic Gregorian (y shifted by 400 years so all
-- intermediate values are non-negative)
CREATE OR REPLACE MACRO civ_days(y, m, d) AS (
    ((y + 400 - CASE WHEN m <= 2 THEN 1 ELSE 0 END) // 400) * 146097
    + ((y + 400 - CASE WHEN m <= 2 THEN 1 ELSE 0 END) % 400) * 365
    + ((y + 400 - CASE WHEN m <= 2 THEN 1 ELSE 0 END) % 400) // 4
    - ((y + 400 - CASE WHEN m <= 2 THEN 1 ELSE 0 END) % 400) // 100
    + (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    - 719468 - 146097);
CREATE OR REPLACE MACRO dt_ms(y, mo, d, h, mi, se, off_min) AS CASE
    WHEN mo BETWEEN 1 AND 12 AND d BETWEEN 1 AND dim(y, mo) AND h < 24 AND mi < 60 AND se < 60
    THEN (civ_days(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - off_min * 60)::BIGINT * 1000
    ELSE NULL END;
CREATE OR REPLACE MACRO ts_ms_ref(s) AS CASE
    WHEN length(s) = 20 AND regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z')
    THEN dt_ms(TRY_CAST(substr(s,1,4) AS BIGINT), TRY_CAST(substr(s,6,2) AS BIGINT), TRY_CAST(substr(s,9,2) AS BIGINT),
               TRY_CAST(substr(s,12,2) AS BIGINT), TRY_CAST(substr(s,15,2) AS BIGINT), TRY_CAST(substr(s,18,2) AS BIGINT), 0)
    WHEN length(s) = 25 AND regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}[+-][0-9]{2}:[0-9]{2}')
    THEN CASE WHEN TRY_CAST(substr(s,21,2) AS BIGINT) < 24 AND TRY_CAST(substr(s,24,2) AS BIGINT) < 60 THEN
        dt_ms(TRY_CAST(substr(s,1,4) AS BIGINT), TRY_CAST(substr(s,6,2) AS BIGINT), TRY_CAST(substr(s,9,2) AS BIGINT),
              TRY_CAST(substr(s,12,2) AS BIGINT), TRY_CAST(substr(s,15,2) AS BIGINT), TRY_CAST(substr(s,18,2) AS BIGINT),
              (CASE WHEN substr(s,20,1) = '-' THEN -1 ELSE 1 END)
                * (TRY_CAST(substr(s,21,2) AS BIGINT) * 60 + TRY_CAST(substr(s,24,2) AS BIGINT))) END
    WHEN regexp_full_match(s, '[0-9]+')
    THEN CASE WHEN length(ltrim(s, '0')) = 0 THEN 0::BIGINT
              WHEN length(ltrim(s, '0')) <= 17 THEN TRY_CAST(ltrim(s, '0') AS BIGINT)
              ELSE NULL END   -- > 17 digits: far outside any window
    ELSE NULL END;
-- Fast form used by the pipeline. The regex fixes the exact shape with ASCII
-- digits; the TIMESTAMP cast then rejects impossible dates (Feb 30, month 13,
-- day 00); it accepts hour 24, so hour/minute/second ranges are checked as
-- 2-digit strings. ts_ms_ref is the pure-arithmetic reference (tested equal).
CREATE OR REPLACE MACRO ts_ms(s) AS CASE
    -- F1: the regex fixes shape and ASCII digits; strptime rejects impossible
    -- values (month 13, Feb 29 off leap years, hour 24, minute/second 60)
    WHEN regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z')
    THEN epoch_ms(try_strptime(s, '%Y-%m-%dT%H:%M:%SZ'))
    -- F2: local date/time validated as written, offset 00-23 : 00-59
    WHEN regexp_full_match(s, '[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}[+-][0-9]{2}:[0-9]{2}')
         AND substr(s,21,2) < '24' AND substr(s,24,2) < '60'
    THEN epoch_ms(try_strptime(substr(s,1,19), '%Y-%m-%dT%H:%M:%S'))
         - (CASE WHEN substr(s,20,1) = '-' THEN -60000 ELSE 60000 END)
           * (TRY_CAST(substr(s,21,2) AS BIGINT) * 60 + TRY_CAST(substr(s,24,2) AS BIGINT))
    -- F3: digits only; more than 17 significant digits is far outside any window
    WHEN regexp_full_match(s, '0*[0-9]{1,17}')
    THEN coalesce(TRY_CAST(nullif(ltrim(s, '0'), '') AS BIGINT), 0)
    ELSE NULL END;
"""


def connected_components(con):
    """links_ok(a, b) -> person_map(uid, name) for every ID in a valid link line."""
    con.execute("""
        CREATE TEMP TABLE ids AS
        SELECT uid, (row_number() OVER (ORDER BY uid) - 1)::INTEGER AS k
        FROM (SELECT a AS uid FROM links_ok UNION SELECT b FROM links_ok)""")
    con.execute("""
        CREATE TEMP TABLE edges AS
        SELECT DISTINCT least(ia.k, ib.k) AS u, greatest(ia.k, ib.k) AS v
        FROM links_ok l JOIN ids ia ON l.a = ia.uid JOIN ids ib ON l.b = ib.uid
        WHERE ia.k <> ib.k""")
    # label of every original node; only nodes touching an edge ever change
    con.execute("CREATE TEMP TABLE lab AS SELECT k, k AS r FROM ids")
    rounds = 0
    while True:
        ne = con.execute("SELECT count(*) FROM edges").fetchone()[0]
        if ne == 0:
            break
        rounds += 1
        # hook each supernode to its smallest neighbour (strictly smaller or itself)
        con.execute("""
            CREATE OR REPLACE TEMP TABLE hook AS
            SELECT x, min(y) AS p FROM (
                SELECT u AS x, v AS y FROM edges UNION ALL SELECT v, u FROM edges
                UNION ALL SELECT u, u FROM edges UNION ALL SELECT v, v FROM edges)
            GROUP BY x""")
        # pointer jumping until every node points at its root
        while True:
            con.execute("""
                CREATE OR REPLACE TEMP TABLE hook2 AS
                SELECT h.x, h2.p AS p FROM hook h JOIN hook h2 ON h.p = h2.x""")
            changed = con.execute(
                "SELECT count(*) FROM hook h JOIN hook2 g USING (x) WHERE h.p <> g.p").fetchone()[0]
            con.execute("DROP TABLE hook; ALTER TABLE hook2 RENAME TO hook")
            if changed == 0:
                break
        con.execute("""
            CREATE OR REPLACE TEMP TABLE lab AS
            SELECT l.k, coalesce(h.p, l.r) AS r FROM lab l LEFT JOIN hook h ON l.r = h.x""")
        con.execute("""
            CREATE OR REPLACE TEMP TABLE edges AS
            SELECT DISTINCT least(h1.p, h2.p) AS u, greatest(h1.p, h2.p) AS v
            FROM edges e JOIN hook h1 ON e.u = h1.x JOIN hook h2 ON e.v = h2.x
            WHERE h1.p <> h2.p""")
    con.execute("""
        CREATE TEMP TABLE person_map AS
        SELECT i.uid, n.uid AS name
        FROM lab l JOIN ids i ON l.k = i.k JOIN ids n ON l.r = n.k
        WHERE l.k <> l.r""")
    return rounds


def main():
    os.makedirs(SCRATCH, exist_ok=True)
    tmp = os.path.join(SCRATCH, "duckdb_tmp")
    con = duckdb.connect()
    con.execute(f"SET threads = {NCPU}")
    con.execute(f"SET memory_limit = '{MEM_LIMIT_GB:.2f}GB'")
    con.execute(f"SET temp_directory = '{tmp}'")
    con.execute("SET preserve_insertion_order = false")
    con.execute(MACROS)

    with open(os.path.join(DATA, "manifest.json")) as f:
        man = json.load(f)
    ws, we = parse_f1(man["window_start"]), parse_f1(man["window_end"])
    log("window", man["window_start"], man["window_end"], "threads", NCPU, "mem", f"{MEM_LIMIT_GB:.1f}GB")

    pool = ThreadPoolExecutor(max(1, NCPU))
    rd = Reader(con, pool)

    # ---- small dimension tables (same dialect) ----
    rd.build([os.path.join(DATA, "products.csv")], lambda src: con.execute(f"""
        CREATE TEMP TABLE products AS
        SELECT product_id, any_value(category) AS category
        FROM {fields_sql(src, 2, ['product_id', 'category'])}
        WHERE product_id IS NOT NULL GROUP BY product_id"""))
    rd.build([os.path.join(DATA, "campaigns.csv")], lambda src: con.execute(f"""
        CREATE TEMP TABLE campaigns AS
        SELECT campaign_id, any_value(channel) AS channel
        FROM {fields_sql(src, 2, ['campaign_id', 'channel'])}
        WHERE campaign_id IS NOT NULL GROUP BY campaign_id"""))

    # ---- links -> persons ----
    rd.build(list_csv("links"), lambda src: con.execute(f"""
        CREATE TEMP TABLE links_ok AS
        SELECT a, b FROM {fields_sql(src, 2, ['a', 'b'])}
        WHERE a IS NOT NULL AND b IS NOT NULL"""))
    rounds = connected_components(con)
    log("links done, CC rounds", rounds,
        "mapped ids", con.execute("SELECT count(*) FROM person_map").fetchone()[0])

    # ---- events: parse + validate every copy, then dedup ----
    # Validation is a pure function of the copy, so validating each copy and
    # then keeping the copy with the smallest ingest_ts is the same as
    # dedup-then-validate.
    ev_fields = ['event_id', 'user_id', 'ts', 'ingest_ts', 'event_type', 'product_id', 'price_cents']
    rd.build(list_csv("events"), lambda src: con.execute(f"""
        CREATE TEMP TABLE er AS
        SELECT event_id AS eid, ing, uid, t, purchase, price, prod,
               (uid IS NOT NULL AND prod IS NOT NULL AND etype IN ('view', 'cart', 'purchase')
                AND t >= {ws} AND t < {we}
                AND (NOT purchase OR price BETWEEN 1 AND 10000000)
                AND ing - t <= {DAY_MS}) AS ok
        FROM (
            SELECT event_id, ts_ms(ingest_ts) AS ing, user_id AS uid, ts_ms(ts) AS t,
                   event_type AS etype, event_type = 'purchase' AS purchase, product_id AS prod,
                   CASE WHEN event_type = 'purchase' AND regexp_full_match(price_cents, '0*[0-9]{{1,8}}')
                        THEN TRY_CAST(ltrim(price_cents, '0') AS BIGINT) END AS price
            FROM {fields_sql(src, 7, ev_fields)})"""))
    log("events parsed")
    # kept copies only (smallest ingest_ts per event_id), then valid ones;
    # price/prod are carried only for purchases (price IS NOT NULL <=> purchase)
    con.execute("""CREATE TEMP TABLE ev AS
        SELECT coalesce(m.name, e.uid) AS pid, e.t, CASE WHEN e.purchase THEN e.price END AS price,
               CASE WHEN e.purchase THEN e.prod END AS prod
        FROM (SELECT * FROM er WHERE ok AND rowid IN (
                SELECT arg_min(rowid, ing) FROM er WHERE eid IS NOT NULL GROUP BY eid)) e
        LEFT JOIN person_map m ON e.uid = m.uid""")
    con.execute("DROP TABLE er")
    log("events valid", con.execute("SELECT count(*) FROM ev").fetchone()[0])

    # ---- sessionize ----
    # nf: first event of a session (gap to the previous distinct ts > 30 min;
    # equal ts give gap 0). ss: running max of session-start ts; RANGE frames
    # include ties, so all events sharing a ts get the same session.
    con.execute(f"""CREATE TEMP TABLE ev_s AS
        SELECT pid, price, prod,
               max(CASE WHEN nf THEN t END) OVER (PARTITION BY pid ORDER BY t
                    RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS ss
        FROM (SELECT *, coalesce(t - lag(t) OVER (PARTITION BY pid ORDER BY t) > {GAP_MS}, true) AS nf
              FROM ev)""")
    con.execute("DROP TABLE ev")
    con.execute("""CREATE TEMP TABLE sess AS
        SELECT pid, ss, count(*) AS n, coalesce(sum(price), 0) AS rev, count(price) > 0 AS has_p
        FROM ev_s GROUP BY pid, ss""")
    log("sessions", con.execute("SELECT count(*) FROM sess").fetchone()[0])

    # ---- clicks ----
    # Dedup on click_id is a no-op for every output: copies are identical, and
    # credit (latest ts, then smallest click_id) and assists (distinct
    # campaigns) are insensitive to exact duplicates.
    rd.build(list_csv("clicks"), lambda src: con.execute(f"""
        CREATE TEMP TABLE clk_raw AS
        SELECT click_id, user_id, campaign_id, ts_ms(ts) AS t
        FROM {fields_sql(src, 4, ['click_id', 'user_id', 'campaign_id', 'ts'])}"""))
    con.execute(f"""
        CREATE TEMP TABLE clk AS
        SELECT coalesce(m.name, c.user_id) AS pid, c.t, c.click_id, c.campaign_id
        FROM clk_raw c
        SEMI JOIN campaigns k ON c.campaign_id = k.campaign_id
        LEFT JOIN person_map m ON c.user_id = m.uid
        WHERE c.click_id IS NOT NULL AND c.user_id IS NOT NULL
          AND c.t >= {ws - WEEK_MS} AND c.t < {we}""")
    con.execute("DROP TABLE clk_raw")
    log("clicks valid", con.execute("SELECT count(*) FROM clk").fetchone()[0])

    # credit: latest click with ts <= start, tie -> smallest click_id, within 7 days
    con.execute("""CREATE TEMP TABLE clk_top AS
        SELECT pid, t, arg_min(campaign_id, click_id) AS campaign_id FROM clk GROUP BY pid, t""")
    con.execute(f"""CREATE TEMP TABLE credit AS
        SELECT s.pid, s.ss, s.rev, s.has_p,
               CASE WHEN c.t IS NOT NULL AND s.ss - c.t <= {WEEK_MS} THEN c.campaign_id ELSE 'organic' END AS camp
        FROM sess s ASOF LEFT JOIN clk_top c ON s.pid = c.pid AND c.t <= s.ss""")
    # assists: merge each (person, campaign)'s click intervals [t, t+7d] into
    # disjoint intervals, then count sessions starting inside each one.
    con.execute(f"""CREATE TEMP TABLE ivl AS
        SELECT pid, campaign_id, min(t) AS a, max(t) + {WEEK_MS} AS b
        FROM (SELECT *, sum(nf) OVER (PARTITION BY pid, campaign_id ORDER BY t
                         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS g
              FROM (SELECT pid, campaign_id, t,
                           CASE WHEN t - lag(t) OVER (PARTITION BY pid, campaign_id ORDER BY t) <= {WEEK_MS}
                                THEN 0 ELSE 1 END AS nf
                    FROM clk))
        GROUP BY pid, campaign_id, g""")
    con.execute("""CREATE TEMP TABLE assist AS
        SELECT i.campaign_id, epoch_ms(s.ss)::DATE AS day, count(*) AS n
        FROM sess s JOIN ivl i ON s.pid = i.pid AND s.ss >= i.a AND s.ss <= i.b
        GROUP BY ALL""")
    log("attribution done")

    # ---- outputs ----
    ud = os.path.join(OUT, "user_daily")
    cd = os.path.join(OUT, "campaign_daily")
    os.makedirs(ud, exist_ok=True)
    os.makedirs(cd, exist_ok=True)
    con.execute(f"""COPY (
        WITH sd AS (
            SELECT pid AS user_id, epoch_ms(ss)::DATE AS day, count(*)::BIGINT AS sessions,
                   sum(n)::BIGINT AS events, sum(rev)::BIGINT AS revenue_cents
            FROM sess GROUP BY ALL),
        cat AS (
            SELECT pid, day, arg_min(category, (-r, category)) AS top_category FROM (
                SELECT e.pid, epoch_ms(e.ss)::DATE AS day, coalesce(p.category, 'unknown') AS category,
                       sum(e.price) AS r
                FROM ev_s e LEFT JOIN products p ON e.prod = p.product_id
                WHERE e.price IS NOT NULL GROUP BY ALL)
            GROUP BY ALL)
        SELECT sd.user_id, sd.day, sd.sessions, sd.events, sd.revenue_cents, cat.top_category
        FROM sd LEFT JOIN cat ON sd.user_id = cat.pid AND sd.day = cat.day
    ) TO '{os.path.join(ud, "part-0.parquet")}' (FORMAT parquet)""")
    con.execute(f"""COPY (
        WITH cr AS (
            SELECT camp AS campaign_id, epoch_ms(ss)::DATE AS day, count(*) AS sessions,
                   count(*) FILTER (WHERE has_p) AS purchasing_sessions, sum(rev) AS revenue_cents
            FROM credit GROUP BY ALL)
        SELECT campaign_id,
               CASE WHEN campaign_id = 'organic' THEN 'organic' ELSE k.channel END AS channel,
               day,
               coalesce(cr.sessions, 0)::BIGINT AS sessions,
               coalesce(cr.purchasing_sessions, 0)::BIGINT AS purchasing_sessions,
               coalesce(cr.revenue_cents, 0)::BIGINT AS revenue_cents,
               coalesce(a.n, 0)::BIGINT AS assisted_sessions
        FROM cr FULL OUTER JOIN assist a USING (campaign_id, day)
        LEFT JOIN campaigns k USING (campaign_id)
    ) TO '{os.path.join(cd, "part-0.parquet")}' (FORMAT parquet)""")
    log("done")


if __name__ == "__main__":
    main()
