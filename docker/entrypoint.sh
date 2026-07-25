#!/usr/bin/env bash
# Entrypoint for the Muxr dev rig.
#
# Boots the selected multiplexer backend(s) + an SSH server, then waits. You SSH
# in (no password) and run `muxrctl` to configure the cert + tokens and start the
# gRPC server:
#
#   ssh -t root@<host> -p <ssh-port>      # no password
#   muxrctl                             # Configure → Cert → Tokens → Server → Pair
#
# Environment variables:
#   BACKEND  — multiplexer backend(s) to boot:
#                `zellij` (default) | `herdr` | `both`
#              • zellij / herdr → muxrd is restricted to that ONE backend
#                (MUXRD_BACKEND is exported so the muxrctl-spawned daemon selects it).
#              • both           → start zellij AND a headless herdr server, and
#                leave MUXRD_BACKEND UNSET so muxrd auto-detects and serves BOTH
#                simultaneously (Phase 3 serve-all default). This is the on-device
#                multi-backend test rig.
#   SESSION  — session / herdr-workspace name  (default: backend-dev). In `both`
#              mode BOTH backends expose a session with this same name on purpose,
#              so you can verify same-name cross-backend routing (the app tells
#              them apart by the backend badge: zellij = green, herdr = blue).
#   NOTIFY_ENABLED — start muxr-notify in-container for e2e push-notification
#              testing (default: 1). Runs in FCM_MODE=log (no Firebase artifacts
#              needed) on loopback only; muxrd's MUXRD_NOTIFY_RELAY_URL is
#              pointed at it automatically. Set 0 to skip.
#   NOTIFY_PORT — loopback port muxr-notify listens on (default: 8090).
set -euo pipefail

BACKEND="${BACKEND:-zellij}"
SESSION="${SESSION:-backend-dev}"
HERDR_SOCKET_PATH="${HERDR_SOCKET_PATH:-/root/.config/herdr/herdr.sock}"
NOTIFY_ENABLED="${NOTIFY_ENABLED:-1}"
NOTIFY_PORT="${NOTIFY_PORT:-8090}"

case "${BACKEND}" in
  zellij|herdr|both) ;;
  *) echo "[rig] ERROR: BACKEND must be one of: zellij | herdr | both (got '${BACKEND}')" >&2; exit 1 ;;
esac

# Which backend(s) does this run drive?
want_zellij=0; want_herdr=0
[ "${BACKEND}" = "zellij" ] && want_zellij=1
[ "${BACKEND}" = "herdr" ]  && want_herdr=1
[ "${BACKEND}" = "both" ]   && { want_zellij=1; want_herdr=1; }

# ── 0. Propagate the rig env to SSH login shells ─────────────────────────────
# sshd does NOT pass the container's docker `ENV` to interactive sessions, so
# `muxrctl` run over SSH would not see them — and the muxrctl-spawned `muxrd`
# daemon inherits muxrctl's env. Mirror the relevant vars where SSH logins pick
# them up: /etc/environment (read by PAM for every session) and /etc/profile.d
# (sourced by login shells).
#
# Backend-selection rule (muxrd: CLI --backend > MUXRD_BACKEND env > serve-all):
#   • single backend (zellij/herdr) → export MUXRD_BACKEND to RESTRICT to it.
#   • both                          → DO NOT export MUXRD_BACKEND, so muxrd
#                                     auto-detects every available backend and
#                                     serves them all simultaneously.
# HERDR_SOCKET_PATH is exported whenever herdr is in play (herdr/both) so muxrd's
# herdr probe + the headless herdr server agree on the socket location.
#
# MUXRD_NOTIFY_RELAY_URL points muxrd at the in-container muxr-notify instance
# (started below by start_notify) so the notifier has a local e2e target
# instead of the production default (https://noti.muxr.app). Only exported
# when NOTIFY_ENABLED=1 (the default).
restrict_backend=""
[ "${BACKEND}" = "zellij" ] && restrict_backend="zellij"
[ "${BACKEND}" = "herdr" ]  && restrict_backend="herdr"

