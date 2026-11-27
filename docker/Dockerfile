# ─────────────────────────────────────────────────────────────────────────────
# Muxr — muxrd gRPC backend rig
#
# Multi-stage build:
#   builder        — rust:1.95-alpine, static musl binaries (muxrd + muxrctl +
#                    muxr-notify — the whole workspace). 1.95 is the workspace's
#                    `rust-version` floor (raised for zellij-utils 0.45.1) and cargo
#                    enforces it per package, so an older base image cannot build
#                    this workspace at all
#   runtime        — debian:trixie-slim with full terminal tooling + zellij
#                    (build argument ZELLIJ_VERSION: 0.46.0 by default, or 0.45.1),
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
#   tmux-build     — debian:trixie-slim building tmux from its release tarball
#                    (build arguments TMUX_VERSION, default 3.8, and TMUX_SHA256).
#                    The tarball is checked with `sha256sum -c` against the
#                    pinned digest BEFORE it is unpacked: this is the rig's first
#                    checksum-verified download (the zellij, herdr and lazygit
#                    downloads above trust TLS and the release URL alone)
#   runtime-tmux   — runtime + that tmux copied over the distribution's tmux
#                    3.5a, so exactly one tmux is on PATH. BACKEND=tmux →
#                    muxrd restricted to tmux. Built ONLY for the `tmux` profile
#   runtime-herdr-tmux
#                  — runtime-herdr + the same tmux: all three backends.
#                    BACKEND=all → start zellij, herdr and tmux; muxrd
#                    auto-detects & serves all three. Built ONLY for the `all`
#                    profile
#
# muxr-notify is a SEPARATE distributable artifact from muxrd/muxrctl (never
# bundled in the release suite tarballs or install.sh) — its own deployment
# image is muxr-notify/Dockerfile. It only rides in THIS rig image so the rig
# can exercise the full push-notification path end-to-end.
#
# ARCHITECTURE: every third-party binary this image downloads (zellij, herdr,
# lazygit) is selected from BuildKit's automatic `TARGETARCH`, so the rig builds
# natively on an amd64 host and on an arm64 one with nothing to configure. The
# three upstreams do NOT agree on how to spell the ARM asset — zellij and herdr
# publish `aarch64`, lazygit publishes `arm64` — so each download maps
# `TARGETARCH` itself instead of sharing one token: a shared token passes on
# amd64 and 404s only on arm64. To force the amd64 image under emulation:
#   DOCKER_DEFAULT_PLATFORM=linux/amd64 ./docker/run.sh …
# tmux needs no mapping: the tmux rigs download its source and build it for
# the build platform.
#
# Build context: repo root  (compose sets context: ..)
#   docker compose -f docker/compose.yaml up --build                              # zellij (default)
#   docker compose -f docker/compose.yaml --profile herdr up --build muxrd-herdr  # herdr only
#   docker compose -f docker/compose.yaml --profile both  up --build muxrd-both   # BOTH backends
#   docker compose -f docker/compose.yaml --profile tmux  up --build muxrd-tmux   # tmux only
#   docker compose -f docker/compose.yaml --profile all   up --build muxrd-all    # all three
# ─────────────────────────────────────────────────────────────────────────────

# ══════════════════════════════════════════════════════════════════════════════
# Stage 1 — builder: compile the workspace (muxrd + muxrctl) as musl
# ══════════════════════════════════════════════════════════════════════════════
FROM rust:1.95-alpine AS builder

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
# Stage 2 — runtime: Debian tooling image + zellij (DEFAULT image)
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

# ── Zellij (ZELLIJ_VERSION: 0.46.0 by default, or 0.45.1) ─────────────────────
# muxrd supports the current zellij release line and the newest one; this
# argument selects which of them the rig runs. The rig is for the versions
# muxrd supports, so any other value fails the build here rather than giving
# an image whose muxrd refuses to start. The same variable builds the URL and
# the assertion, so the two cannot drift apart.
#
# Architecture-aware: `TARGETARCH` is `amd64`/`arm64`, while zellij names its
# assets after Rust target triples — hence the mapping to `x86_64`/`aarch64`.
ARG TARGETARCH
ARG ZELLIJ_VERSION=0.46.0
RUN set -eux; \
    case "${ZELLIJ_VERSION}" in \
      0.45.1|0.46.0) ;; \
      *) echo "ERROR: ZELLIJ_VERSION='${ZELLIJ_VERSION}' is not a zellij version this rig supports (0.45.1 or 0.46.0)" >&2; exit 1 ;; \
    esac; \
    case "${TARGETARCH:-amd64}" in \
      amd64) zj_arch=x86_64 ;; \
      arm64) zj_arch=aarch64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
      "https://github.com/zellij-org/zellij/releases/download/v${ZELLIJ_VERSION}/zellij-${zj_arch}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/zellij; \
    zellij --version; \
    zellij --version | grep -qx "zellij ${ZELLIJ_VERSION}" || { echo "ERROR: zellij version is not ${ZELLIJ_VERSION}" >&2; exit 1; }

