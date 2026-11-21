#!/bin/sh
# Copy the working tree's engine/ sources and bench/ to the benchmark VM (~/ss) and build there.
#   bench/vm_sync.sh LABEL ['extra RUSTFLAGS']   -> ~/ss/out/perf/LABEL on the VM (default flags: -C target-cpu=x86-64-v3)
set -eu
cd "$(dirname "$0")/.."
LABEL="$1"; FLAGS="${2:--C target-cpu=x86-64-v3}"
tar czf /tmp/se-sync.tgz engine/Cargo.toml engine/Cargo.lock engine/src bench/*.py bench/*.sh
gcloud compute scp --zone=us-central1-a --tunnel-through-iap /tmp/se-sync.tgz shopping-bench:/tmp/se-sync.tgz >/dev/null 2>&1
out/vm.sh ". ~/.cargo/env && cd ~/ss && tar xzf /tmp/se-sync.tgz && mkdir -p out/perf && cd engine && RUSTFLAGS='$FLAGS' CARGO_TARGET_DIR=../out/perf/target cargo build --release 2>&1 | tail -2 && cp ../out/perf/target/release/shopping-engine ../out/perf/$LABEL && ls -l ../out/perf/$LABEL" | grep -v -e NumPy -e "^$" -e WARNING -e iap
