#!/usr/bin/env bash
# Headless entrypoint for the Muxr PUBLIC DEMO server.
#
# Runs as the unprivileged `demo` user (uid 10001; see the sibling Dockerfile's
# `USER demo`). Provisions a TLS cert + two stable named auth tokens (minted
# ONCE and reused across restarts — see below), starts a bar-less zellij
# session AND a headless herdr space so both backends show up in the app's
# session list, prints the pairing URI(s) + an ANSI QR (to the container log
# AND to $HOME/pairing.txt), then runs muxrd in the foreground.
#
# There is NO interactive control surface (no muxrctl, no SSH). Everything the
# reviewer/checkout user can reach is the sandboxed terminal(s) the app
# attaches to.
#
# ── Why mint-once / cert-reuse, not "regenerate every boot" ─────────────────
# Ed's requirement is a single auth token reusable by many users who "check
# out" the demo. muxrd already supports many independent session tokens from
# one auth token (Login is repeatable) — what breaks reuse across a restart is
# DURABILITY of two things, not one:
#   (a) the auth token, if minted fresh every boot;
#   (b) the TLS cert, if regenerated every boot — the pairing URI PINS its
#       fingerprint (tm=pin&fp=<sha256>), so a published QR dies on ANY
#       restart regardless of the token.
# Both are fixed by persisting $XDG_DATA_HOME (the volume) and making this
# script mint-once/reuse instead of throwaway-every-boot:
#   - the TLS cert is REUSED whenever `muxrd init`'s own idempotency (the SAN
#     sidecar check in muxrd/src/tls.rs) finds the on-disk cert still covers
#     the requested SAN set — we never delete it first;
#   - each named token is minted ONCE; its plaintext secret (only ever printed
#     once, at mint time) is persisted to a 0600 file alongside tokens.db so a
#     later boot can recover it instead of re-minting under the same name
#     (which `create-token` would refuse — token names are unique).
set -euo pipefail

# ── Env vars ──────────────────────────────────────────────────────────────
SESSION="${DEMO_SESSION:-demo}"
# DEMO_HOST — the public IP/DNS the reviewer's app will dial. Injected into the
# TLS cert SAN (via `muxrd init --san`) so the self-signed cert validates for
# that address. Empty = loopback-only cert (fine for local testing). Changing
# this across a restart forces a cert REGENERATION (see step 2 below) — every
# previously published QR then goes stale, which is why we log it loudly.
DEMO_HOST="${DEMO_HOST:-}"
# Bind address, passed to `muxrd start --bind`. Using the CLI flag (not an env
# var) keeps the demo decoupled from muxrd's env-var names across versions.
BIND="${DEMO_BIND:-0.0.0.0:50051}"
PORT="${DEMO_PORT:-50051}"
# DEMO_READ_ONLY=1 (default) mints `demo-public` read-only — the published
# checkout token is view-only unless an operator explicitly opts in to
# read-write by setting this to 0.
READ_ONLY="${DEMO_READ_ONLY:-1}"
# DEMO_REVIEWER_TOKEN=1 additionally mints a read-write `demo-reviewer` token
# for store reviewers who need to type. Off by default — the published
# `demo-public` token is the only one most checkouts ever see.
REVIEWER_ENABLED="${DEMO_REVIEWER_TOKEN:-0}"
# Optional expiry applied to any token minted THIS boot (existing/reused
# tokens are unaffected). Same syntax as `muxrd create-token --expires-in`:
# `<n>s`/`m`/`h`/`d`, bare seconds, or unset (never expires).
DEMO_TOKEN_EXPIRES_IN="${DEMO_TOKEN_EXPIRES_IN:-}"

# Fixed image-layout paths (the shared contract with the sibling Dockerfile —
# not operator-configurable).
readonly DEMO_CLONE_DIR="/opt/demo/muxr-core"

# ── 0. Materialise the writable HOME tree on the tmpfs mount ─────────────────
umask 077
mkdir -p \
  "${XDG_DATA_HOME}/zellij" \
  "${XDG_DATA_HOME}/zellij/demo-secrets" \
  "${XDG_CONFIG_HOME}/zellij/layouts" \
  "${XDG_CACHE_HOME}" \
  "${XDG_RUNTIME_DIR}"
