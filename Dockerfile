# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Stage 1 — builder
#
# Uses the official Rust image pinned to the channel declared in
# rust-toolchain.toml (stable).  Dependency crates are compiled in their own
# layer so that re-builds caused by application source changes skip the slow
# dependency compile step.
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Install native dependencies required by sqlx (OpenSSL / TLS backend)
RUN apt-get update -qq && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# --- Dependency layer cache -------------------------------------------------
# Copy only the workspace manifests and lock file first.  Docker layer caching
# means this layer is reused as long as no Cargo.toml / Cargo.lock changes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./

# Copy every crate manifest (but not source) so Cargo can resolve the workspace.
COPY crates/mcs-api/Cargo.toml          crates/mcs-api/Cargo.toml
COPY crates/mcs-auth/Cargo.toml         crates/mcs-auth/Cargo.toml
COPY crates/mcs-cluster/Cargo.toml      crates/mcs-cluster/Cargo.toml
COPY crates/mcs-core/Cargo.toml         crates/mcs-core/Cargo.toml
COPY crates/mcs-domain/Cargo.toml       crates/mcs-domain/Cargo.toml
COPY crates/mcs-game/Cargo.toml         crates/mcs-game/Cargo.toml
COPY crates/mcs-observability/Cargo.toml crates/mcs-observability/Cargo.toml
COPY crates/mcs-payments/Cargo.toml     crates/mcs-payments/Cargo.toml
COPY crates/mcs-rating/Cargo.toml       crates/mcs-rating/Cargo.toml
COPY crates/mcs-server/Cargo.toml       crates/mcs-server/Cargo.toml
COPY crates/mcs-storage/Cargo.toml      crates/mcs-storage/Cargo.toml
COPY crates/mcs-variant-rbc/Cargo.toml           crates/mcs-variant-rbc/Cargo.toml
COPY crates/mcs-variant-standard/Cargo.toml       crates/mcs-variant-standard/Cargo.toml

# Create stub lib/main files so `cargo build` can resolve and cache deps without
# any real source code.  They will be overwritten by the full COPY below.
RUN for crate in mcs-api mcs-auth mcs-cluster mcs-core mcs-domain mcs-game \
        mcs-observability mcs-payments mcs-rating mcs-storage \
        mcs-variant-rbc mcs-variant-standard; do \
        mkdir -p crates/$crate/src && echo "pub fn _stub() {}" > crates/$crate/src/lib.rs; \
    done && \
    mkdir -p crates/mcs-server/src && \
    printf 'fn main() {}\n' > crates/mcs-server/src/main.rs

RUN cargo build --release -p mcs-server 2>&1 | tail -5; true

# --- Full source build -------------------------------------------------------
# Now overwrite stubs with actual source and do the real compile.
COPY crates/ crates/

# Touch the binary crate source so Cargo knows to recompile it even though the
# manifest timestamp has not changed.
RUN touch crates/mcs-server/src/main.rs

RUN cargo build --release -p mcs-server

# ---------------------------------------------------------------------------
# Stage 2 — runtime
#
# Minimal Debian Bookworm slim image.  Only the compiled binary and CA
# certificates (for TLS egress, e.g. Redis over TLS) are included.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Install only what the binary needs at runtime.
RUN apt-get update -qq && \
    apt-get install -y --no-install-recommends ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

# Create a dedicated non-root user and group.
RUN groupadd --system --gid 1001 mcs && \
    useradd --system --uid 1001 --gid mcs --no-create-home mcs

# Persistent data lives here; operators mount a volume at this path.
RUN mkdir -p /data && chown mcs:mcs /data
VOLUME ["/data"]

COPY --from=builder /build/target/release/mcs-server /usr/local/bin/mcs-server

# Expose the HTTP port.
EXPOSE 8080

# ---------------------------------------------------------------------------
# Environment defaults
#
#   MCS_BIND            – listen on all interfaces inside the container
#   MCS_DATABASE_URL    – SQLite file inside the mounted /data volume
#   MCS_LOG__FORMAT     – structured JSON is friendlier for log aggregators
#
# REQUIRED at runtime (not set here — must be supplied by the operator):
#   MCS_SESSION__SECRET – high-entropy secret (>= 32 bytes); omitting it causes
#                         the server to log a warning and generate an ephemeral
#                         secret that is invalidated on every restart.
# ---------------------------------------------------------------------------
ENV MCS_BIND="0.0.0.0:8080" \
    MCS_DATABASE_URL="sqlite:///data/mcs.db?mode=rwc" \
    MCS_LOG__FORMAT="json"

USER mcs

ENTRYPOINT ["/usr/local/bin/mcs-server"]
