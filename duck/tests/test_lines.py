"""Fuzz: the DuckDB fast line reader (with its pre-scan/fallback) must yield
exactly the lines of an exact Python b'\\n' split (first line of each file
dropped). Empty lines may come back as NULL; both are malformed downstream."""
import os, random, sys, tempfile
sys.argv = ['x']
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))
import duckdb
from concurrent.futures import ThreadPoolExecutor
import pipeline as P

ALPH = ['a', 'b', ',', '"', '\n', '\n', '\n', '\r', '\x00', '\x01', '\x02', '\t', ' ', 'é', '٢', '﻿', '\x0b', '\x0c', '\\', "'", '#']
if os.environ.get('NOCR'): ALPH = [c for c in ALPH if c not in '\r\x01']
random.seed(11)
tmp = tempfile.mkdtemp()
bad = 0
pool = ThreadPoolExecutor(4)
for trial in range(300):
    files = []
    for k in range(random.randrange(1, 4)):
        n = random.randrange(0, 60)
        w = [random.random() for _ in ALPH]
        s = ''.join(random.choices(ALPH, weights=w, k=n))
        p = os.path.join(tmp, f"t{trial}_{k}.csv")
        with open(p, 'wb') as f:
            f.write(s.encode())
        files.append(p)
    con = duckdb.connect()
    rd = P.Reader(con, pool)
    got = rd.build(files, lambda src: con.execute(f"SELECT coalesce(l, '') FROM {src}").fetchall())
    got = sorted(r[0] for r in got)
    exp = sorted(x.decode() for x in P.py_lines(files).column('l').to_pylist() and
                 [b.encode() if isinstance(b, str) else b for b in P.py_lines(files).column('l').to_pylist()])
    if got != exp:
        bad += 1
        if bad < 5:
            print('MISMATCH', trial, [open(f, 'rb').read() for f in files], got, exp)
print('trials 300, mismatches', bad)
sys.exit(1 if bad else 0)
