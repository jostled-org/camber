# Multi-stage build for Camber projects and `camber serve` proxy mode.
#
# Build a library project:
#   docker build --build-arg BIN=my-service .
#
# Build the camber CLI (proxy mode):
#   docker build .

# 1.88 is the floor the dependency tree imposes: rcgen, time, and tonic-build
# all refuse to build below it. No crate here declares a rust-version, so this
# pin is the only place that constraint is recorded.
ARG RUST_VERSION=1.88
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
COPY crates/camber-macros/Cargo.toml crates/camber-macros/Cargo.toml

# Create stub files so cargo can resolve the workspace. camber declares a build
# script, so it needs a stub build.rs here or the cache build skips it and the
# real build recompiles the dependency tree from scratch.
RUN mkdir -p crates/camber/src crates/camber-cli/src crates/camber-build/src crates/camber-macros/src && \
    echo "fn main() {}" > crates/camber/build.rs && \
    echo "" > crates/camber/src/lib.rs && \
    echo "fn main() {}" > crates/camber-cli/src/main.rs && \
    echo "" > crates/camber-cli/src/lib.rs && \
    echo "" > crates/camber-build/src/lib.rs && \
    echo "" > crates/camber-macros/src/lib.rs

# Cache dependencies
RUN cargo build --release --workspace 2>/dev/null || true

# Copy real source
COPY crates/ crates/

# Build the target binary.
#
# COPY preserves the build context's mtimes, and cargo fingerprints by mtime,
# so the stub artifacts from the cache layer above can look newer than the real
# sources that just replaced them — cargo then links the empty stubs and the
# build fails on missing items. Touching the sources forces the workspace
# crates to rebuild while the cached dependency artifacts stay valid.
ARG BIN
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release --bin ${BIN}

# ── Runtime stage ────────────────────────────────────────────────────
FROM alpine:3.21 AS runtime

# An ARG declared before the first FROM is only in scope for FROM lines. Without
# this redeclaration ${BIN} expands to nothing and the COPY below silently
# copies the whole release directory over /usr/local/bin/app.
ARG BIN

RUN apk add --no-cache ca-certificates

COPY --from=builder /build/target/release/${BIN} /usr/local/bin/app

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/app"]
