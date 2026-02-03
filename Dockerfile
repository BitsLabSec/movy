FROM rust:trixie AS builder

WORKDIR /work
COPY Cargo.lock .
COPY Cargo.toml .
COPY crates ./crates
COPY move ./move

RUN apt update && apt install libz3-dev libclang-dev libssl-dev -y
RUN rustup default 1.92.0
RUN cargo build --release

FROM rust:trixie AS runner

RUN apt update && apt install libz3-dev libssl-dev -y
COPY --from=builder /work/target/release/movy /usr/bin/movy

ENTRYPOINT [ "/usr/bin/movy" ]