chmod 0700 "${XDG_RUNTIME_DIR}" "${XDG_DATA_HOME}/zellij/demo-secrets"

# Bar-less zellij config (no tab-bar/status-bar plugins), demo layout, + prompt.
cp /opt/demo/config/config.kdl "${XDG_CONFIG_HOME}/zellij/config.kdl"
cp /opt/demo/config/layout.kdl "${XDG_CONFIG_HOME}/zellij/layouts/muxr.kdl"
cp /opt/demo/config/demo.bashrc "${HOME}/.bashrc"

# ── 1. Wipe non-essential zellij state on the PERSISTED volume ──────────────
# Only the cert (muxrd/), the token DB (tokens.db / tokens_for_dev.db) and our
# own secret store (demo-secrets/) are meant to survive a restart. Anything
# else that might land directly under $XDG_DATA_HOME/zellij (e.g. a future
# zellij version's own session-resurrection cache — today that state lives
# under the cache dir, which is tmpfs here and already resets on its own) is
# wiped on every boot so "restarting resets session state" holds regardless.
find "${XDG_DATA_HOME}/zellij" -mindepth 1 -maxdepth 1 \
  ! -name muxrd ! -name tokens.db ! -name tokens_for_dev.db ! -name demo-secrets \
  -exec rm -rf {} +

# ── 2. TLS cert — REUSED across restarts unless the SAN set changes ─────────
san_args=()
if [ -n "${DEMO_HOST}" ]; then
  # Comma-separated list accepted; muxrd treats each as IP-or-DNS.
  IFS=',' read -ra _sans <<< "${DEMO_HOST}"
  for s in "${_sans[@]}"; do
    s="$(echo "$s" | xargs)"   # trim
    [ -n "$s" ] && san_args+=(--san "$s")
  done
fi
echo "[demo] provisioning TLS cert (SAN: ${DEMO_HOST:-<loopback only>})…"
# `muxrd init` is idempotent (muxrd/src/tls.rs: load_or_generate_identity
# reuses the on-disk cert+key whenever the SAN sidecar still covers the
# request); we never delete the cert first. Capture its own log line so we can
# re-state the reused/regenerated decision unmistakably — an operator who
# changes DEMO_HOST invalidates every previously published QR and must see it.
CERT_INIT_LOG="${XDG_RUNTIME_DIR}/muxrd-init.log"
muxrd init "${san_args[@]}" 2>&1 | tee "${CERT_INIT_LOG}"
if grep -q "loading existing cert" "${CERT_INIT_LOG}"; then
  echo "[demo] cert: REUSED — on-disk cert already covers this SAN set; the pairing fingerprint is UNCHANGED from the last boot."
elif grep -q "generating self-signed cert" "${CERT_INIT_LOG}"; then
  echo "[demo] cert: REGENERATED (first boot, or DEMO_HOST changed) — every previously published pairing QR is now INVALID."
else
  echo "[demo] cert: could not determine reuse/regenerate status from 'muxrd init' output above — treat the fingerprint below as authoritative."
fi

# ── 3. API tokens — MINT ONCE; secrets persist so restarts reuse them ───────
DEMO_SECRETS_DIR="${XDG_DATA_HOME}/zellij/demo-secrets"

# Is `name` already a row in `muxrd list-tokens`? Checked against the listing
# captured once below (data rows only — skip the 2-line header; a lone
# "No tokens found." line also skips clean via the same `tail`).
token_exists() {
  printf '%s\n' "${TOKENS_LISTING}" | tail -n +3 | awk '{print $1}' | grep -Fxq "$1"
}

