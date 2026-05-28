FROM rust:trixie AS builder

WORKDIR /work
COPY Cargo.lock .
COPY Cargo.toml .
COPY crates ./crates
COPY move ./move

# z3 and openssl are now vendored (built from source), so we no longer
# need libz3-dev / libssl-dev — instead we need cmake / perl / python3
# to drive their build scripts. libclang-dev stays for bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libclang-dev cmake pkg-config build-essential perl python3 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup default 1.92.0
RUN cargo build --release

FROM rust:trixie AS runner

# Vendored z3 / openssl are statically linked into the binary, so the
# runtime image only needs glibc (provided by the base image).
COPY --from=builder /work/target/release/movy /usr/bin/movy

ENTRYPOINT [ "/usr/bin/movy" ]