# ── lazygit (release binary; not in apt) ─────────────────────────────────────
# lazygit spells its ARM asset `arm64`, NOT `aarch64` like zellij and herdr, so
# it maps `TARGETARCH` on its own — reusing the zellij/herdr token here would
# build fine on amd64 and 404 only on an arm64 host. (`TARGETARCH` is already
# declared in this stage, above the zellij step.)
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) lg_arch=x86_64 ;; \
      arm64) lg_arch=arm64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    LG_VER="$(curl -fsSL https://api.github.com/repos/jesseduffield/lazygit/releases/latest | jq -r .tag_name | sed 's/^v//')"; \
    curl -fsSL "https://github.com/jesseduffield/lazygit/releases/latest/download/lazygit_${LG_VER}_linux_${lg_arch}.tar.gz" \
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
# Opt-in variant. Because it extends `runtime` (which already has zellij)
# and adds the herdr binary, this single image can run muxrd against herdr alone
# (`BACKEND=herdr`) OR against zellij AND herdr simultaneously (`BACKEND=both`,
# Phase 3 serve-all — the on-device multi-backend test rig). The entrypoint
# branches on BACKEND. Built ONLY when a herdr-bearing compose profile selects
# `target: runtime-herdr`, so the DEFAULT zellij rig (`target: runtime`) never
# pulls herdr.
#
# Apache-2.0 discipline: herdr is a SEPARATE, UNMODIFIED, user-installed binary that
# muxrd drives only over its public Unix sockets. We download the official upstream
# release here for a LOCAL dev rig (not a distributed image) and never bundle into
# the default image, modify, or redistribute herdr. muxrd remains the TLS/bearer
# boundary; herdr runs same-user/same-host (its sockets are 0600, no auth).
FROM runtime AS runtime-herdr

# PINNED to the last herdr release muxrd has been TESTED against.
#
# 0.9.5 is the pin (the newest release tested); 0.9.0 is the supported FLOOR. Both
# ship wire protocol 22 with byte-identical message bodies — the value recorded in
# `HERDR_MAX_TESTED_PROTOCOL` (muxrd/src/multiplexer/herdr/wire.rs). The pin and
# that constant must move together. Run the suite against the floor with
# `HERDR_VERSION=0.9.0 ./docker/run.sh --herdr`.
#
# This used to default to `latest`, on the theory that muxrd negotiates the
# protocol at runtime so the rig should track upstream. It does negotiate — but
# this layer sits DOWNSTREAM of the muxrd binary layer, so `latest` meant any
# unrelated muxrd rebuild silently swapped the herdr under the rig. That is
# exactly how the rig came up on herdr 0.8.2 / protocol 20 against a muxrd whose
# tested ceiling was then 17, with nothing in the build output saying so.
#
# BUMPS MUST BE DELIBERATE: raising this version is a paired change — bump it
# here, re-verify the wire layout against the new release, and update
# `HERDR_MAX_TESTED_PROTOCOL` (and this comment) in the same commit. Keep
# compose.yaml's two `${HERDR_VERSION:-…}` defaults and run.sh's banner in step.
# To try another release without committing to it:
#   --build-arg HERDR_VERSION=<x.y.z>
#
# `latest` is still accepted as an explicit opt-in and resolves to the newest
# non-prerelease tag (herdr's `preview-*` builds are flagged as prereleases, so
# they are correctly skipped) — an untested-by-construction rig, use knowingly.
ARG HERDR_VERSION=0.9.5

