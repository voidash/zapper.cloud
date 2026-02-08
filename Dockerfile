# Build stage
FROM rust:1.84-bookworm AS builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./
COPY crates/zap-core/Cargo.toml crates/zap-core/
COPY crates/zap-cli/Cargo.toml crates/zap-cli/
COPY crates/zap-web/Cargo.toml crates/zap-web/

# Create dummy source files for dependency caching
RUN mkdir -p src crates/zap-core/src crates/zap-cli/src crates/zap-web/src && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub fn dummy() {}" > crates/zap-core/src/lib.rs && \
    echo "pub fn dummy() {}" > crates/zap-cli/src/lib.rs && \
    echo "pub fn dummy() {}" > crates/zap-web/src/lib.rs

# Build dependencies only
RUN cargo build --release && rm -rf src crates

# Copy actual source
COPY src src
COPY crates crates

# Build the actual binary
RUN touch src/main.rs crates/*/src/*.rs && \
    cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/zap /usr/local/bin/zap

# Create non-root user
RUN useradd -m -u 1000 zap && \
    mkdir -p /data && \
    chown zap:zap /data

USER zap

ENV ZAP_TEMP_DIR=/data
ENV RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

CMD ["zap", "serve", "--addr", "0.0.0.0:8080"]
