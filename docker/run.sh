#!/usr/bin/env bash
# run.sh — convenience wrapper for the Muxr dev rig.
#
# Usage:
#   ./docker/run.sh [OPTIONS] [-- EXTRA_COMPOSE_ARGS...]
#
# Options:
#   --host <IP>     Publish the gRPC + SSH ports on this host interface.
#                   Default: 127.0.0.1 (loopback — nothing exposed on the network).
#   --host=<IP>     Same, equals-sign form.
#   --port <N>      Host port to publish gRPC on. Default: 50051. Use this when
#                   50051 is already taken (e.g. a production muxrd on the same
#                   host). Equals-sign form accepted (--port=50052).
#   --ssh-port <N>  Host port to publish the rig's SSH on. Default: 2222.
#   --herdr         Run the herdr-backend rig instead of the default zellij rig
#                   (downloads a pinned, unmodified upstream herdr binary; muxrd
#                   drives it via `--backend herdr`). Run ONE rig at a time.
#   --both          Run the MULTI-backend rig: zellij AND herdr at once. muxrd
#                   auto-detects and serves BOTH simultaneously (Phase 3 serve-all
#                   — the on-device multi-backend test rig). Run ONE rig at a time.
#   -h, --help      Show this help and exit.
#
# Examples:
#   # Loopback only — safe for local testing (zellij backend):
#   ./docker/run.sh
#
#   # herdr backend rig (loopback):
#   ./docker/run.sh --herdr
#
#   # herdr rig on an alternate gRPC port (50051 busy with the prod muxrd):
#   ./docker/run.sh --herdr --port 50052 --host 100.x.y.z
#
#   # BOTH backends at once, exposed on the LAN for on-device testing:
#   ./docker/run.sh --both --host 192.168.1.50
#
#   # Expose on the LAN so a phone can connect:
#   ./docker/run.sh --host 192.168.1.50
#
#   # Build without cache, then run:
#   ./docker/run.sh --host 192.168.1.50 -- --no-deps
#
# Once it is up, SSH in (no password) and start the server with muxrctl:
#   ssh -t root@<host> -p 2222
#   muxrctl

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="${SCRIPT_DIR}/compose.yaml"

usage() {
  grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
  exit 0
}

BIND_ADDR="127.0.0.1"
HERDR=0
BOTH=0
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host=*)
      BIND_ADDR="${1#--host=}"
      shift
      ;;
    --host)
      shift
      BIND_ADDR="${1:?--host requires an IP/hostname argument}"
      shift
      ;;
    --port=*)
      GRPC_PORT="${1#--port=}"
      shift
      ;;
    --port)
      shift
      GRPC_PORT="${1:?--port requires a port number argument}"
      shift
      ;;
    --ssh-port=*)
      SSH_PORT="${1#--ssh-port=}"
      shift
      ;;
    --ssh-port)
      shift
      SSH_PORT="${1:?--ssh-port requires a port number argument}"
      shift
      ;;
    --herdr)
      HERDR=1
      shift
      ;;
    --both)
      BOTH=1
      shift
      ;;
    -h|--help)
      usage
      ;;
    --)
      shift
      EXTRA_ARGS+=("$@")
      break
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      ;;
  esac
done

if [[ "${HERDR}" -eq 1 && "${BOTH}" -eq 1 ]]; then
  echo "[run.sh] ERROR: --herdr and --both are mutually exclusive" >&2
  exit 1
fi

export BIND_ADDR
# Published host ports — compose.yaml reads these (defaults 50051 / 2222).
[[ -n "${GRPC_PORT:-}" ]] && export GRPC_PORT
[[ -n "${SSH_PORT:-}" ]] && export SSH_PORT

# Use sudo ONLY if the current user can't reach the Docker daemon directly
# (i.e. not in the `docker` group and not rootless). When `docker info` already
# works, calling sudo is pure overhead and a needless password prompt.
DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  echo "[run.sh] no direct Docker access — falling back to 'sudo docker' (add yourself to the 'docker' group to skip this)"
  DOCKER=(sudo docker)
fi

backend_label=zellij
[[ "${HERDR}" -eq 1 ]] && backend_label=herdr
[[ "${BOTH}" -eq 1 ]]  && backend_label="both (zellij+herdr)"

echo "[run.sh] BIND_ADDR=${BIND_ADDR}  backend=${backend_label}"
echo "[run.sh] publishing  gRPC ${BIND_ADDR}:${GRPC_PORT:-50051}  +  SSH ${BIND_ADDR}:${SSH_PORT:-2222}"
echo "[run.sh] after boot:  ssh -t root@${BIND_ADDR} -p ${SSH_PORT:-2222}  then run  muxrctl"
echo ""

# Profile-gated services are named explicitly so the default zellij service isn't
# also started (it would clash on the published ports).
if [[ "${BOTH}" -eq 1 ]]; then
  exec "${DOCKER[@]}" compose -f "${COMPOSE_FILE}" --profile both up --build muxrd-both "${EXTRA_ARGS[@]}"
elif [[ "${HERDR}" -eq 1 ]]; then
  exec "${DOCKER[@]}" compose -f "${COMPOSE_FILE}" --profile herdr up --build muxrd-herdr "${EXTRA_ARGS[@]}"
else
  exec "${DOCKER[@]}" compose -f "${COMPOSE_FILE}" up --build "${EXTRA_ARGS[@]}"
fi
