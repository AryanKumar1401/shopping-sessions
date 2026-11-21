#!/bin/sh
# Sync this worktree's engine/ + bench/ to the VM dir ~/w-person and build ~/w-person/out/perf/LABEL (under the VM lock).
#   bench/vm_w.sh LABEL
set -eu
cd "$(dirname "$0")/.."
LABEL="$1"; FLAGS="${2:--C target-cpu=x86-64-v3}"
T=${TMPDIR:-/tmp}/se-sync-person.tgz
tar czf "$T" engine/Cargo.toml engine/Cargo.lock engine/src bench/*.py bench/*.sh
gcloud compute scp --zone=us-central1-a --tunnel-through-iap "$T" shopping-bench:/tmp/se-sync-person.tgz >/dev/null 2>&1
/Users/aryankumar/shopping-sessions/out/vm.sh ". ~/.cargo/env && cd ~/w-person && tar xzf /tmp/se-sync-person.tgz && touch engine/src/*.rs && cd engine && RUSTFLAGS='$FLAGS' CARGO_TARGET_DIR=../out/perf/target flock /tmp/vm.lock cargo build --release 2>&1 | tail -3 && cp ../out/perf/target/release/shopping-engine ../out/perf/$LABEL && ls -l ../out/perf/$LABEL" | grep -v -e NumPy -e "^$" -e WARNING -e iap
