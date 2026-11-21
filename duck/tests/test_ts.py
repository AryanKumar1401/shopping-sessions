"""Differential test: fast ts_ms macro vs arithmetic ts_ms_ref vs Python."""
import random, sys, os, datetime as dt
sys.argv = ['x']
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))
import duckdb
import pipeline as P

def py_ts(s):
    import re
    if s is None: return None
    if re.fullmatch(r'[0-9]+', s): 
        t = s.lstrip('0'); return (int(t) if t else 0) if len(t) <= 17 else None
    m = re.fullmatch(r'([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(Z|([+-])([0-9]{2}):([0-9]{2}))', s)
    if not m: return None
    y, mo, d, h, mi, se = map(int, m.groups()[:6])
    if h > 23 or mi > 59 or se > 59: return None
    off = 0
    if m.group(7) != 'Z':
        oh, om = int(m.group(9)), int(m.group(10))
        if oh > 23 or om > 59: return None
        off = (oh * 60 + om) * (1 if m.group(8) == '+' else -1)
    try:
        if y == 0: return 'skip'
        t = dt.datetime(y, mo, d, h, mi, se, tzinfo=dt.timezone.utc)
    except ValueError:
        return None
    return int(t.timestamp()) * 1000 - off * 60000

random.seed(7)
def rnd():
    r = random.random()
    if r < 0.2:
        return str(random.randrange(0, 10**random.randrange(1, 20))).zfill(random.randrange(1, 22))
    y = random.choice([1970, 1999, 2000, 2024, 2025, 2026, 2027, 2028, 2100, 2400, 9999, 1])
    parts = [f"{y:04d}", f"{random.randrange(0, 14):02d}", f"{random.randrange(0, 33):02d}",
             f"{random.randrange(0, 26):02d}", f"{random.randrange(0, 62):02d}", f"{random.randrange(0, 62):02d}"]
    s = f"{parts[0]}-{parts[1]}-{parts[2]}T{parts[3]}:{parts[4]}:{parts[5]}"
    z = random.random()
    if z < 0.4: s += 'Z'
    elif z < 0.9: s += random.choice('+-') + f"{random.randrange(0, 26):02d}:{random.randrange(0, 62):02d}"
    else: s += random.choice(['z', '+0200', '', 'Z ', '.5Z'])
    if random.random() < 0.05:
        i = random.randrange(len(s)); s = s[:i] + random.choice(['٢', '２', ' ', '+', 'a']) + s[i + 1:]
    return s

vals = [rnd() for _ in range(300000)] + ['2028-02-29T00:00:00Z', '2027-02-29T00:00:00Z', '2100-02-29T00:00:00Z',
        '2000-02-29T00:00:00Z', '2026-10-04T01:30:00+02:00', '2026-10-03T10:00:00-00:00', '0' * 5000, '']
c = duckdb.connect()
c.execute(P.MACROS)
c.execute("CREATE TABLE v(s VARCHAR)")
c.executemany("INSERT INTO v VALUES (?)", [(v,) for v in vals[:20000]]) if False else None
import pyarrow as pa
c.register('va', pa.table({'s': vals}))
rows = c.execute("SELECT s, ts_ms(s), ts_ms_ref(s) FROM va").fetchall()
bad = 0
for s, a, b in rows:
    p = py_ts(s)
    if p == 'skip': p = b
    if not (a == b == p):
        bad += 1
        if bad < 20: print('MISMATCH', repr(s), a, b, p)
print('checked', len(rows), 'mismatches', bad, 'valid', sum(1 for r in rows if r[1] is not None))
sys.exit(1 if bad else 0)
