# Build stage
FROM rust:1.94-bookworm AS builder

WORKDIR /app

# Install protobuf compiler for tonic-build
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto

# Create dummy source to cache dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub fn placeholder() {}" > src/lib.rs

# Build dependencies (this layer will be cached)
RUN cargo build --release && rm -rf src

# Copy actual source code
COPY src ./src

# Build the actual binary (touch to invalidate cache)
# Raise rustc thread stack to avoid LLVM stack overflow during fat LTO codegen
ENV RUST_MIN_STACK=33554432
RUN touch src/main.rs src/lib.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies. git is required: upload-pack / receive-pack are
# served by spawning the real git binary in stateless-RPC mode.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates git && \
    rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /app/target/release/arbor-git /usr/local/bin/arbor-git

# Create non-root user
RUN useradd -r -s /bin/false arbor && \
    mkdir -p /var/lib/arbor-git && \
    chown arbor:arbor /var/lib/arbor-git

USER arbor

# Expose ports
EXPOSE 50051 8080

# Set default environment variables
ENV RUST_LOG=arbor_git=info,tower_http=info
ENV GRPC_PORT=50051
ENV HTTP_PORT=8080
ENV STORAGE_PATH=/var/lib/arbor-git

CMD ["arbor-git"]