# herdr spells its ARM asset `aarch64` (like zellij, unlike lazygit).
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) herdr_arch=x86_64 ;; \
      arm64) herdr_arch=aarch64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    if [ "${HERDR_VERSION}" = "latest" ]; then \
      resolved="$(curl -fsSL https://api.github.com/repos/herdrdev/herdr/releases/latest \
        | jq -r .tag_name | sed 's/^v//')"; \
      [ -n "${resolved}" ] && [ "${resolved}" != "null" ]; \
    else \
      resolved="${HERDR_VERSION}"; \
    fi; \
    curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/herdrdev/herdr/releases/download/v${resolved}/herdr-linux-${herdr_arch}"; \
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

# ══════════════════════════════════════════════════════════════════════════════
# Stage 4 — tmux-build: tmux from its release tarball, checksum-verified
# ══════════════════════════════════════════════════════════════════════════════
# The distribution's tmux (3.5a on trixie, installed by `runtime`) is the
# backend's floor; the tmux rigs run the newest release instead, so the paths
# muxrd gates on tmux 3.7 and 3.8 (floating panes, moving a pane between tiled
# and floating, `pane_command_running`) run somewhere. Built on the same base
# as `runtime`, so the binary links against the libevent and ncurses libraries
# the distribution's tmux already pulled into the runtime image.
#
# PINNED: TMUX_VERSION and TMUX_SHA256 move together. The digest is the
# release asset's sha256, verified by downloading and building it; `sha256sum
# -c` refuses any other tarball before it is unpacked, so a moved tag or a
# tampered asset fails the build here. Keep compose.yaml's two
# `${TMUX_VERSION:-…}` / `${TMUX_SHA256:-…}` defaults and run.sh's banner in
# step. To build another release, pass both:
#   --build-arg TMUX_VERSION=<x.y> --build-arg TMUX_SHA256=<digest>
FROM debian:trixie-slim AS tmux-build

ARG TMUX_VERSION=3.8
ARG TMUX_SHA256=e79c699c7e949dccd0a4a125e17b8d1261e311b16979dbc8eb34542f3966d82e

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential libevent-dev libncurses-dev bison pkg-config ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN set -eux; \
    tarball="tmux-${TMUX_VERSION}.tar.gz"; \
    curl -fsSL -o "${tarball}" \
      "https://github.com/tmux/tmux/releases/download/${TMUX_VERSION}/${tarball}"; \
    echo "${TMUX_SHA256}  ${tarball}" | sha256sum -c -; \
    tar -xzf "${tarball}"; \
    cd "tmux-${TMUX_VERSION}"; \
    ./configure && make && make install; \
    /usr/local/bin/tmux -V

# ══════════════════════════════════════════════════════════════════════════════
# Stage 5 — runtime-tmux: the runtime image (zellij) + the source-built tmux
# ══════════════════════════════════════════════════════════════════════════════
# The built tmux is copied over the distribution's /usr/bin/tmux, so exactly
# one tmux is on PATH: the muxrd under test, the rig's tmux session and a
# `tmux attach` in an SSH shell all run the pinned release. Built ONLY when the
# `tmux` compose profile selects `target: runtime-tmux`.
FROM runtime AS runtime-tmux

ARG TMUX_VERSION=3.8
COPY --from=tmux-build /usr/local/bin/tmux /usr/bin/tmux
RUN set -eux; \
    [ "$(command -v tmux)" = /usr/bin/tmux ]; \
    tmux -V; \
    tmux -V | grep -q "tmux ${TMUX_VERSION}"

# Default this image to the tmux-only backend; the entrypoint exports
# MUXRD_BACKEND=tmux and MUXRD_TMUX_SOCKET to SSH logins.
ENV BACKEND=tmux

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# ══════════════════════════════════════════════════════════════════════════════
# Stage 6 — runtime-herdr-tmux: runtime-herdr (zellij + herdr) + the same tmux
#           → an image carrying ALL THREE backends
# ══════════════════════════════════════════════════════════════════════════════
# Built ONLY when the `all` compose profile selects `target: runtime-herdr-tmux`.
FROM runtime-herdr AS runtime-herdr-tmux

ARG TMUX_VERSION=3.8
COPY --from=tmux-build /usr/local/bin/tmux /usr/bin/tmux
RUN set -eux; \
    [ "$(command -v tmux)" = /usr/bin/tmux ]; \
    tmux -V; \
    tmux -V | grep -q "tmux ${TMUX_VERSION}"

# BACKEND=all: the entrypoint starts zellij, herdr and tmux and leaves
# MUXRD_BACKEND unset, so muxrd serves all three.
ENV BACKEND=all

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
