# Multi-stage build for electrs
FROM rust:1.75-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    clang \
    cmake \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy source code
COPY . .

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false -u 1000 electrs

# Copy binary from builder
COPY --from=builder /build/target/release/electrs /usr/local/bin/electrs

# Create directories
RUN mkdir -p /data/db /data/logs && \
    chown -R electrs:electrs /data

USER electrs
WORKDIR /data

EXPOSE 3000 50001 4224

ENTRYPOINT ["/usr/local/bin/electrs"]


