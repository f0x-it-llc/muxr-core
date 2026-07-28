# ─────────────────────────────────────────────────────────────────────────────
# Muxr — muxrd gRPC backend rig
#
# Multi-stage build:
#   builder        — rust:1.92-alpine, static musl binaries (muxrd + muxrctl +
#                    muxr-notify — the whole workspace)
#   runtime        — debian:trixie-slim with full terminal tooling + zellij 0.44.3,
#                    an SSH server, muxrctl to configure/start the server, and
#                    muxr-notify (started in-container by the entrypoint in
#                    FCM_MODE=log, so `MUXRD_NOTIFY_RELAY_URL` has a local target
#                    for e2e testing — no Firebase artifacts needed)
#                    (the DEFAULT image — zellij only)
#   runtime-herdr  — runtime + a pinned, UNMODIFIED upstream herdr binary, so the
#                    image carries BOTH backends (zellij AND herdr). Built ONLY
#                    when a herdr-bearing compose profile is selected, so the
#                    default rig never pulls herdr. The entrypoint's BACKEND env
#                    picks what this image actually runs:
#                      BACKEND=herdr → muxrd restricted to herdr
#                      BACKEND=both  → start zellij + herdr; muxrd auto-detects &
#                                      serves BOTH simultaneously (Phase 3
#                                      serve-all default — the on-device multi-
#                                      backend test rig)
#
# muxr-notify is a SEPARATE distributable artifact from muxrd/muxrctl (never
# bundled in the release suite tarballs or install.sh) — its own deployment
# image is muxr-notify/Dockerfile. It only rides in THIS rig image so the rig
# can exercise the full push-notification path end-to-end.
#
# Build context: repo root  (compose sets context: ..)
#   docker compose -f docker/compose.yaml up --build                              # zellij (default)
#   docker compose -f docker/compose.yaml --profile herdr up --build muxrd-herdr  # herdr only
#   docker compose -f docker/compose.yaml --profile both  up --build muxrd-both   # BOTH backends
# ─────────────────────────────────────────────────────────────────────────────

# ══════════════════════════════════════════════════════════════════════════════
# Stage 1 — builder: compile the workspace (muxrd + muxrctl) as musl
# ══════════════════════════════════════════════════════════════════════════════
FROM rust:1.92-alpine AS builder

# Static-build deps (A3 working recipe):
# - musl-dev            — libc headers for the musl native host
# - curl-static         — static libcurl (no curl-dev; curl-static pulls it)
# - openssl-dev / openssl-libs-static — headers + static libssl/libcrypto
# - pkgconfig           — pkg-config binary (openssl-sys needs it)
# - nghttp2-static      — static libnghttp2 (curl HTTP/2 dep)
# - zstd-static         — static libzstd
# - brotli-static       — static libbrotli
# - libssh2-static / libssh2-dev — static libssh2 + headers
# - zlib-static         — static zlib
# - libpsl-static       — static libpsl
# - libidn2-static      — static libidn2
# - libunistring-static — static libunistring (libidn2 dep)
# - perl                — required by openssl build scripts
# - protobuf protoc     — protoc compiler for tonic-prost-build
# - git                 — Cargo may fetch git deps
RUN apk add --no-cache \
      musl-dev \
      curl-static \
      openssl-dev openssl-libs-static \
      pkgconfig \
      nghttp2-static \
      zstd-static \
      brotli-static \
      libssh2-static libssh2-dev \
      zlib-static \
      libpsl-static \
      libidn2-static \
      libunistring-static \
      perl \
      protobuf protoc \
      git

WORKDIR /build

# Copy the whole Cargo workspace (muxrctl path-depends on muxrd; muxr-notify is
# a third, independent workspace member — see docker/README or ARCHITECTURE for
# why it needs its own explicit COPY: `cargo build --workspace` resolves every
# member's manifest, so an uncopied member breaks the build, not just gets
# skipped) so all three binaries build from the same locked dependency set.
COPY Cargo.toml Cargo.lock ./
COPY muxrd/ ./muxrd/
COPY muxrctl/ ./muxrctl/
COPY muxr-notify/ ./muxr-notify/

# Build all release binaries (muxrd + muxrctl + muxr-notify).
# On rust:alpine the native target IS musl — no --target flag needed.
# OPENSSL_STATIC=1  → openssl-sys links statically
# PKG_CONFIG_ALL_STATIC=1 → all pkg-config deps linked statically
RUN OPENSSL_STATIC=1 PKG_CONFIG_ALL_STATIC=1 \
    cargo build --release --workspace

# ══════════════════════════════════════════════════════════════════════════════
# Stage 2 — runtime: Debian tooling image + zellij 0.44.3 pinned (DEFAULT image)
# ══════════════════════════════════════════════════════════════════════════════
FROM debian:trixie-slim AS runtime

