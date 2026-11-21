#!/usr/bin/env python3
"""
Differential fuzzing: oracle vs every available implementation.

    .venv/bin/python qa/diff.py [--seeds A:B] [--jobs J] [--impl NAME[=CMD]]... [--no-shrink]
                                [--save] [--quiet]

For every seed, qa/gen.py makes a dataset; the oracle and each implementation
run on it; both output tables are compared exactly (order-insensitive, same
loaders as scripts/run_fixtures.py). On a mismatch the input is shrunk
(delta debugging over lines, then files, then field simplification) and the
minimal reproducer is written to out/qa/repro/<impl>_<seed>/{input,expected,
actual.txt}. With --save, also copy it to fixtures_extra/qa_<n>_<impl>_<seed>/
(only do that after checking the oracle by hand against rules.md).

Exit status: 0 if every run matched, 1 otherwise.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ProcessPoolExecutor, as_completed
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "qa"))

import polars as pl  # noqa: E402
from run_fixtures import SCHEMAS, load_actual, rows  # noqa: E402

import gen  # noqa: E402

PY = str(ROOT / ".venv/bin/python")
IMPLS = {
    "oracle": [PY, str(ROOT / "oracle/oracle.py")],
    "duck": [PY, str(ROOT / "duck/pipeline.py")],
    "rust": [str(ROOT / "engine/target/release/shopping-engine")],
}
SCRATCH = ROOT / "out" / "qa"
TIMEOUT = 120


# ------------------------------------------------------------------ running

def run_impl(cmd, data_dir, out_dir, timeout=TIMEOUT):
    """Returns (rows_by_table or None, error string or None)."""
    shutil.rmtree(out_dir, ignore_errors=True)
    os.makedirs(out_dir, exist_ok=True)
    scratch = os.path.join(out_dir, "_scratch")
    env = dict(os.environ, DATA_DIR=str(data_dir), OUTPUT_DIR=str(out_dir), SCRATCH_DIR=scratch,
               NCPU=os.environ.get("QA_IMPL_NCPU", "2"), MEM_GB=os.environ.get("QA_IMPL_MEM_GB", "2"),
               RAYON_NUM_THREADS=os.environ.get("QA_IMPL_NCPU", "2"))
    try:
        p = subprocess.run(cmd + [str(data_dir), str(out_dir)], env=env, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, "timeout"
    if p.returncode != 0:
        return None, f"exit {p.returncode}: {p.stderr.decode('utf-8', 'replace')[-1500:]}"
    res = {}
    for table, schema in SCHEMAS.items():
        try:
            res[table] = rows(load_actual(os.path.join(out_dir, table), schema))
        except Exception as e:  # noqa: BLE001
            return None, f"bad output {table}: {e}"
    return res, None


def outcome(cmd, data_dir, out_dir):
    res, err = run_impl(cmd, data_dir, out_dir)
    if err:
        sys.stderr.write(f"[{' '.join(map(str, cmd))[-60:]}] on {data_dir}: {err[:3000]}\n")
        return ("ERR", err.split(":")[0])
    return ("OK", res)


def differs(a, b):
    """a, b are outcome() results. Returns a signature string if they differ, else None."""
    if a[0] == "ERR":
        return None  # oracle failure: not a finding about the impl (reported separately)
    if b[0] == "ERR":
        return "error:" + b[1]
    sig = []
    for t in SCHEMAS:
        if a[1][t] != b[1][t]:
            sig.append(t)
    return "mismatch:" + "+".join(sig) if sig else None


def table_diff(a, b, limit=12):
    out = []
    for t in SCHEMAS:
        sa, sb = set(a[t]), set(b[t])
        only_a = sorted(sa - sb, key=repr)[:limit]
        only_b = sorted(sb - sa, key=repr)[:limit]
        if len(a[t]) != len(sa) or len(b[t]) != len(sb):
            out.append(f"{t}: duplicate rows (oracle {len(a[t]) - len(sa)}, impl {len(b[t]) - len(sb)})")
        if only_a or only_b:
            out.append(f"{t}: only in oracle: {only_a}\n{t}: only in impl:   {only_b}")
    return "\n".join(out)


# ------------------------------------------------------------------ spec guard

def _field(raw):
    if raw.startswith(b'"'):
        if len(raw) < 2 or not raw.endswith(b'"') or b'"' in raw[1:-1]:
            return None
        return raw[1:-1]
    return None if b'"' in raw else raw


def _rows(data, ncols):
    parts = data.split(b"\n")
    if parts and parts[-1] == b"":
        parts.pop()
    for line in parts[1:]:
        if not line:
            continue
        f = line.split(b",")
        if len(f) != ncols:
            continue
        v = [_field(x) for x in f]
        if None in v:
            continue
        yield v


def _is_f1(b):
    import re
    if not re.fullmatch(rb"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", b):
        return False
    import datetime
    try:
        datetime.datetime.strptime(b.decode(), "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        return False
    return True


def spec_ok(files):
    """True if the dataset stays within rules.md's guarantees (so a diff is a real bug)."""
    if "manifest.json" not in files or "products.csv" not in files or "campaigns.csv" not in files:
        return False
    try:
        for d in files.values():
            d.decode("utf-8")
    except UnicodeDecodeError:
        return False
    ev = {}
    cl = {}
    for path, data in files.items():
        if not path.endswith(".csv") or "/" not in path:
            continue
        folder = path.split("/")[0]
        if not data.split(b"\n")[0]:
            return False  # every CSV has a header row
        if folder == "events":
            for v in _rows(data, 7):
                if v[0] == b"":
                    continue
                if not _is_f1(v[3]):
                    return False
                key = (v[0], v[3])
                if ev.setdefault(key, v) != v:
                    return False
        elif folder == "clicks":
            for v in _rows(data, 4):
                if v[0] == b"":
                    continue
                if cl.setdefault(v[0], v) != v:
                    return False
    for name in ("products.csv", "campaigns.csv"):
        lines = files[name].split(b"\n")
        if lines[-1] == b"":
            lines.pop()
        ids = set()
        for ln in lines[1:]:
            f = ln.split(b",")
            if len(f) != 2 or b'"' in ln or not f[0] or f[0] in ids or b"\r" in ln:
                return False
            ids.add(f[0])
            if name == "products.csv" and f[1] == b"unknown":
                return False
            if name == "campaigns.csv" and f[0] == b"organic":
                return False
    return True


