FROM rust:1.87 as builder

WORKDIR /app
COPY . .

RUN make build-app

FROM debian:bookworm-slim

WORKDIR /app
RUN apt-get update && apt-get install -y \
    libssl3 \
    libssl-dev \
    openssl \
    ca-certificates \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/hyperarb /app/hyperarb

ENTRYPOINT ["/app/hyperarb"]