ENV DEBIAN_FRONTEND=noninteractive \
    TERM=xterm-256color \
    LANG=C.UTF-8 \
    EDITOR=nvim

# ── Base + a broad set of terminal tooling (core: strict) ────────────────────
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl wget git openssl xz-utils unzip \
      openssh-server \
      locales procps iproute2 less man-db \
      bash zsh \
      neovim \
      btop htop \
      ripgrep fd-find fzf bat tree jq ncdu tmux nano \
      build-essential python3 python3-pip \
      nodejs npm \
    && ln -sf /usr/bin/batcat /usr/local/bin/bat \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd

# ── Fun extras (best-effort: missing ones must not fail the build) ───────────
RUN apt-get install -y --no-install-recommends \
      cmatrix sl cowsay fortune-mod fastfetch ; \
    rm -rf /var/lib/apt/lists/*

# ── Zellij PINNED v0.44.3 (the version muxrd is compiled against) ──────
# muxrd refuses to start on any other version — do NOT change this.
RUN curl -fsSL \
      https://github.com/zellij-org/zellij/releases/download/v0.44.3/zellij-x86_64-unknown-linux-musl.tar.gz \
      | tar -xz -C /usr/local/bin \
    && chmod +x /usr/local/bin/zellij \
    && zellij --version \
    && zellij --version | grep -q "0.44.3" || (echo "ERROR: zellij version is not 0.44.3" && exit 1)

# ── lazygit (release binary; not in apt) ─────────────────────────────────────
RUN set -eux; \
    LG_VER="$(curl -fsSL https://api.github.com/repos/jesseduffield/lazygit/releases/latest | jq -r .tag_name | sed 's/^v//')"; \
    curl -fsSL "https://github.com/jesseduffield/lazygit/releases/latest/download/lazygit_${LG_VER}_Linux_x86_64.tar.gz" \
      | tar -xz -C /usr/local/bin lazygit; \
    lazygit --version

# ── NvChad (Neovim config) pre-bootstrapped ──────────────────────────────────
# Clone the NvChad starter and pre-install plugins headlessly so first attach is
# instant. Mason/LSP/treesitter parsers that need network are synced best-effort.
RUN git clone --depth 1 https://github.com/NvChad/starter /root/.config/nvim \
    && rm -rf /root/.config/nvim/.git \
    && nvim --headless "+Lazy! sync" +qa 2>/dev/null || true \
    && nvim --headless "+TSInstallSync! lua bash" +qa 2>/dev/null || true

# ── Demo project content so panes have something to show ─────────────────────
RUN mkdir -p /root/projects/api/src \
    && printf 'use axum::{Router, routing::get};\n\n#[tokio::main]\nasync fn main() {\n    let app = Router::new().route("/", get(handler));\n    let listener = tokio::net::TcpListener::bind("0.0.0.0:8082").await.unwrap();\n    axum::serve(listener, app).await.unwrap();\n}\n' > /root/projects/api/src/main.rs \
    && git -C /root/projects/api init -q \
    && git -C /root/projects/api add -A \
    && git -C /root/projects/api -c user.email=test@example.com -c user.name=test commit -qm "init" || true

# ── muxrd + muxrctl binaries (static musl, from builder stage) ──────
COPY --from=builder /build/target/release/muxrd /usr/local/bin/muxrd
COPY --from=builder /build/target/release/muxrctl /usr/local/bin/muxrctl
RUN chmod +x /usr/local/bin/muxrd /usr/local/bin/muxrctl

# ── muxr-notify binary (rig-only in-container relay, FCM_MODE=log — see
# entrypoint.sh's start_notify()). This is the SAME rig image; muxr-notify is
# never distributed as part of the muxr-core release suite — its own
# deployment image lives at muxr-notify/Dockerfile. ──────────────────────────
COPY --from=builder /build/target/release/muxr-notify /usr/local/bin/muxr-notify
RUN chmod +x /usr/local/bin/muxr-notify

# Bind on all interfaces by default so the gRPC server is reachable through the
# published port. The Docker port mapping (compose) still gates host exposure;
# override per-run via muxrctl's Configure screen or MUXRD_BIND.
#
# MUXRD_SAN — comma-separated extra SANs to include in the self-signed
# TLS cert (e.g. a tailnet or LAN IP that is not a local interface inside the
# container). Set at runtime via compose.yaml or `docker run -e`; do NOT
# hard-code a SAN in the image, since the IP depends on the deployment site.
ENV MUXRD_BIND=0.0.0.0:50051

# Multiplexer backend the rig drives. The default `runtime` image is zellij-only;
# the `runtime-herdr` stage overrides this to `herdr`. The entrypoint branches on it.
ENV BACKEND=zellij

# muxr-notify (in-container push relay for e2e testing — FCM_MODE=log, no
# Firebase artifacts needed). Loopback-only; not published to the host, since
# only the co-located muxrd needs to reach it. Set NOTIFY_ENABLED=0 to skip
# starting it. See entrypoint.sh's start_notify().
ENV NOTIFY_ENABLED=1
ENV NOTIFY_PORT=8090

# ── SSH access (dev rig) ─────────────────────────────────────────────────────
# Passwordless root login over SSH so you can attach and drive muxrctl. Host
# keys are generated at build time; the entrypoint clears root's password so no
# credential is needed. This is a dev/test affordance — see the security note.
RUN ssh-keygen -A \
    && mkdir -p /run/sshd \
    && sed -ri 's/^#?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config \
    && sed -ri 's/^#?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config \
    && sed -ri 's/^#?PermitEmptyPasswords.*/PermitEmptyPasswords yes/' /etc/ssh/sshd_config

