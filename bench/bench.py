#!/usr/bin/env python3
"""End-to-end benchmark of engine binaries; works natively (macOS) and inside a Linux container.

  bench/bench.py --data data/s7-5m --reps 9 [--out DIR] [--tag TEXT] [--record] \
      VARIANT [VARIANT...]

VARIANT = "label=path/to/binary[,ENV=V,ENV=V...]". Variants are interleaved (A B A B ...), so
drift of the host (other load, page cache) hits them equally. For each run:
  wall      process start to exit, measured around fork/exec .. wait4
  user/sys  CPU seconds from wait4's rusage (the whole process, all threads)
  rss       peak RSS (MB)
  minflt / majflt   page faults from rusage
  phases    the engine's own `[timing]` lines (SE_TIMING=1), wall ms per phase
  exit      wall - (last phase timestamp): process start + teardown not seen by the engine
  instructions / cycles   (macOS only, via /usr/bin/time -l) whole-process counts: the one
            number here that does not move with host load
Prints min and median of N per variant; --record appends one JSON line per variant to
bench/results.jsonl (rendered into bench/SCOREBOARD.md by bench/scoreboard.py).
"""
import argparse, json, os, platform, re, statistics, subprocess, sys, time, datetime

RX = re.compile(r"\[timing\] (.*?)\s+wall\s+([\d.]+) ms\s+user\s+([\d.]+) ms\s+sys\s+([\d.]+) ms\s+\(t=([\d.]+) ms\)")


def run_once(exe, data, out, env):
    e = dict(os.environ)
    e.update(env)
    e["SE_TIMING"] = "1"
    r, w = os.pipe()
    t0 = time.perf_counter()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.dup2(w, 2)
        dn = os.open(os.devnull, os.O_WRONLY)
        os.dup2(dn, 1)
        if sys.platform == "darwin":  # /usr/bin/time -l adds instructions retired and cycles
            os.execve("/usr/bin/time", ["/usr/bin/time", "-l", exe, data, out], e)
        os.execve(exe, [exe, data, out], e)
    os.close(w)
    chunks = []
    while True:
        b = os.read(r, 65536)
        if not b:
            break
        chunks.append(b)
    os.close(r)
    _, status, ru = os.wait4(pid, 0)
    wall = time.perf_counter() - t0
    if status != 0:
        sys.stderr.write(b"".join(chunks).decode(errors="replace"))
        raise SystemExit(f"{exe} exited with status {status}")
    phases, last_t = {}, 0.0
    for line in b"".join(chunks).decode(errors="replace").splitlines():
        m = RX.match(line)
        if m:
            phases[m.group(1)] = (float(m.group(2)), float(m.group(3)), float(m.group(4)))
            last_t = float(m.group(5))
    text = b"".join(chunks).decode(errors="replace")
    mi = re.search(r"(\d+)\s+instructions retired", text)
    mc = re.search(r"(\d+)\s+cycles elapsed", text)
    rss_div = 1 << 20 if sys.platform == "darwin" else 1 << 10  # bytes on macOS, KB on Linux
    return {
        "wall": wall * 1e3,
        "user": ru.ru_utime * 1e3,
        "sys": ru.ru_stime * 1e3,
        "rss": ru.ru_maxrss / rss_div,
        "minflt": ru.ru_minflt,
        "majflt": ru.ru_majflt,
        "phases": phases,
        "exit": wall * 1e3 - last_t,
        "ginstr": int(mi.group(1)) / 1e9 if mi else 0.0,
        "gcycles": int(mc.group(1)) / 1e9 if mc else 0.0,
    }


def env_desc():
    if os.environ.get("BENCH_ENV"):
        return os.environ["BENCH_ENV"]
    return f"{platform.system()}/{platform.machine()} native"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--out", default=None)
    ap.add_argument("--tag", default="")
    ap.add_argument("--record", action="store_true")
    ap.add_argument("--results", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "results.jsonl"))
    ap.add_argument("--pre", default=None, help="shell command run before every single run (e.g. drop caches)")
    ap.add_argument("variants", nargs="+")
    a = ap.parse_args()
    out = a.out or os.environ.get("BENCH_OUT") or "out/perf/bench-out"
    os.makedirs(out, exist_ok=True)
    vs = []
    for v in a.variants:
        label, _, rest = v.partition("=")
        parts = rest.split(",")
        vs.append((label, parts[0], dict(p.split("=", 1) for p in parts[1:] if p)))
    runs = {l: [] for l, _, _ in vs}
    for _ in range(a.reps):
        for label, exe, env in vs:
            if a.pre:
                subprocess.run(a.pre, shell=True, check=False)
            runs[label].append(run_once(exe, a.data, out, env))
    keys = ["wall", "user", "sys", "rss", "minflt", "majflt", "exit", "ginstr", "gcycles"]
    names = []
    for l in runs:
        for r in runs[l]:
            for k in r["phases"]:
                if k not in names:
                    names.append(k)
    for label, exe, env in vs:
        rs = runs[label]
        agg = {k: (min(r[k] for r in rs), statistics.median(r[k] for r in rs)) for k in keys}
        ph = {}
        for n in names:
            xs = [r["phases"][n] for r in rs if n in r["phases"]]
            if xs:
                ph[n] = {
                    "wall": (min(x[0] for x in xs), statistics.median(x[0] for x in xs)),
                    "user": (min(x[1] for x in xs), statistics.median(x[1] for x in xs)),
                    "sys": (min(x[2] for x in xs), statistics.median(x[2] for x in xs)),
                }
        print(f"== {label}  ({exe} {' '.join(f'{k}={v}' for k, v in env.items())})  data={a.data} n={a.reps}")
        print(f"   wall  min {agg['wall'][0]:8.1f}  med {agg['wall'][1]:8.1f} ms   user min {agg['user'][0]:8.1f} med {agg['user'][1]:8.1f}   sys min {agg['sys'][0]:7.1f} med {agg['sys'][1]:7.1f}")
        print(f"   rss {agg['rss'][1]:7.1f} MB   minflt {agg['minflt'][1]:.0f}   majflt {agg['majflt'][1]:.0f}   start+exit overhead min {agg['exit'][0]:.1f} med {agg['exit'][1]:.1f} ms")
        if agg["ginstr"][0]:
            print(f"   instructions min {agg['ginstr'][0]:.3f} G   cycles min {agg['gcycles'][0]:.3f} G (whole process, user+kernel; macOS only)")
        for n, p in ph.items():
            print(f"   {n:<26} wall min {p['wall'][0]:8.1f} med {p['wall'][1]:8.1f}   user min {p['user'][0]:8.1f}   sys min {p['sys'][0]:7.1f}")
        if a.record:
            rec = {
                "date": datetime.datetime.now().strftime("%Y-%m-%d %H:%M"),
                "variant": label,
                "tag": a.tag,
                "exe": exe,
                "envvars": env,
                "data": os.path.basename(os.path.normpath(a.data)),
                "n": a.reps,
                "env": env_desc(),
                "agg": agg,
                "phases": ph,
            }
            with open(a.results, "a") as f:
                f.write(json.dumps(rec) + "\n")


if __name__ == "__main__":
    main()
