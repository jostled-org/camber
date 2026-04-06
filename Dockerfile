# Multi-stage build for Camber projects and `camber serve` proxy mode.
#
# Build a library project:
#   docker build --build-arg BIN=my-service .
#
# Build the camber CLI (proxy mode):
#   docker build .

ARG RUST_VERSION=1.85
ARG BIN=camber

# ── Build stage ──────────────────────────────────────────────────────
FROM rust:${RUST_VERSION}-alpine AS builder

RUN apk add --no-cache musl-dev protobuf-dev

WORKDIR /build

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/camber/Cargo.toml crates/camber/Cargo.toml
COPY crates/camber-cli/Cargo.toml crates/camber-cli/Cargo.toml
COPY crates/camber-build/Cargo.toml crates/camber-build/Cargo.toml
COPY crates/suspension-core/Cargo.toml crates/suspension-core/Cargo.toml

# Create stub files so cargo can resolve the workspace
RUN mkdir -p crates/camber/src crates/camber-cli/src crates/camber-build/src crates/suspension-core/src && \
    echo "fn main() {}" > crates/camber/src/lib.rs && \
    echo "fn main() {}" > crates/camber-cli/src/main.rs && \
    echo "fn main() {}" > crates/camber-cli/src/lib.rs && \
    echo "fn main() {}" > crates/camber-build/src/lib.rs && \
    echo "fn main() {}" > crates/suspension-core/src/lib.rs

# Cache dependencies
RUN cargo build --release --workspace 2>/dev/null || true

# Copy real source
COPY crates/ crates/

# Build the target binary
ARG BIN
RUN cargo build --release --bin ${BIN}

# ── Runtime stage ────────────────────────────────────────────────────
FROM alpine:3.21 AS runtime

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/${BIN} /usr/local/bin/app

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/app"]
