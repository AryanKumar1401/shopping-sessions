# Shopping Sessions: Rust engine, static binary in an empty image.
# Scorer builds natively on linux/amd64 (Intel Cascade Lake or newer).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY engine/Cargo.toml engine/Cargo.lock ./
COPY engine/src ./src
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && cp target/x86_64-unknown-linux-gnu/release/shopping-engine /shopping-engine

FROM scratch
COPY --from=build /shopping-engine /shopping-engine
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch
CMD ["/shopping-engine"]