# Mint `name` (read-only iff `$2` = "1") the first time it is seen, or recover
# its secret from the persisted store on every later boot. Prints the token
# secret to stdout (or an empty string if it exists but the secret cannot be
# recovered); all narration goes to stderr so command substitution stays clean.
mint_or_reuse_token() {
  local name="$1" ro="$2"
  local secret_file="${DEMO_SECRETS_DIR}/${name}.token"

  if token_exists "${name}"; then
    if [ -r "${secret_file}" ]; then
      echo "[demo] token '${name}': REUSED (secret recovered from ${secret_file})" >&2
      cat "${secret_file}"
    else
      echo "[demo] WARNING: token '${name}' already exists in the token DB but its persisted secret is missing at ${secret_file} — 'create-token' only prints a secret once, at mint time. Skipping the pairing URI for '${name}' this boot; run 'muxrd revoke-token ${name}' and restart to mint a fresh one." >&2
      printf ''
    fi
    return 0
  fi

  echo "[demo] token '${name}': minting (first time this name has been seen)…" >&2
  local ro_flag=()
  [ "${ro}" = "1" ] && ro_flag=(--read-only)
  local expires_flag=()
  [ -n "${DEMO_TOKEN_EXPIRES_IN}" ] && expires_flag=(--expires-in "${DEMO_TOKEN_EXPIRES_IN}")
  # `create-token` prints "  TOKEN: <uuid>" once (leading whitespace); capture it.
  local create_output
  create_output="$(muxrd create-token -n "${name}" "${ro_flag[@]}" "${expires_flag[@]}")"
  local token
  token="$(printf '%s\n' "${create_output}" | sed -n 's/^[[:space:]]*TOKEN: //p' | tail -1)"
  if [ -z "${token}" ]; then
    echo "[demo] FATAL: could not mint token '${name}'" >&2
    exit 1
  fi
  printf '%s' "${token}" > "${secret_file}"
  chmod 0600 "${secret_file}"
  echo "[demo] token '${name}': minted and persisted to ${secret_file}" >&2
  echo "${token}"
}

TOKENS_LISTING="$(muxrd list-tokens 2>/dev/null || true)"

PUBLIC_TOKEN="$(mint_or_reuse_token "demo-public" "${READ_ONLY}")"

REVIEWER_TOKEN=""
if [ "${REVIEWER_ENABLED}" = "1" ]; then
  REVIEWER_TOKEN="$(mint_or_reuse_token "demo-reviewer" "0")"
fi

# ── 4. Cert fingerprint + pairing URI(s) (v2) — byte-identical across restarts ──
CERT="${XDG_DATA_HOME}/zellij/muxrd/server.crt"
FP="$(openssl x509 -in "${CERT}" -outform DER 2>/dev/null | openssl dgst -sha256 -hex | sed 's/^.*= //')"

# base64url(no pad) of the token bytes, matching muxrctl's payload encoding.
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

HOST_PARAM="${DEMO_HOST%%,*}"   # first SAN is the advertised host
[ -z "${HOST_PARAM}" ] && HOST_PARAM="127.0.0.1"

# $1=token $2=ro("1"/"0") $3=URL-encoded display name → prints the v2 URI.
# tm=pin (self-signed + fingerprint pin) — the app pins exactly this cert.
build_pair_uri() {
  local token="$1" ro="$2" name="$3" token_b64
  token_b64="$(printf '%s' "${token}" | b64url)"
  printf 'muxr://pair?v=2&h=%s&p=%s&t=%s&ro=%s&n=%s&tm=pin&fp=%s' \
    "${HOST_PARAM}" "${PORT}" "${token_b64}" "${ro}" "${name}" "${FP}"
}

PUBLIC_URI=""
[ -n "${PUBLIC_TOKEN}" ] && PUBLIC_URI="$(build_pair_uri "${PUBLIC_TOKEN}" "${READ_ONLY}" "Muxr%20Demo")"

REVIEWER_URI=""
if [ -n "${REVIEWER_TOKEN}" ]; then
  REVIEWER_URI="$(build_pair_uri "${REVIEWER_TOKEN}" "0" "Muxr%20Demo%20Reviewer")"
fi

# Persist for `docker exec cat /run/demo/pairing.txt` retrieval.
{
  echo "Muxr demo pairing"
  echo "host        : ${HOST_PARAM}:${PORT}"
  echo "fingerprint : ${FP}"
  echo
  if [ -n "${PUBLIC_TOKEN}" ]; then
    echo "[demo-public] read-only=${READ_ONLY}"
    echo "  token       : ${PUBLIC_TOKEN}"
    echo "  pairing URI : ${PUBLIC_URI}"
  else
    echo "[demo-public] secret unavailable this boot — see WARNING above"
  fi
  if [ "${REVIEWER_ENABLED}" = "1" ]; then
    echo
    if [ -n "${REVIEWER_TOKEN}" ]; then
      echo "[demo-reviewer] read-write"
      echo "  token       : ${REVIEWER_TOKEN}"
      echo "  pairing URI : ${REVIEWER_URI}"
    else
      echo "[demo-reviewer] secret unavailable this boot — see WARNING above"
    fi
  fi
} > "${HOME}/pairing.txt"

