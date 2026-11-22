# Shopping Sessions: Rust engine, static binary in an empty image.
# The scorer builds natively on linux/amd64 (Intel Cascade Lake or newer).
# The arch switch only exists so the same file builds natively on arm64 for local benchmarking.
FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY engine/Cargo.toml engine/Cargo.lock ./
COPY engine/src ./src
RUN set -eux; \
    case "$(uname -m)" in \
      x86_64)  T=x86_64-unknown-linux-gnu;  F="-C target-cpu=x86-64-v3" ;; \
      aarch64) T=aarch64-unknown-linux-gnu; F="" ;; \
      *) echo "unsupported arch" >&2; exit 1 ;; \
    esac; \
    RUSTFLAGS="$F -C target-feature=+crt-static" cargo build --release --locked --target "$T"; \
    cp "target/$T/release/shopping-engine" /shopping-engine

FROM scratch
COPY --from=build /shopping-engine /shopping-engine
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
# glibc malloc: huge pages for its heaps, grown 32 MiB at a time (see engine/src/main.rs; the
# engine re-executes itself with this if the variable is missing).
ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=33554432
CMD ["/shopping-engine"]
