# ─────────────────────────────────────────────────────────────────────────────
# Muxr — PUBLIC DEMO server (App Store / Play Store review sandbox)
#
# A locked-down, multi-tenant-safe muxrd instance whose ONLY purpose is to let
# a store reviewer (or anyone with the published pairing link) attach the
# mobile app and drive a real — but heavily sandboxed — terminal. muxrd serves
# BOTH backends (zellij 0.45.1 AND herdr), so either session type shows up in
# the app's session list.
#
# Like the muxr-core dev rig, this image does NOT build from source. It
# installs the official prebuilt `muxrd` release via the published install.sh
# (checksum-verified), so no Rust toolchain is needed at build time. The BUILD
# CONTEXT is this directory only (like `website/`, unlike `docker/` whose
# compose sets `context: ..`): every COPY below names a file that lives in
# demo/, and everything else is downloaded or cloned at build time.
#
# Threat model: an anonymous reviewer (or member of the public, once the
# pairing link is shared) pairs the app and gets an interactive shell inside
# this container. Everything below assumes that shell is HOSTILE:
#   • runs as an unprivileged user (uid 10001) — never root
#   • NO sudo, NO su, NO sshd, NO setuid/setgid binaries (stripped at build time)
#   • NO compilers, NO package managers usable by the demo user
#   • the muxr-core clone is read-only (nvim opens it but `:w` fails)
#   • writable state is tmpfs (HOME) except one persisted volume for the TLS
#     cert + auth token DB, mounted at $XDG_DATA_HOME — see demo/entrypoint.sh
#   • capabilities dropped, no-new-privileges, pids/mem/cpu capped — compose.yaml
#   • egress must be firewalled at the VM — see README.md
#
#   docker compose -f demo/compose.yaml up --build
# ─────────────────────────────────────────────────────────────────────────────
FROM debian:trixie-slim AS demo

ENV DEBIAN_FRONTEND=noninteractive \
    TERM=xterm-256color \
    LANG=C.UTF-8 \
    EDITOR=nvim

# Pin the muxr-core release the demo clones + installs. Bump alongside the app.
ARG MUXR_VERSION="0.4.1"

# Pin the herdr release muxrd is TESTED against (HERDR_MAX_TESTED_PROTOCOL in
# muxrd/src/multiplexer/herdr/wire.rs — currently wire protocol 22). muxrd
# serves both zellij AND herdr, so a reviewer sees both session kinds.
ARG HERDR_VERSION="0.9.0"

# Pin neovim >= 0.12 (release tarball, NOT apt — see the install step below for
# why apt's 0.10.4 breaks NvChad). NvChad's starter itself only floors at
# 0.11, but NvChad `branch = "v2.5"` pulls nvim-treesitter's rewritten "main"
# branch, whose lua/nvim-treesitter/config.lua calls vim.list.unique() — an
# API only present from nvim 0.12. 0.11.x satisfies NvChad's own stated floor
# but fails THIS build with "attempt to index field 'list' (a nil value)";
# verified empirically against both 0.11.4 and 0.12.5 before picking this pin.
ARG NVIM_VERSION="0.12.5"

# Pin the tree-sitter CLI. nvim-treesitter's rewritten install path shells out
# to `tree-sitter build` to compile each parser (the classic direct-`cc`
# invocation, and the `:TSInstallSync` command entirely, are both gone from
# this branch) — build-time only, purged with the C compiler once parsers
# are built.
ARG TREE_SITTER_VERSION="0.27.0"

# Pin opencode (release tarball; latest at time of writing). Self-contained
# ~60MB binary that embeds Bun — no node/bun needed.
ARG OPENCODE_VERSION="1.18.29"

# ── Curated VIEW-ONLY toolset (permanent) ────────────────────────────────────
# Deliberately EXCLUDED (attack-surface reduction): openssh-server, sudo,
# build-essential, python3-pip, nodejs, npm, tmux, wget. Neovim is installed
# from a release tarball below, NOT apt (trixie's apt neovim is 0.10.4, too
# old for NvChad — see the NVIM_VERSION comment above).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates coreutils \
      locales procps iproute2 less \
      bash \
      btop htop \
      ripgrep fd-find fzf bat tree jq ncdu \
      git \
      qrencode openssl \
      cmatrix sl cowsay fortune-mod fastfetch \
    && ln -sf /usr/bin/batcat /usr/local/bin/bat \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# ── Build-time-only toolchain (purged near the end of this file) ────────────
