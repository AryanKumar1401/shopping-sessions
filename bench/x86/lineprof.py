#!/usr/bin/env python3
"""lineprof.py BINARY < `perf script -F ip`: samples per source line of our own files.
Each sample is charged to the innermost inlined frame that lies in engine/src (so std
helpers inlined into a line count for that line)."""
import collections, subprocess, sys
exe = sys.argv[1]
ips = collections.Counter(l.strip() for l in sys.stdin if l.strip())
addrs = [a for a in ips if not a.startswith("ffff")]
out = subprocess.run(["addr2line", "-a", "-i", "-e", exe] + ["0x" + a for a in addrs], capture_output=True, text=True).stdout
per = collections.Counter(); cur = None; got = False; total = sum(ips.values())
for line in out.splitlines():
    if line.startswith("0x"):
        cur = line[2:].lstrip("0"); got = False
        continue
    if got or cur is None:
        continue
    f = line.split(" ")[0]
    if "/src/" in f and "rustc" not in f and "/library/" not in f and ".cargo" not in f:
        name = f.split("/src/")[-1]
        n = next((ips[a] for a in (cur, cur.rjust(16, "0")) if a in ips), 0) or sum(v for k, v in ips.items() if k.lstrip("0") == cur)
        per[name] += n; got = True
files = collections.Counter()
for k, v in per.items():
    files[k.split(":")[0]] += v
print("samples", total, " by file:", ", ".join(f"{f} {100*v/total:.1f}%" for f, v in files.most_common(8)))
want = sys.argv[2] if len(sys.argv) > 2 else "person.rs"
for k, v in [x for x in per.most_common() if x[0].startswith(want)][:70]:
    print(f"{100*v/total:6.2f}%  {k}")