notify_relay_url=""
[ "${NOTIFY_ENABLED}" = "1" ] && notify_relay_url="http://127.0.0.1:${NOTIFY_PORT}"

{
  printf 'MUXRD_BIND=%s\nMUXRD_SAN=%s\n' \
    "${MUXRD_BIND:-}" "${MUXRD_SAN:-}"
  [ -n "${restrict_backend}" ] && printf 'MUXRD_BACKEND=%s\n' "${restrict_backend}"
  [ "${want_herdr}" -eq 1 ] && printf 'HERDR_SOCKET_PATH=%s\n' "${HERDR_SOCKET_PATH}"
  [ -n "${notify_relay_url}" ] && printf 'MUXRD_NOTIFY_RELAY_URL=%s\n' "${notify_relay_url}"
} > /etc/environment
{
  printf "export MUXRD_BIND='%s'\nexport MUXRD_SAN='%s'\n" \
    "${MUXRD_BIND:-}" "${MUXRD_SAN:-}"
  [ -n "${restrict_backend}" ] && printf "export MUXRD_BACKEND='%s'\n" "${restrict_backend}"
  [ -n "${notify_relay_url}" ] && printf "export MUXRD_NOTIFY_RELAY_URL='%s'\n" "${notify_relay_url}"
  [ "${want_herdr}" -eq 1 ] && printf "export HERDR_SOCKET_PATH='%s'\n" "${HERDR_SOCKET_PATH}"
} > /etc/profile.d/muxrd-env.sh
chmod 0644 /etc/profile.d/muxrd-env.sh

# ── 1. Clear root's password so SSH login needs no credential (dev rig) ──────
passwd -d root

# ── 2. Backend boot helpers ──────────────────────────────────────────────────
start_zellij() {
  # zellij: install the managed config (empty load_plugins so the zellij:link
  # background plugin never loads) into the persisted config volume, then start a
  # backgrounded session muxrd attaches to once started from muxrctl.
  mkdir -p /root/.config/zellij
  cp /usr/local/share/muxr/config.kdl /root/.config/zellij/config.kdl
  echo "[rig] starting zellij session '${SESSION}'…"
  zellij --layout muxr attach --create-background "${SESSION}" || true
}

start_herdr() {
  # herdr: a SEPARATE, UNMODIFIED, user-installed binary (AGPL-3.0). muxrd drives
  # it only over its public 0600 sockets; muxrd stays the TLS/bearer boundary.
  # Start a headless herdr server and seed 3 demo workspaces (spaces) so the
  # spaces menu is exercisable on device.
  export HERDR_SOCKET_PATH
  echo "[rig] starting headless herdr server…"
  herdr server > /var/log/herdr-server.log 2>&1 &
  # Wait for the API socket to appear (herdr derives the wire socket alongside).
  for _ in $(seq 1 50); do [ -S "${HERDR_SOCKET_PATH}" ] && break; sleep 0.1; done
  if [ -S "${HERDR_SOCKET_PATH}" ]; then
    herdr status server 2>&1 | sed 's/^/[rig][herdr] /' || true
    # Seed 3 demo workspaces idempotently (skip a label that already exists —
    # muxrd treats duplicate workspace labels as ambiguous).
    _existing="$(herdr workspace list 2>/dev/null || true)"
    if ! printf '%s\n' "${_existing}" | grep -q "main"; then
      echo "[rig] seeding herdr workspace 'main' (initial/focused)…"
      herdr workspace create --label main --cwd /root/projects/api --focus 2>/dev/null || true
      # Add a 2nd tab so tab-switching within a space is also demoable.
      herdr tab create --label editor --no-focus 2>/dev/null || true
    fi
    if ! printf '%s\n' "${_existing}" | grep -q "logs"; then
      echo "[rig] seeding herdr workspace 'logs'…"
      herdr workspace create --label logs --cwd /root/projects/api 2>/dev/null || true
    fi
    if ! printf '%s\n' "${_existing}" | grep -q "api"; then
      echo "[rig] seeding herdr workspace 'api'…"
      herdr workspace create --label api --cwd /root/projects/api 2>/dev/null || true
    fi
  else
    echo "[rig] WARNING: herdr socket ${HERDR_SOCKET_PATH} did not appear — see /var/log/herdr-server.log" >&2
  fi
}

