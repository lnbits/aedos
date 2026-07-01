FROM rust:1.91-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.toml
COPY crates crates
RUN cargo build --release -p nostr-label-oracle

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/nostr-label-oracle /usr/local/bin/nostr-label-oracle
EXPOSE 8080
CMD ["nostr-label-oracle"]
