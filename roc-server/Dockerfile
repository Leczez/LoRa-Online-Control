# Build context is the repository root (this is a Cargo workspace member),
# e.g.: docker build -f roc-server/Dockerfile -t roc-server .

FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY sx126x sx126x
COPY sx127x sx127x
COPY lora-cli lora-cli
COPY roc-server roc-server
RUN cargo build --release -p roc-server

FROM debian:bookworm-slim
RUN useradd -r -U -d /data -M roc-server && mkdir -p /data && chown roc-server:roc-server /data
COPY --from=builder /app/target/release/roc-server /usr/local/bin/roc-server

ENV ROC_SERVER_LISTEN=0.0.0.0:8080
ENV ROC_SERVER_DB=/data/punches.db
VOLUME /data
EXPOSE 8080
USER roc-server
ENTRYPOINT ["/usr/local/bin/roc-server"]