# ── 5. Seed BOTH backends before muxrd starts, so both appear in the app ────
ZELLIJ_UP=0
HERDR_UP=0

start_zellij_session() {
  echo "[demo] starting bar-less zellij session '${SESSION}'…"
  if zellij --layout muxr attach --create-background "${SESSION}"; then
    ZELLIJ_UP=1
  elif zellij attach --create-background "${SESSION}"; then
    ZELLIJ_UP=1
  else
    echo "############################################################" >&2
    echo "[demo] ERROR: could not start the zellij session '${SESSION}' — continuing without it (herdr may still be usable)" >&2
    echo "############################################################" >&2
  fi
}

start_herdr_backend() {
  local socket="${HERDR_SOCKET_PATH}"
  echo "[demo] starting headless herdr server…"
  herdr server > "${XDG_RUNTIME_DIR}/herdr-server.log" 2>&1 &
  # Wait for the API socket to appear (herdr derives the wire socket alongside).
  for _ in $(seq 1 50); do [ -S "${socket}" ] && break; sleep 0.1; done
  if [ -S "${socket}" ]; then
    HERDR_UP=1
    herdr status server 2>&1 | sed 's/^/[demo][herdr] /' || true
    # Seed one demo space idempotently (skip if it already exists across restarts).
    local existing
    existing="$(herdr workspace list 2>/dev/null || true)"
    if ! printf '%s\n' "${existing}" | grep -q "${SESSION}"; then
      echo "[demo] seeding herdr workspace '${SESSION}'…"
      herdr workspace create --label "${SESSION}" --cwd "${DEMO_CLONE_DIR}" --focus 2>/dev/null || true
    fi
  else
    echo "############################################################" >&2
    echo "[demo] ERROR: herdr socket ${socket} did not appear — see ${XDG_RUNTIME_DIR}/herdr-server.log; continuing without herdr (zellij may still be usable)" >&2
    echo "############################################################" >&2
  fi
}

# Default matches muxrd's own resolution (HERDR_SOCKET_PATH env, else
# $XDG_CONFIG_HOME/herdr/herdr.sock) so the headless server we start here and
# the muxrd we exec into later agree on the same socket.
export HERDR_SOCKET_PATH="${HERDR_SOCKET_PATH:-${XDG_CONFIG_HOME}/herdr/herdr.sock}"
start_herdr_backend
start_zellij_session

# ── 6. Banner + QR to the container log ──────────────────────────────────────
BACKENDS_LINE="zellij:$([ "${ZELLIJ_UP}" = 1 ] && echo up || echo DOWN) herdr:$([ "${HERDR_UP}" = 1 ] && echo up || echo DOWN)"
POSTURE_LINE="demo-public $([ "${READ_ONLY}" = 1 ] && echo "read-only (view)" || echo "read-write (interactive)")"
[ "${REVIEWER_ENABLED}" = "1" ] && POSTURE_LINE="${POSTURE_LINE}, demo-reviewer read-write"

cat <<BANNER

╔══════════════════════════════════════════════════════════════════╗
║  Muxr DEMO server — scan to pair the app                          ║
╠══════════════════════════════════════════════════════════════════╣
  host        : ${HOST_PARAM}:${PORT}
  posture     : ${POSTURE_LINE}
  fingerprint : ${FP}
  backends    : ${BACKENDS_LINE}
  pairing URI(s) also saved to ${HOME}/pairing.txt
                (docker exec <ctr> cat ${HOME}/pairing.txt)
╚══════════════════════════════════════════════════════════════════╝
BANNER

if [ -n "${PUBLIC_URI}" ]; then
  qrencode -t ANSIUTF8 "${PUBLIC_URI}" 2>/dev/null || echo "(install qrencode to render the QR; URI saved to pairing.txt)"
else
  echo "(no demo-public pairing URI available this boot — see WARNING above)"
fi
echo

# ── 7. Serve muxrd in the foreground (keeps the container alive) ─────────────
# No --backend restriction: muxrd auto-detects and serves every backend it
# finds usable (zellij and/or herdr), matching whatever step 5 brought up.
echo "[demo] starting muxrd on ${BIND}…"
exec muxrd start --bind "${BIND}"
