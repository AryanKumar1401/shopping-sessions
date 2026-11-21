#!/usr/bin/env python3
"""Renders bench/results.jsonl (written by bench.py --record) into bench/SCOREBOARD.md.
Hand-written notes live in bench/NOTES.md and are appended verbatim."""
import json, os
here = os.path.dirname(os.path.abspath(__file__))
rows = [json.loads(l) for l in open(os.path.join(here, "results.jsonl")) if l.strip()]
out = ["# Scoreboard", "",
       "One row per measured variant, appended by `bench/bench.py --record` (see `bench/README.md`).",
       "All numbers are measurements on the environment named in the row; nothing here is extrapolated.",
       "Times in ms: `min / median` of N interleaved runs. `wall` is fork/exec to exit of the engine process",
       "(it does not include `docker run` start-up). `exit` = wall minus the engine's last phase timestamp",
       "(process start + teardown). `instr` (G instructions, whole process) exists on macOS only.",
       "Phases: wall min of links+tables / parse / dedup / person pass.", ""]
envs = []
for r in rows:
    if r["env"] not in envs:
        envs.append(r["env"])
for e in envs:
    out += [f"## {e}", "",
            "| date | variant | data | N | wall | user | sys | rss MB | minflt | majflt | exit | instr G | links / parse / dedup / person |",
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for r in rows:
        if r["env"] != e:
            continue
        a, p = r["agg"], r["phases"]
        f = lambda k, d=0: f"{a[k][0]:.{d}f} / {a[k][1]:.{d}f}"
        ph = " / ".join(f"{p[k]['wall'][0]:.0f}" if k in p else "-" for k in ["links+tables", "parse events+clicks", "dedup", "person pass + user_daily"])
        envs_s = " ".join(f"{k}={v}" for k, v in r["envvars"].items())
        name = r["variant"] + (f" ({r['tag']})" if r["tag"] else "") + (f" `{envs_s}`" if envs_s else "")
        instr = f"{a['ginstr'][0]:.2f}" if a.get("ginstr", [0])[0] else "-"
        out.append(f"| {r['date']} | {name} | {r['data']} | {r['n']} | {f('wall')} | {f('user')} | {f('sys')} | {a['rss'][1]:.0f} | {a['minflt'][1]:.0f} | {a['majflt'][1]:.0f} | {f('exit', 1)} | {instr} | {ph} |")
    out.append("")
notes = os.path.join(here, "NOTES.md")
if os.path.exists(notes):
    out.append(open(notes).read())
open(os.path.join(here, "SCOREBOARD.md"), "w").write("\n".join(out) + "\n")
print(f"{len(rows)} rows")