WORKDIR /root

COPY docker/layout.kdl /root/.config/zellij/layouts/muxr.kdl
# Managed zellij config (disables the zellij:link background plugin). Staged at an
# image path; entrypoint.sh installs it into the persisted config volume on boot.
COPY docker/config.kdl /usr/local/share/muxr/config.kdl
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# gRPC port + SSH
EXPOSE 50051 22

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# ══════════════════════════════════════════════════════════════════════════════
# Stage 3 — runtime-herdr: the runtime image (zellij) + a pinned, UNMODIFIED herdr
#           binary → an image carrying BOTH backends
# ══════════════════════════════════════════════════════════════════════════════
# Opt-in variant. Because it extends `runtime` (which already has zellij 0.44.3)
# and adds the herdr binary, this single image can run muxrd against herdr alone
# (`BACKEND=herdr`) OR against zellij AND herdr simultaneously (`BACKEND=both`,
# Phase 3 serve-all — the on-device multi-backend test rig). The entrypoint
# branches on BACKEND. Built ONLY when a herdr-bearing compose profile selects
# `target: runtime-herdr`, so the DEFAULT zellij rig (`target: runtime`) never
# pulls herdr.
#
# AGPL-3.0 discipline: herdr is a SEPARATE, UNMODIFIED, user-installed binary that
# muxrd drives only over its public Unix sockets. We download the official upstream
# release here for a LOCAL dev rig (not a distributed image) and never bundle into
# the default image, modify, or redistribute herdr. muxrd remains the TLS/bearer
# boundary; herdr runs same-user/same-host (its sockets are 0600, no auth).
FROM runtime AS runtime-herdr

# The rig tracks the LATEST herdr release and is deliberately NOT pinned.
#
# muxrd discovers herdr's wire protocol version at runtime (JSON-API `ping`) and
# echoes it in the relay handshake, so it is compatible with whatever herdr the
# operator is running. Pinning here would only hide that: the rig would keep
# testing one frozen protocol while real users moved on — which is exactly how
# muxrd came to be stuck on protocol 14 after herdr shipped 16 and 17.
#
# `latest` resolves to the newest non-prerelease tag (herdr's `preview-*` builds are
# flagged as prereleases, so they are correctly skipped). Override to reproduce a
# specific version:  --build-arg HERDR_VERSION=0.7.4
ARG HERDR_VERSION=latest
RUN set -eux; \
    if [ "${HERDR_VERSION}" = "latest" ]; then \
      resolved="$(curl -fsSL https://api.github.com/repos/ogulcancelik/herdr/releases/latest \
        | jq -r .tag_name | sed 's/^v//')"; \
      [ -n "${resolved}" ] && [ "${resolved}" != "null" ]; \
    else \
      resolved="${HERDR_VERSION}"; \
    fi; \
    curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/ogulcancelik/herdr/releases/download/v${resolved}/herdr-linux-x86_64"; \
    chmod +x /usr/local/bin/herdr; \
    # Sanity: the download is a real Linux ELF (curl -f already failed on 404).
    head -c 64 /usr/local/bin/herdr | grep -q ELF; \
    echo "herdr ${resolved} installed (wire protocol negotiated at runtime)"

# Default this image to the herdr-only backend; compose overrides BACKEND per
# service (`herdr` or `both`). The entrypoint propagates HERDR_SOCKET_PATH to SSH
# login sessions (and MUXRD_BACKEND only in single-backend mode) so the muxrctl-
# spawned daemon resolves the right backend(s):
#   muxrd selection = CLI --backend > MUXRD_BACKEND env > serve-all (Phase 3).
#   BACKEND=herdr → MUXRD_BACKEND=herdr (restrict);  BACKEND=both → unset (serve all).
ENV BACKEND=herdr

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
