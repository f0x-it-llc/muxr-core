#!/usr/bin/env bash
# stop.sh — tear down the Muxr dev rig and reset its state.
#
# Usage:
#   ./docker/stop.sh [OPTIONS]
#
# By default this stops ALL rig variants (zellij, herdr, both), removes their
# containers, deletes the named volumes (token DB, TLS cert, zellij config,
# herdr workspace state, muxr-notify DB) and removes the rig images
# (muxr-grpc-rig / muxr-herdr-rig / muxr-both-rig) — so the next
# ./docker/run.sh re-pairs from a clean slate and rebuilds the image.
#
# Options:
#   --keep-data   Keep the named volumes: tokens, cert, and session state
#                 survive. Only containers + images are removed.
#   --keep-image  Keep the built rig images. Only containers (+ volumes,
#                 unless --keep-data) are removed.
#   --purge       Additionally prune the Docker BUILD CACHE
#                 (docker builder prune). This is daemon-wide — it also drops
#                 cached layers of unrelated projects — but it is the only way
#                 to guarantee the next build recompiles from scratch instead
#                 of reusing a cached cargo-build layer.
#   -h, --help    Show this help and exit.
#
# Examples:
#   # Full reset (tokens + images gone; next run.sh rebuilds & re-pairs):
#   ./docker/stop.sh
#
#   # Stop the rig but keep tokens/cert so the phone stays paired:
#   ./docker/stop.sh --keep-data
#
#   # Nuke everything including the layer cache (guaranteed fresh compile):
#   ./docker/stop.sh --purge

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="${SCRIPT_DIR}/compose.yaml"

usage() {
  grep '^#' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
  exit 0
}

KEEP_DATA=0
KEEP_IMAGE=0
PURGE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep-data)  KEEP_DATA=1;  shift ;;
    --keep-image) KEEP_IMAGE=1; shift ;;
    --purge)      PURGE=1;      shift ;;
    -h|--help)    usage ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      ;;
  esac
done

# Same daemon-access rule as run.sh: sudo only when the current user can't
# reach the Docker daemon directly. Keeping the two scripts consistent matters —
# rootful and rootless Docker are SEPARATE daemons with separate images,
# volumes, and build caches, so cleaning up via a different daemon than the one
# run.sh built on would remove nothing (and is one way a "really old" cached
# rig keeps coming back).
DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  echo "[stop.sh] no direct Docker access — falling back to 'sudo docker'"
  DOCKER=(sudo docker)
fi

# --profile herdr --profile both widens `down` to cover every rig variant, not
# just the default zellij service.
COMPOSE=("${DOCKER[@]}" compose -f "${COMPOSE_FILE}" --profile herdr --profile both)

DOWN_ARGS=(--remove-orphans)
[[ "${KEEP_DATA}" -eq 0 ]]  && DOWN_ARGS+=(--volumes)
[[ "${KEEP_IMAGE}" -eq 0 ]] && DOWN_ARGS+=(--rmi all)

echo "[stop.sh] tearing down all rig variants (zellij + herdr + both)…"
[[ "${KEEP_DATA}" -eq 0 ]]  && echo "[stop.sh]   removing volumes (token DB, cert, session state — re-pair needed)"
[[ "${KEEP_DATA}" -eq 1 ]]  && echo "[stop.sh]   keeping volumes (tokens/cert/session state preserved)"
[[ "${KEEP_IMAGE}" -eq 0 ]] && echo "[stop.sh]   removing rig images (next run.sh rebuilds)"

"${COMPOSE[@]}" down "${DOWN_ARGS[@]}"

if [[ "${PURGE}" -eq 1 ]]; then
  echo "[stop.sh] pruning Docker build cache (daemon-wide)…"
  "${DOCKER[@]}" builder prune -f
fi

echo "[stop.sh] done. Start fresh with:  ./docker/run.sh [--herdr|--both] [--host <ip>]"