# Describe the herdr actually installed in this image, for the banner.
#
# The rig no longer pins a herdr version and muxrd no longer pins a wire protocol
# (it discovers the server's via the JSON-API `ping`), so the banner must report
# what is really running instead of a hard-coded number. Falls back gracefully when
# an older herdr's `status server` does not print a protocol line.
herdr_wire_desc() {
  _hv="$(herdr --version 2>/dev/null | awk '{print $2}')"
  _hp="$(herdr status server 2>/dev/null | sed -n 's/^[[:space:]]*protocol:[[:space:]]*//p' | head -1)"
  if [ -n "${_hp}" ]; then
    printf 'herdr %s, wire protocol %s negotiated at runtime' "${_hv:-unknown}" "${_hp}"
  else
    printf 'herdr %s, wire protocol negotiated at runtime' "${_hv:-unknown}"
  fi
}

start_notify() {
  # muxr-notify: in-container push relay for e2e notification testing only.
  # FCM_MODE=log skips OAuth2 + the FCM HTTP call entirely and just logs the
  # would-be message — no Firebase service-account JSON needed. Loopback-only
  # (never published to the host); muxrd's MUXRD_NOTIFY_RELAY_URL (exported
  # above) is the only consumer. The DB lives under the persisted zellij data
  # volume so registered push-handles survive a rig restart.
  mkdir -p /root/.local/share/zellij/muxr-notify
  echo "[rig] starting muxr-notify (FCM_MODE=log) on 127.0.0.1:${NOTIFY_PORT}…"
  NOTIFY_LISTEN="127.0.0.1:${NOTIFY_PORT}" \
    NOTIFY_DB="/root/.local/share/zellij/muxr-notify/notify.db" \
    FCM_MODE=log \
    muxr-notify > /var/log/muxr-notify.log 2>&1 &
}

[ "${want_zellij}" -eq 1 ] && start_zellij
[ "${want_herdr}" -eq 1 ] && start_herdr
[ "${NOTIFY_ENABLED}" = "1" ] && start_notify

# ── 3. Connection banner ─────────────────────────────────────────────────────
if [ "${BACKEND}" = "both" ]; then
  backend_line="both (zellij + $(herdr_wire_desc)) — muxrd auto-detects & serves ALL (MUXRD_BACKEND unset)"
  session_line="${SESSION} (zellij) · herdr spaces: main*, logs, api"
elif [ "${BACKEND}" = "herdr" ]; then
  backend_line="$(herdr_wire_desc); muxrd restricted via MUXRD_BACKEND=herdr"
  session_line="spaces: main*, logs, api"
else
  backend_line="zellij"
  session_line="${SESSION}"
fi

if [ "${NOTIFY_ENABLED}" = "1" ]; then
  notify_line="muxr-notify (FCM_MODE=log) on 127.0.0.1:${NOTIFY_PORT} — MUXRD_NOTIFY_RELAY_URL set"
else
  notify_line="disabled (NOTIFY_ENABLED=0) — muxrd falls back to its configured/default relay"
fi

cat <<BANNER

╔══════════════════════════════════════════════════════════════════╗
║  Muxr dev rig is up — drive it with muxrctl over SSH     ║
╠══════════════════════════════════════════════════════════════════╣
  1. SSH in — no password (a TTY is required for the TUI — note the -t):
       ssh -t root@<host> -p <ssh-port>

  2. Run the control TUI and start the server:
       muxrctl
     → Configure → Cert → Tokens → Server (start) → Pair (scan the QR)

  backend        : ${backend_line}
  session/space  : ${session_line}
  push relay     : ${notify_line}
  gRPC port      : 50051 (published once you start the server)
╚══════════════════════════════════════════════════════════════════╝

BANNER

# ── 4. Run sshd in the foreground (keeps the container alive) ─────────────────
mkdir -p /run/sshd
echo "[rig] starting sshd (foreground)…"
exec /usr/sbin/sshd -D -e
