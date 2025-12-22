FROM rust:bookworm AS builder

WORKDIR /work
COPY Cargo.lock .
COPY Cargo.toml .
COPY crates ./crates
COPY move ./move

RUN apt update && apt install libclang-dev -y
RUN rustup default 1.88
RUN cargo build --release

FROM rust:bookworm AS runner

COPY --from=builder /work/target/release/movy /usr/bin/movy

ENTRYPOINT [ "/usr/bin/movy" ]