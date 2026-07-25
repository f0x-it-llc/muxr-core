# ─────────────────────────────────────────────────────────────────────────────
# muxr-notify — standalone deploy image (e.g. for noti.muxr.app)
#
# This is a SEPARATE distributable artifact from muxrd/muxrctl: it is never
# included in the muxr-core release suite tarballs or install.sh (see root
# README.md "Two binaries" / muxr-notify/README.md). It gets its own image
# because the existing docker/Dockerfile builds an interactive terminal-
# multiplexer test rig (zellij/herdr + SSH + a full toolbox) — the wrong base
# for a plain HTTP relay meant to sit behind a TLS-terminating reverse proxy.
#
# Multi-stage build:
#   builder  — rust:1.92-slim (glibc), compiles the whole workspace's Cargo.lock
#              but only the muxr-notify binary target
#   runtime  — debian:bookworm-slim + ca-certificates (outbound HTTPS to FCM /
#              Google OAuth2 token endpoints) + a non-root user
#
# Build (context MUST be the repo root — the workspace Cargo.toml/Cargo.lock and
# the muxrd/muxrctl manifests are needed to resolve the workspace, even though
# neither binary is compiled):
#   docker build -f muxr-notify/Dockerfile -t muxr-notify .
#
# Run (FCM_MODE=log needs no Firebase artifacts; see muxr-notify/README.md for
# the full env reference and the self-host delivery caveat):
#   docker run --rm -p 8080:8080 -e FCM_MODE=log muxr-notify
# ─────────────────────────────────────────────────────────────────────────────

# ══════════════════════════════════════════════════════════════════════════════
# Stage 1 — builder
# ══════════════════════════════════════════════════════════════════════════════
FROM rust:1.92-slim AS builder

# build-essential: cc for rusqlite's `bundled` feature (vendored SQLite compiled
# from source). No protoc/cmake/clang needed — muxr-notify has no tonic/prost or
# aws-lc-rs dependency of its own (rustls' crypto provider pulled in here is
# `ring`, which is pure Rust + a small amount of portable C/asm, no cmake).
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the whole Cargo workspace. muxr-notify is one of three members
# (muxrd, muxrctl, muxr-notify); cargo needs every member's manifest (and, for
# path-dependents like muxrctl -> muxrd, their full source) to resolve the
# workspace, even though only muxr-notify is actually compiled below.
COPY Cargo.toml Cargo.lock ./
COPY muxrd/ ./muxrd/
COPY muxrctl/ ./muxrctl/
COPY muxr-notify/ ./muxr-notify/

RUN cargo build --release -p muxr-notify

# ══════════════════════════════════════════════════════════════════════════════
# Stage 2 — runtime: minimal Debian slim, non-root, plain HTTP listener
# ══════════════════════════════════════════════════════════════════════════════
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin notify

COPY --from=builder /build/target/release/muxr-notify /usr/local/bin/muxr-notify
RUN chmod +x /usr/local/bin/muxr-notify

# Persistent SQLite store — mount a volume at this path in production so
# registered push-handles survive container recreation.
RUN mkdir -p /var/lib/muxr-notify && chown notify:notify /var/lib/muxr-notify
ENV NOTIFY_DB=/var/lib/muxr-notify/notify.db

# Plain HTTP — TLS terminates at the fronting reverse proxy (Traefik/Dokploy/
# Cloudflare), the same pattern muxrd's h2c mode uses. Bind on all interfaces so
# the proxy (a separate container/host) can reach it; the container/network
# boundary is what actually gates external exposure.
ENV NOTIFY_LISTEN=0.0.0.0:8080

# FCM_MODE defaults to `send` in muxr-notify itself (fails fast without
# FCM_SERVICE_ACCOUNT); the dev rig overrides this to `log` at the compose/
# entrypoint layer. A real deployment must set FCM_MODE=send + mount a
# service-account JSON — see muxr-notify/README.md.

USER notify
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/muxr-notify"]