# curl/xz-utils fetch+extract every release below; build-essential compiles
# treesitter parsers (via the tree-sitter CLI, which shells out to `cc`).
#
# NOTE on `tar`: the retired demo's own header comment claimed curl/tar/xz are
# "purged afterwards", but `tar` is a Debian Essential:yes package — dpkg
# itself depends on it, and `apt-get purge tar` refuses outright ("dpkg:
# Depends: tar (>= 1.28-1) but it is not going to be installed"), verified
# against this exact base image. So unlike curl/xz-utils/build-essential,
# `tar` is never installed here as a "build-time-only" package to begin with —
# it ships with the base image and stays, matching Debian's own constraint
# rather than the stale claim.
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl xz-utils build-essential \
    && rm -rf /var/lib/apt/lists/*

# ── Zellij PINNED v0.45.1 (the version muxrd is compiled against) ────────────
# muxrd refuses to start on any other version — do NOT change this without
# also updating docker/Dockerfile's identical pin.
#
# Architecture-aware: `TARGETARCH` is `amd64`/`arm64`, while zellij names its
# assets after Rust target triples — hence the mapping to `x86_64`/`aarch64`.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) zj_arch=x86_64 ;; \
      arm64) zj_arch=aarch64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
      "https://github.com/zellij-org/zellij/releases/download/v0.45.1/zellij-${zj_arch}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/zellij; \
    zellij --version; \
    zellij --version | grep -q "0.45.1" || { echo "ERROR: zellij version is not 0.45.1" >&2; exit 1; }

# ── herdr PINNED $HERDR_VERSION (second backend; muxrd serves both) ──────────
# herdr spells its ARM asset `aarch64` (like zellij, unlike lazygit — not used
# in this image, but see docker/Dockerfile for that contrast).
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) herdr_arch=x86_64 ;; \
      arm64) herdr_arch=aarch64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/herdr \
      "https://github.com/herdrdev/herdr/releases/download/v${HERDR_VERSION}/herdr-linux-${herdr_arch}"; \
    chmod +x /usr/local/bin/herdr; \
    head -c 64 /usr/local/bin/herdr | grep -q ELF; \
    echo "herdr ${HERDR_VERSION} installed (wire protocol negotiated at runtime)"

# ── Neovim PINNED $NVIM_VERSION (release tarball; neovim spells its ARM asset
# `arm64`, NOT `aarch64` like zellij/herdr — mapped on its own, per the
# per-download idiom) ────────────────────────────────────────────────────────
# Extracted to /opt/nvim (stripping the arch-named top-level directory the
# tarball ships) rather than unpacked over /usr, so the whole install — and
# the NvChad config baked onto it below — lives at one predictable, read-only
# path unaffected by anything under /usr.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) nvim_arch=x86_64 ;; \
      arm64) nvim_arch=arm64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/nvim.tar.gz \
      "https://github.com/neovim/neovim/releases/download/v${NVIM_VERSION}/nvim-linux-${nvim_arch}.tar.gz"; \
    mkdir -p /opt/nvim; \
    tar -xzf /tmp/nvim.tar.gz -C /opt/nvim --strip-components=1; \
    rm -f /tmp/nvim.tar.gz; \
    /opt/nvim/bin/nvim --version; \
    /opt/nvim/bin/nvim --version | grep -q "NVIM v${NVIM_VERSION}" || { echo "ERROR: nvim version is not ${NVIM_VERSION}" >&2; exit 1; }

# ── tree-sitter CLI PINNED $TREE_SITTER_VERSION (build-time only) ───────────
# tree-sitter spells its ARM asset `arm64` (like neovim), and ships a plain
# gzipped binary (no tar wrapper) rather than a tarball.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) ts_arch=x64 ;; \
      arm64) ts_arch=arm64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
      "https://github.com/tree-sitter/tree-sitter/releases/download/v${TREE_SITTER_VERSION}/tree-sitter-linux-${ts_arch}.gz" \
      | gunzip -c > /usr/local/bin/tree-sitter; \
    chmod +x /usr/local/bin/tree-sitter; \
    tree-sitter --version

# ── opencode PINNED $OPENCODE_VERSION (release tarball) ──────────────────────
# NO API key shipped, NO egress exception added for it (Ed's explicit
# decision) — opencode shows its login/onboarding screen when launched with no
# network and no key configured; that is expected sandboxing, not a bug.
# opencode spells its ARM asset `arm64` and its x86_64 asset `x64`.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) oc_arch=x64 ;; \
      arm64) oc_arch=arm64 ;; \
      *) echo "ERROR: unsupported TARGETARCH '${TARGETARCH}'" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
      "https://github.com/sst/opencode/releases/download/v${OPENCODE_VERSION}/opencode-linux-${oc_arch}.tar.gz" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/opencode; \
    opencode --version

# ── muxrd + muxrctl (via the official, checksum-verified install.sh) ────────
# Unchanged mechanism from the retired demo: install.sh supports --version and
# MUXR_INSTALL_DIR (and arm64 natively). It ERRORS if muxrctl is not
# executable post-install, so `rm -f muxrctl` must run AFTER install.sh
# completes — the demo has no control surface by design (no muxrctl, no SSH).
RUN MUXR_INSTALL_DIR=/usr/local/bin bash -c \
      'curl -fsSL https://raw.githubusercontent.com/f0x-it-llc/muxr-core/main/install.sh \
         | bash -s -- ${MUXR_VERSION:+--version "$MUXR_VERSION"}' \
    && rm -f /usr/local/bin/muxrctl \
    && muxrd --version

# ── nvim wrapper: point neovim's config resolution at the read-only, baked
# NvChad install instead of $XDG_CONFIG_HOME ─────────────────────────────────
# $XDG_CONFIG_HOME is tmpfs (/run/demo/.config, wiped every restart — see the
# ENV block below), so NvChad's config can't live there: it would need a full
# `Lazy sync` + treesitter parser build on every container restart, which is
# both slow and impossible with no runtime egress. This wrapper overrides
# $XDG_CONFIG_HOME to /opt/demo (giving `stdpath("config")` = /opt/demo/nvim,
# where NvChad is cloned below) for nvim ONLY, leaving the container-wide
# $XDG_CONFIG_HOME untouched for zellij. Verified empirically: a plain
# `set runtimepath^=...` + manual `:source init.lua` (no env override) does
# NOT survive lazy.nvim's own setup(), which resets 'runtimepath' back to
# whatever `stdpath("config")` already says — so the override has to happen
# before nvim resolves stdpath, not after, via a manually-extended rtp.
RUN printf '%s\n' \
      '#!/bin/sh' \
      '# Muxr demo nvim wrapper — see demo/Dockerfile for why this exists.' \
      'export XDG_CONFIG_HOME=/opt/demo' \
      'exec /opt/nvim/bin/nvim "$@"' \
      > /usr/local/bin/nvim \
    && chmod 0755 /usr/local/bin/nvim

# ── Unprivileged demo user (HOME on tmpfs at runtime — see compose.yaml) ─────
RUN groupadd -g 10001 demo \
    && useradd -u 10001 -g 10001 -M -d /run/demo -s /bin/bash demo

# ── Read-only muxr-core clone (real public repo; root-owned, chmod a-w below)
# FULL clone (not shallow) — the whole point is a real `git log`/`git show`,
# and the .git directory is a couple MB over 53 commits. Replaces the old
# demo's synthetic /opt/demo/projects/api entirely.
RUN git clone https://github.com/f0x-it-llc/muxr-core /opt/demo/muxr-core

# git refuses to operate on a repository it doesn't own ("detected dubious
# ownership") once the EUID reading it differs from the owner on disk — and
# this clone is root-owned by contract (see the shared path/env comment
# above), while `git log`/`git show` here run as the unprivileged demo user
# (layout.kdl's "logs" tab). A per-user `~/.gitconfig` exception can't work
# either, since $HOME is the tmpfs wiped every restart. `--system` writes
# /etc/gitconfig instead: root-owned, world-readable, survives restarts,
# and doesn't touch the clone's own required ownership or permissions.
RUN git config --system --add safe.directory /opt/demo/muxr-core

# ── NvChad pre-bootstrapped + treesitter parsers pre-installed ──────────────
# FAILS THE BUILD ON ERROR — no `2>/dev/null`, no `|| true`. The dev rig's
# `nvim --headless "+Lazy! sync" +qa 2>/dev/null || true` (docker/Dockerfile)
# is precisely how an incompatible neovim/NvChad pairing shipped unnoticed;
# every command below is checked and the RUN fails loudly on any Lua error.
#
# XDG_DATA_HOME is set here (matching the runtime ENV block below) so
# lazy.nvim's plugin installs and nvim-treesitter's parser installs land
# straight into /var/lib/demo — the same path the persisted named volume
# mounts onto at runtime. Docker prepopulates a fresh named volume from the
# image's existing content at its mountpoint on first `docker run`, so this
# bakes an instant first nvim attach with no runtime egress required, exactly
# like the muxrd cert/token durability this same path was chosen for (see
# demo/entrypoint.sh).
RUN git clone --depth 1 https://github.com/NvChad/starter /opt/demo/nvim \
    && rm -rf /opt/demo/nvim/.git
ENV XDG_DATA_HOME=/var/lib/demo
RUN set -eu; \
    run_nvim_checked() { \
      desc="$1"; shift; \
      out="$(XDG_CONFIG_HOME=/opt/demo HOME=/root /opt/nvim/bin/nvim --headless "$@" 2>&1)"; \
      printf '%s\n' "$out"; \
      if printf '%s' "$out" | grep -qiE 'error detected|error in command line|E[0-9]{3,4}:|lua: '; then \
        echo "ERROR: $desc reported a Lua/startup error (see output above)" >&2; \
        exit 1; \
      fi; \
    }; \
    run_nvim_checked "NvChad Lazy sync" "+Lazy! sync" +qa; \
    run_nvim_checked "treesitter parser install" \
      -c "lua require('nvim-treesitter.config').setup({})" \
      -c "lua require('nvim-treesitter.install').install({'rust','lua','bash','markdown'}, {summary=true}):wait()" \
      -c qa; \
    run_nvim_checked "checkhealth" '+checkhealth' +qa

# ── Bar-less zellij config + demo layout + login profile + entrypoint (from
# this directory; the image is self-contained — no muxr-core checkout needed
# for these). Staged root-owned/a-w; the entrypoint installs them into the
# tmpfs config volume on every boot (they can't live there permanently — see
# the nvim wrapper comment above for why the same is true of NvChad).
COPY config.kdl /opt/demo/config/config.kdl
COPY layout.kdl /opt/demo/config/layout.kdl
COPY demo.bashrc /opt/demo/config/demo.bashrc
COPY entrypoint.sh /usr/local/bin/demo-entrypoint.sh
RUN chmod 0755 /usr/local/bin/demo-entrypoint.sh

# ── Purge build-time-only tooling (curl/xz-utils/build-essential/tree-sitter
# CLI) now that every download and parser build above is done ───────────────
RUN apt-get purge -y curl xz-utils build-essential \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/* \
    && rm -f /usr/local/bin/tree-sitter

# $XDG_DATA_HOME is the ONE writable-by-demo persisted path (muxrd's cert +
# zellij's token DB, both minted by the entrypoint as the demo user — plus the
# NvChad data baked in above). Hand it to the demo user now that every
# build-time write into it is done.
RUN chown -R 10001:10001 /var/lib/demo

# ── HARDENING: lock down every staged/cloned path, then neutralise every
# setuid/setgid binary in the image (run last, after every install above) ───
RUN chmod -R a-w /opt/demo \
    && find / -xdev \( -perm -4000 -o -perm -2000 \) -type f -exec chmod ug-s {} +

# XDG_DATA_HOME was already pinned above (build-time NvChad data landed
# there); HOME + the remaining XDG paths point under the tmpfs mount so a
# read-only rootfs is possible — see compose.yaml.
ENV HOME=/run/demo \
    XDG_CONFIG_HOME=/run/demo/.config \
    XDG_CACHE_HOME=/run/demo/.cache \
    XDG_RUNTIME_DIR=/run/demo/xdg \
    DEMO_BIND=0.0.0.0:50051 \
    DEMO_SESSION=demo \
    DEMO_READ_ONLY=1

USER demo
WORKDIR /opt/demo/muxr-core

EXPOSE 50051
ENTRYPOINT ["/usr/local/bin/demo-entrypoint.sh"]
