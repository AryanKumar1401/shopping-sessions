#!/bin/sh
# Build the engine for Linux inside a container (incremental: cargo target dir in a volume)
# and copy the static binary to out/perf/linux/<label>.
#   bench/linux_build.sh LABEL [PLATFORM]      PLATFORM: linux/arm64 (default, native on this Mac) or linux/amd64 (emulated)
# Same RUSTFLAGS as the root Dockerfile. The final image must still be built from the Dockerfile.
set -eu
cd "$(dirname "$0")/.."
LABEL="$1"; PLATFORM="${2:-linux/arm64}"
case "$PLATFORM" in
  linux/arm64) T=aarch64-unknown-linux-gnu; F="" ;;
  linux/amd64) T=x86_64-unknown-linux-gnu;  F="-C target-cpu=x86-64-v3" ;;
  *) echo "bad platform" >&2; exit 1 ;;
esac
VOL="se-perf-target-$(echo "$PLATFORM" | tr / -)"
mkdir -p out/perf/linux
docker run --rm --platform "$PLATFORM" \
  -v "$PWD/engine":/src:ro -v "$VOL":/target -v "$PWD/out/perf/linux":/out \
  -e RUSTFLAGS="$F -C target-feature=+crt-static" -e CARGO_TARGET_DIR=/target \
  -w /src rust:1-bookworm \
  sh -c "cargo build --release --locked --target $T 2>&1 | tail -2 && cp /target/$T/release/shopping-engine /out/$LABEL"
ls -l "out/perf/linux/$LABEL"
