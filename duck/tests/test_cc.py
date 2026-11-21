"""Differential test: SQL connected components vs Python union-find."""
import random, sys, os
sys.argv = ['x']
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))
import duckdb, pyarrow as pa
import pipeline as P

def uf(edges):
    par = {}
    def f(x):
        par.setdefault(x, x)
        while par[x] != x:
            par[x] = par[par[x]]; x = par[x]
        return x
    for a, b in edges:
        ra, rb = f(a), f(b)
        if ra != rb: par[max(ra, rb)] = min(ra, rb)  # str order = byte order for ASCII
    return {x: f(x) for x in list(par)}

random.seed(3)
for trial in range(40):
    n = random.choice([5, 50, 500, 3000])
    ids = [f"u{random.randrange(10**6)}" for _ in range(n)]
    edges = []
    # random chains in random order + random extra edges + self loops
    for _ in range(random.randrange(1, 4)):
        ch = random.sample(ids, min(len(ids), random.randrange(2, 400)))
        edges += [(ch[i], ch[i + 1]) if random.random() < .5 else (ch[i + 1], ch[i]) for i in range(len(ch) - 1)]
    edges += [(random.choice(ids), random.choice(ids)) for _ in range(random.randrange(n))]
    random.shuffle(edges)
    c = duckdb.connect()
    c.register('le', pa.table({'a': [e[0] for e in edges], 'b': [e[1] for e in edges]}))
    c.execute("CREATE TEMP TABLE links_ok AS SELECT * FROM le")
    rounds = P.connected_components(c)
    got = dict(c.execute("SELECT uid, name FROM person_map").fetchall())
    exp = {k: v for k, v in uf(edges).items() if k != v}
    assert got == exp, (trial, len(got), len(exp))
    print('trial', trial, 'n', n, 'edges', len(edges), 'rounds', rounds, 'ok')
print('ALL OK')
