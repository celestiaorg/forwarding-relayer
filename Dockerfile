# Multi-stage build for forwarding-relayer (backend + relayer)
# Using nightly to support dependencies that require edition2024
FROM rustlang/rust:nightly-bookworm AS builder

WORKDIR /build

# Copy all source files
COPY Cargo.toml Cargo.lock ./
COPY e2e/Cargo.toml ./e2e/
COPY src ./src
COPY e2e/src ./e2e/src

# Build the binary with nightly features
RUN cargo build --release --bin forwarding-relayer

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/forwarding-relayer /usr/local/bin/forwarding-relayer

# Copy entrypoint script
COPY scripts/docker-entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Create storage directory
RUN mkdir -p /app/storage

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["relayer"]