# ------------------------------------------------------------------ shrinking

class Tester:
    def __init__(self, impl_cmd, workdir, signature):
        self.cmd = impl_cmd
        self.work = Path(workdir)
        self.sig = signature
        self.n = 0
        self.cache = {}

    def check(self, files):
        """True if `files` still reproduces the same kind of failure."""
        if not spec_ok(files):
            return False
        key = hash(tuple(sorted(files.items())))
        if key in self.cache:
            return self.cache[key]
        self.n += 1
        d = self.work / "cand"
        shutil.rmtree(d, ignore_errors=True)
        gen.write(files, d)
        a = outcome(IMPLS["oracle"], d, self.work / "o_oracle")
        b = outcome(self.cmd, d, self.work / "o_impl")
        s = differs(a, b)
        ok = s is not None and (s == self.sig or (s.startswith("mismatch") and self.sig.startswith("mismatch")))
        self.cache[key] = ok
        return ok


def split_file(data):
    parts = data.split(b"\n")
    return parts[0], parts[1:]   # header, rest (rest may end with b"" meaning final newline)


def join_file(header, rest):
    return b"\n".join([header] + rest)


def ddmin(items, test):
    """Classic ddmin: smallest subset of items (list) for which test(subset) holds."""
    n = 2
    while len(items) >= 2:
        chunk = max(1, len(items) // n)
        subsets = [items[i:i + chunk] for i in range(0, len(items), chunk)]
        reduced = False
        for i in range(len(subsets)):
            complement = [x for j, s in enumerate(subsets) if j != i for x in s]
            if test(complement):
                items = complement
                n = max(n - 1, 2)
                reduced = True
                break
        if not reduced:
            if n >= len(items):
                break
            n = min(len(items), n * 2)
    if len(items) == 1 and test([]):
        return []
    return items


def shrink(files, tester, log=lambda *a: None):
    files = dict(files)
    # 1. drop whole csv files (never products/campaigns/manifest)
    names = [p for p in files if "/" in p]
    keep = ddmin(names, lambda sub: tester.check({k: v for k, v in files.items() if "/" not in k or k in sub}))
    files = {k: v for k, v in files.items() if "/" not in k or k in keep}
    log(f"  files -> {len(keep)}")
    # 2. ddmin over all data lines of all csv files (incl. products/campaigns lines)
    units = []
    split = {}
    for p, d in files.items():
        if p.endswith(".csv"):
            h, rest = split_file(d)
            split[p] = (h, rest)
            units += [(p, i) for i in range(len(rest))]

    def build(sub):
        chosen = {}
        for p, i in sub:
            chosen.setdefault(p, []).append(i)
        out = dict(files)
        for p, (h, rest) in split.items():
            out[p] = join_file(h, [rest[i] for i in sorted(chosen.get(p, []))])
        return out

    units = ddmin(units, lambda sub: tester.check(build(sub)))
    files = build(units)
    log(f"  lines -> {len(units)}")
    # 3. field simplification: unquote, shorten ids, simplify timestamps
    files = simplify(files, tester)
    # 4. a second pass over lines (simplification may have made more removable)
    split = {p: split_file(d) for p, d in files.items() if p.endswith(".csv")}
    units = [(p, i) for p, (h, rest) in split.items() for i in range(len(rest))]
    units = ddmin(units, lambda sub: tester.check(build(sub)))
    files = build(units)
    log(f"  final lines {len(units)}, tests {tester.n}")
    return files


def simplify(files, tester):
    import re
    # (a) global token renames: every distinct value in id-like positions to something short
    def tokens():
        seen = []
        for p, d in files.items():
            if not p.endswith(".csv") or "/" not in p:
                continue
            for ln in d.split(b"\n")[1:]:
                for f in ln.split(b","):
                    v = f[1:-1] if len(f) >= 2 and f.startswith(b'"') and f.endswith(b'"') else f
                    if v and v not in seen and len(v) > 2:
                        seen.append(v)
        return seen

    # (b) per-field unquote / replacement; try cheap transformations, keep those that preserve failure
    def try_replace(old, new):
        nonlocal files
        if old == new:
            return
        cand = {}
        for p, d in files.items():
            if p.endswith(".csv") or p == "manifest.json":
                cand[p] = replace_field_values(d, old, new) if p.endswith(".csv") else d
            else:
                cand[p] = d
        if cand != files and tester.check(cand):
            files = cand

    # unquote everything at once, then per file
    cand = {p: (unquote_all(d) if p.endswith(".csv") else d) for p, d in files.items()}
    if cand != files and tester.check(cand):
        files = cand
    else:
        for p in [p for p in files if p.endswith(".csv")]:
            c2 = dict(files)
            c2[p] = unquote_all(files[p])
            if c2 != files and tester.check(c2):
                files = c2
    # timestamps: F3/F2 -> F1 when whole seconds
    for tok in tokens():
        if re.fullmatch(rb"\d+", tok) and len(tok) <= 16:
            ms = int(tok)
            if ms % 1000 == 0 and 0 < ms < 10 ** 14:
                try_replace(tok, gen.f1(ms).encode())
        elif re.fullmatch(rb"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}", tok):
            import datetime
            try:
                t = datetime.datetime.strptime(tok.decode(), "%Y-%m-%dT%H:%M:%S%z")
                try_replace(tok, t.astimezone(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ").encode())
            except (ValueError, OverflowError):
                pass
    # ids: rename long ones to short ones (order-preserving rename is not guaranteed; the test decides)
    short = [b"u%d" % i for i in range(1, 200)]
    used = set(tokens())
    for tok in tokens():
        if re.fullmatch(rb"\d+", tok) or b"T" in tok and b":" in tok:
            continue
        if len(tok) > 3:
            for s in short:
                if s not in used:
                    before = files
                    try_replace(tok, s)
                    if files is not before:
                        used.add(s)
                    break
    return files


def unquote_all(data):
    lines = data.split(b"\n")
    out = [lines[0]]
    for ln in lines[1:]:
        fs = ln.split(b",")
        vals = [_field(f) for f in fs]
        if None in vals:
            out.append(ln)
        else:
            out.append(b",".join(vals))
    return b"\n".join(out)


def replace_field_values(data, old, new):
    lines = data.split(b"\n")
    out = [lines[0]]
    for ln in lines[1:]:
        fs = ln.split(b",")
        for i, f in enumerate(fs):
            if f == old:
                fs[i] = new
            elif f == b'"' + old + b'"':
                fs[i] = b'"' + new + b'"'
        out.append(b",".join(fs))
    return b"\n".join(out)


# ------------------------------------------------------------------ fixtures

def write_expected_csv(res, folder):
    """Write oracle rows as fixture CSVs, then verify the round trip through load_expected."""
    from run_fixtures import load_expected
    os.makedirs(folder, exist_ok=True)
    for t, schema in SCHEMAS.items():
        cols = list(schema)
        df = pl.DataFrame(res[t], schema={c: (pl.Int64 if v == pl.Int128 else v) for c, v in schema.items()},
                          orient="row")
        path = os.path.join(folder, f"{t}.csv")
        df.with_columns(pl.col("day").dt.strftime("%Y-%m-%d")).select(cols).write_csv(path)
        back = rows(load_expected(path, schema))
        if back != res[t]:
            raise RuntimeError(f"CSV round trip lost information for {path}")


def save_repro(files, impl, seed, oracle_res, impl_out, dest):
    shutil.rmtree(dest, ignore_errors=True)
    gen.write(files, Path(dest) / "input")
    write_expected_csv(oracle_res, Path(dest) / "expected")
    with open(Path(dest) / "actual.txt", "w") as f:
        f.write(f"impl={impl} seed={seed}\n")
        if impl_out[0] == "ERR":
            f.write(f"impl error: {impl_out[1]}\n")
        else:
            f.write(table_diff(oracle_res, impl_out[1]) + "\n")


# ------------------------------------------------------------------ driver

def smoke(name, cmd):
    """An implementation is 'available' if it runs and passes fixtures/E1."""
    exe = cmd[-1] if cmd[0] == PY else cmd[0]
    if not (os.path.exists(exe) or shutil.which(exe)):
        return False
    with tempfile.TemporaryDirectory(dir=SCRATCH) as t:
        res, err = run_impl(cmd, ROOT / "fixtures/E1/input", Path(t) / "o", timeout=60)
    return err is None


def one_seed(seed, impls, do_shrink, n_events=None):
    work = SCRATCH / "w" / f"{os.getpid()}_{seed}"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    try:
        files = gen.generate(seed, n_events)
        assert spec_ok(files), f"generator produced an out-of-spec dataset for seed {seed}"
        d = work / "data"
        gen.write(files, d)
        a = outcome(IMPLS["oracle"], d, work / "o_oracle")
        if a[0] == "ERR":
            return {"seed": seed, "oracle_error": a[1], "results": {}}
        results = {}
        for name, cmd in impls.items():
            b = outcome(cmd, d, work / f"o_{name}")
            sig = differs(a, b)
            if sig is not None:
                # confirm (an implementation may be mid-rebuild): wait, run again
                time.sleep(float(os.environ.get("QA_RETRY_WAIT", "8")))
                b = outcome(cmd, d, work / f"o_{name}")
                sig = differs(a, b)
            if sig is None:
                results[name] = None
                continue
            info = {"sig": sig, "repro": None}
            if do_shrink:
                t = Tester(cmd, work / f"shrink_{name}", sig)
                small = shrink(files, t)
                sd = work / "small"
                shutil.rmtree(sd, ignore_errors=True)
                gen.write(small, sd)
                a2 = outcome(IMPLS["oracle"], sd, work / "o2_oracle")
                b2 = outcome(cmd, sd, work / "o2_impl")
                dest = SCRATCH / "repro" / f"{name}_{seed}"
                save_repro(small, name, seed, a2[1], b2, dest)
                info["repro"] = str(dest)
                info["tests"] = t.n
            results[name] = info
        return {"seed": seed, "results": results}
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", default="0:200")
    ap.add_argument("--jobs", type=int, default=5)
    ap.add_argument("--impl", action="append", default=[],
                    help="NAME (from the built-in table) or NAME=CMD (shell-split)")
    ap.add_argument("--no-shrink", action="store_true")
    ap.add_argument("--events", default=None, help="N or A:B (per-seed random size in [A, B])")
    ap.add_argument("--log", default=None, help="append one JSON line per seed")
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--stop-after", type=int, default=0, help="stop after this many failing seeds")
    args = ap.parse_args()
    SCRATCH.mkdir(parents=True, exist_ok=True)
    import shlex
    impls = {}
    for spec in args.impl or ["duck", "rust"]:
        if "=" in spec:
            name, cmd = spec.split("=", 1)
            impls[name] = shlex.split(cmd)
        else:
            impls[spec] = IMPLS[spec]
    avail = {}
    for name, cmd in impls.items():
        if smoke(name, cmd):
            avail[name] = cmd
        else:
            print(f"skip {name}: not available (missing, crashes, or fails fixtures/E1)", file=sys.stderr)
    if not avail:
        print("no implementation available", file=sys.stderr)
        return 2
    a, b = map(int, args.seeds.split(":"))
    seeds = range(a, b)
    t0 = time.time()
    fails = 0
    done = 0
    counts = {n: 0 for n in avail}
    logf = open(args.log, "a") if args.log else None
    with ProcessPoolExecutor(args.jobs) as ex:
        def size(s):
            if not args.events:
                return None
            if ":" in args.events:
                lo, hi = map(int, args.events.split(":"))
                import random
                return random.Random(s * 7919).randint(lo, hi)
            return int(args.events)
        futs = {ex.submit(one_seed, s, avail, not args.no_shrink, size(s)): s for s in seeds}
        for f in as_completed(futs):
            r = f.result()
            done += 1
            if logf:
                logf.write(json.dumps(r) + "\n")
                logf.flush()
            if "oracle_error" in r:
                print(f"seed {r['seed']}: ORACLE ERROR {r['oracle_error']}", flush=True)
                fails += 1
                continue
            for name, info in r["results"].items():
                counts[name] += 1
                if info:
                    fails += 1
                    print(f"seed {r['seed']}: {name} {info['sig']} repro={info['repro']}", flush=True)
            if not args.quiet and done % 50 == 0:
                print(f"... {done}/{len(seeds)} seeds, {fails} failures, {time.time() - t0:.0f}s", flush=True)
            if args.stop_after and fails >= args.stop_after:
                for g in futs:
                    g.cancel()
                break
    print(f"seeds run per impl: {counts}; failures: {fails}; {time.time() - t0:.0f}s")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
