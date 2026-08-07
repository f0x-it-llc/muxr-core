#!/usr/bin/env bash
# ssh.sh — SSH into the Muxr dev rig without host-key friction.
#
# Usage:
#   ./docker/ssh.sh [HOST] [SSH_PORT]     # defaults: 127.0.0.1 2222
#
# The rig is a THROWAWAY environment: SSH host keys are generated at image
# build time, so every stop.sh + rebuild mints a new host identity. Pinning it
# in ~/.ssh/known_hosts therefore guarantees a scary REMOTE HOST
# IDENTIFICATION HAS CHANGED! failure on the next rebuild. This wrapper skips
# known_hosts entirely (dev rig only — never do this for a real server):
#   -o StrictHostKeyChecking=no      accept whatever key the rig presents
#   -o UserKnownHostsFile=/dev/null  and never record it
#
# A TTY is forced (-t) because muxrctl is a TUI.
#
# Examples:
#   ./docker/ssh.sh                     # loopback rig
#   ./docker/ssh.sh 192.168.1.50        # LAN rig
#   ./docker/ssh.sh 100.x.y.z 2222      # tailnet rig (your tailnet IP)
set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-2222}"

exec ssh -t \
  -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null \
  -o LogLevel=ERROR \
  -p "${PORT}" "root@${HOST}"
