FROM rust:1.85-slim as builder

WORKDIR /usr/src/poler-cli
COPY . .

RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/poler-cli/target/release/poler /usr/local/bin/poler

ENTRYPOINT ["poler", "mcp"]
