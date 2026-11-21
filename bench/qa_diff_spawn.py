#!/usr/bin/env python3
"""qa/diff.py with the multiprocessing start method forced to "spawn".
On Linux the default is "fork", and the forked workers of qa/diff.py dead-lock (0% CPU) when
they read parquet; macOS already spawns. Same arguments as qa/diff.py."""
import multiprocessing, os, runpy, sys
if __name__ == "__main__":
    multiprocessing.set_start_method("spawn", force=True)
    qa = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "qa")
    sys.path.insert(0, qa)
    sys.argv[0] = os.path.join(qa, "diff.py")
    runpy.run_path(sys.argv[0], run_name="__main__")
