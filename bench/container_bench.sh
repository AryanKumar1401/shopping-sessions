#!/bin/sh
# Run bench/bench.py inside a native linux/arm64 container with the scorer's docker flags.
#   bench/container_bench.sh DATASET [bench.py args...] VARIANT...
# DATASET is a directory name under data/ (dev, s7-5m, s3-20m). It is copied once into a docker
# volume (se-data-<name>) so that file I/O is the VM's own ext4 + page cache, not virtiofs.
# Binaries come from out/perf/linux/ (see bench/linux_build.sh), visible as /bin-perf/<label>.
# Output goes to a tmpfs-less named volume (se-perf-out) like the scorer's /output bind mount.
#   DROP=1   drop the VM page cache before every single run (cold-cache numbers)
#   CPUS / MEM  override --cpus / --memory (default 8 / 3500m: the Docker VM has only 3.8 GB)
set -eu
cd "$(dirname "$0")/.."
DS="$1"; shift
VOL="se-data-$DS"
IMG="${BENCH_IMAGE:-rust:1-bookworm}"
if ! docker volume inspect "$VOL" >/dev/null 2>&1; then
  docker volume create "$VOL" >/dev/null
  docker run --rm --platform linux/arm64 -v "$PWD/data/$DS":/src:ro -v "$VOL":/data "$IMG" sh -c 'cp -r /src/. /data/ && chmod -R a+rX /data'
fi
docker volume inspect se-perf-out >/dev/null 2>&1 || {
  docker volume create se-perf-out >/dev/null
  docker run --rm --platform linux/arm64 -v se-perf-out:/output "$IMG" chmod 777 /output
}
PRE=""
if [ -n "${DROP:-}" ]; then
  # A privileged helper keeps running next to the benchmark and drops the VM's page cache on request.
  docker rm -f se-dropper >/dev/null 2>&1 || true
  docker run -d --rm --name se-dropper --privileged --platform linux/arm64 -v se-perf-out:/output "$IMG" \
    sh -c 'while :; do if [ -e /output/.drop ]; then sync; echo 3 > /proc/sys/vm/drop_caches; rm -f /output/.drop; fi; sleep 0.05; done' >/dev/null
  PRE="--pre 'touch /output/.drop; while [ -e /output/.drop ]; do sleep 0.05; done'"
  trap 'docker rm -f se-dropper >/dev/null 2>&1 || true' EXIT
fi
docker run --rm --platform linux/arm64 \
  --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
  --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 4096 --cpus "${CPUS:-8}" --memory "${MEM:-3500m}" --memory-swap "${MEM:-3500m}" \
  --user 1000:1000 \
  -v "$VOL":/data:ro -v se-perf-out:/output -v "$PWD/out/perf/linux":/bin-perf:ro -v "$PWD/bench":/bench \
  -e DATA_DIR=/data -e OUTPUT_DIR=/output -e NCPU="${CPUS:-8}" \
  -e BENCH_ENV="docker linux/arm64 (Docker Desktop VM, 8 vCPU, 3.8 GB, kernel $(docker info --format '{{.KernelVersion}}')), scorer flags${DROP:+, page cache dropped before each run}" \
  -w /bin-perf "$IMG" \
  sh -c "python3 /bench/bench.py --data /data --out /output $PRE $*"
