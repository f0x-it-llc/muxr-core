#!/usr/bin/env bash
# zellij-matrix.sh — live check of muxrd against an official zellij release.
#
# Downloads the zellij release binary, starts an isolated zellij session and
# an isolated muxrd, and runs muxrd's example clients against them. Exits
# non-zero when any check fails. Needs bash 3.2+, curl, tar, script(1) and a
# Rust toolchain; no containers. Runs on Linux (x86_64, aarch64) and macOS
# (arm64, x86_64).
#
# Usage:
#   scripts/zellij-matrix.sh [--keep] [--port N] <zellij-version>
#   scripts/zellij-matrix.sh [--keep] [--port N] --mixed <old> <new>
#
#   <zellij-version>   one zellij server, e.g. 0.46.0
#   --mixed <old> <new>  sessions of both versions in one socket directory,
#                      muxrd started with <new> first on PATH, plus a session
#                      of <old> that reuses a name <new> left in its session
#                      index
#   --keep             leave the work directory in place for inspection
#   --port N           port muxrd binds on 127.0.0.1 (default 50631)
#
# Environment:
#   ZELLIJ_BIN_<version with dots as underscores>
#                      path of a zellij binary to use instead of downloading,
#                      e.g. ZELLIJ_BIN_0_46_0=/opt/zellij-0.46.0/zellij
#   CARGO_TARGET_DIR   honoured when locating the built binaries
#
# Everything zellij and muxrd create lives in one short temporary directory:
# HOME, the zellij socket directory, muxrd's data directory, the temporary
# directory and the downloaded binaries. The directory must be short because
# unix socket paths are limited to about 100 bytes. The user's own zellij
# sessions and muxrd data are never read or written. The build is the
# exception: cargo runs first, with the caller's environment, and writes to
# the cargo target directory and cargo's own caches.
#
# MUXRD_SKIP_VERSION_CHECK is never set: muxrd's supported-version gate is
# part of what this checks. The version muxrd reports to clients is covered
# by unit tests; here the gate is observed through muxrd starting with the
# zellij backend.

set -u

usage() {
    sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
}

die() {
    echo "zellij-matrix: $*" >&2
    exit 2
}

KEEP=0
PORT=50631
MODE=single
VERSION=""
OLD=""
NEW=""

while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        --port)
            [ $# -ge 2 ] || die "--port needs a value"
            PORT=$2; shift 2 ;;
        --mixed)
            [ $# -ge 3 ] || die "--mixed needs <old> <new>"
            MODE=mixed; OLD=$2; NEW=$3; shift 3 ;;
        -h|--help) usage; exit 0 ;;
        -*) die "unknown option: $1" ;;
        *)
            [ -z "$VERSION" ] || die "more than one version given"
            VERSION=$1; shift ;;
    esac
done

if [ "$MODE" = single ] && [ -z "$VERSION" ]; then
    usage >&2
    exit 2
fi
if [ "$MODE" = mixed ] && [ -n "$VERSION" ]; then
    die "give either one version or --mixed <old> <new>"
fi
case "$PORT" in
    ''|*[!0-9]*) die "--port must be a number" ;;
esac
# A version is exactly MAJOR.MINOR.PATCH in digits: it becomes part of a
# variable name, a path and a URL.
require_version() {
    case "$1" in
        ''|*[!0-9.]*|.*|*.|*..*|*.*.*.*) die "not a zellij version: '$1'" ;;
        *.*.*) ;;
        *) die "not a zellij version: '$1'" ;;
    esac
}
if [ "$MODE" = single ]; then
    require_version "$VERSION"
else
    require_version "$OLD"
    require_version "$NEW"
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd) || die "cannot locate the repository root"
ESC=$(printf '\033')

# ── Work directory and teardown ──────────────────────────────────────────────

WORK=$(mktemp -d /tmp/mxm.XXXXXX) || die "cannot create a work directory"
LOGS="$WORK/log"
mkdir -p "$LOGS" "$WORK/home" "$WORK/sock" "$WORK/tmp"

NATIVE_PID=""
MUXRD_STARTED=0
FAILED=0
ZELLIJ_BINS=""

stop_native_client() {
    if [ -n "$NATIVE_PID" ]; then
        kill "$NATIVE_PID" 2>/dev/null
        wait "$NATIVE_PID" 2>/dev/null
        NATIVE_PID=""
    fi
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    stop_native_client
    if [ "$MUXRD_STARTED" = 1 ] && ! "$BIN_DIR/muxrd" stop >/dev/null 2>&1; then
        # muxrd's binary and command line are outside the work directory, so
        # a daemon that did not stop is found through this run's pid file.
        muxrd_pidfile=$(find "$WORK/home" -name muxrd.pid 2>/dev/null | head -n 1)
        if [ -n "$muxrd_pidfile" ]; then
            muxrd_pid=$(cat "$muxrd_pidfile" 2>/dev/null)
            case "$muxrd_pid" in
                ''|*[!0-9]*) ;;
                *) kill "$muxrd_pid" 2>/dev/null ;;
            esac
        fi
    fi
    # Scoped by ZELLIJ_SOCKET_DIR to the work directory: only the sessions
    # this run started can be seen, so the user's sessions cannot be touched.
    for zellij_bin in $ZELLIJ_BINS; do
        ZELLIJ_SOCKET_DIR="$WORK/sock" HOME="$WORK/home" \
            "$zellij_bin" kill-all-sessions --yes >/dev/null 2>&1
    done
    sleep 1
    # A zellij process this run started that is still alive was started from
    # a binary inside the work directory or names it in its command line.
    pkill -f "$WORK/" >/dev/null 2>&1
    if [ "$FAILED" != 0 ] || [ "$status" != 0 ]; then
        muxrd_log=$(find "$WORK/home" -name muxrd.log 2>/dev/null | head -n 1)
        if [ -n "$muxrd_log" ]; then
            echo "--- tail of muxrd.log" >&2
            tail -n 40 "$muxrd_log" >&2
        fi
    fi
    if [ "$KEEP" = 1 ]; then
        echo "work directory kept: $WORK"
    else
        rm -rf "$WORK"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ── zellij release binary ────────────────────────────────────────────────────

release_triple() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
        Linux/x86_64) echo x86_64-unknown-linux-musl ;;
        Linux/aarch64|Linux/arm64) echo aarch64-unknown-linux-musl ;;
        Darwin/arm64) echo aarch64-apple-darwin ;;
        Darwin/x86_64) echo x86_64-apple-darwin ;;
        *) return 1 ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Print the path of a zellij binary of the given version, downloading and
# verifying it unless ZELLIJ_BIN_<version> names one.
zellij_binary() {
    version=$1
    override_name="ZELLIJ_BIN_$(echo "$version" | tr '.' '_')"
    override=$(eval "printf '%s' \"\${$override_name:-}\"")
    dest="$WORK/bin-$version"
    mkdir -p "$dest"
    if [ -n "$override" ]; then
        [ -x "$override" ] || { echo "zellij-matrix: $override_name=$override is not executable" >&2; return 1; }
        cp "$override" "$dest/zellij" || return 1
    else
        triple=$(release_triple) || { echo "zellij-matrix: unsupported platform $(uname -s)/$(uname -m)" >&2; return 1; }
        base="https://github.com/zellij-org/zellij/releases/download/v$version/zellij-$triple"
        curl -fsSL --retry 3 -o "$dest/zellij.tar.gz" "$base.tar.gz" \
            || { echo "zellij-matrix: download failed: $base.tar.gz" >&2; return 1; }
        curl -fsSL --retry 3 -o "$dest/zellij.sha256sum" "$base.sha256sum" \
            || { echo "zellij-matrix: download failed: $base.sha256sum" >&2; return 1; }
        tar -xzf "$dest/zellij.tar.gz" -C "$dest" || return 1
        # The checksum file lists the hash of the binary inside the archive;
        # its path column is a build path, so only the hash is compared.
        expected=$(awk '{print $1; exit}' "$dest/zellij.sha256sum")
        actual=$(sha256_of "$dest/zellij")
        if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
            echo "zellij-matrix: checksum mismatch for zellij $version ($triple)" >&2
            return 1
        fi
    fi
    reported=$("$dest/zellij" --version 2>/dev/null)
    if [ "$reported" != "zellij $version" ]; then
        echo "zellij-matrix: expected 'zellij $version', the binary reports '$reported'" >&2
        return 1
    fi
    echo "$dest/zellij"
}

# ── Checks ───────────────────────────────────────────────────────────────────

# run_check <name> <command...>: run the command with its output captured,
# print PASS/FAIL, and on failure show the end of the output.
run_check() {
    name=$1
    shift
    log="$LOGS/$(echo "$name" | tr -c 'A-Za-z0-9._\n-' '_').log"
    if "$@" >"$log" 2>&1; then
        echo "PASS $name"
    else
        echo "FAIL $name"
        sed "s/$ESC\[[0-9;?]*[A-Za-z]//g" "$log" | tail -n 25 | sed 's/^/    /'
        FAILED=1
    fi
}

# Start a background session with the given zellij binary and wait until its
# server reports a terminal pane: a session is listed, and answers a tab
# query, before its first pane exists.
start_session() {
    zellij_bin=$1
    session=$2
    "$zellij_bin" attach --create-background "$session" >"$LOGS/create-$session.log" 2>&1 || return 1
    tries=0
    while [ "$tries" -lt 40 ]; do
        if "$zellij_bin" --session "$session" action list-panes 2>/dev/null \
            | sed "s/$ESC\[[0-9;?]*[A-Za-z]//g" | grep -q '^terminal_'; then
            return 0
        fi
        tries=$((tries + 1))
        sleep 0.5
    done
    return 1
}

# Attach a native zellij client in a pseudo-terminal, in the background, so
# the session has had a client. A new session has its first tab either way,
# but zellij 0.46 does not create a FURTHER tab in a session that no client
# has attached to yet, and the tab checks create one. Fails when the client
# is not alive after the wait.
attach_native_client() {
    session=$1
    case "$(uname -s)" in
        Darwin) script -q /dev/null zellij attach "$session" >/dev/null 2>&1 </dev/null & ;;
        *) script -qc "zellij attach $session" /dev/null >/dev/null 2>&1 </dev/null & ;;
    esac
    NATIVE_PID=$!
    sleep 3
    kill -0 "$NATIVE_PID" 2>/dev/null
}

# Kill a session and wait until its server no longer answers.
kill_session() {
    zellij_bin=$1
    session=$2
    "$zellij_bin" kill-session "$session" || return 1
    tries=0
    while [ "$tries" -lt 20 ]; do
        # A killed session may stay listed as exited (it can be resurrected);
        # it is gone when the listing has no live entry of that name. A
        # listing that fails proves nothing, except zellij's own way of
        # saying that no session is left.
        if listing=$("$zellij_bin" list-sessions --no-formatting 2>&1); then
            if ! printf '%s\n' "$listing" | sed "s/$ESC\[[0-9;?]*[A-Za-z]//g" \
                | grep -v 'EXITED' | awk '{print $1}' | grep -Fqx "$session"; then
                return 0
            fi
        else
            case "$listing" in
                *"No active zellij sessions"*) return 0 ;;
            esac
        fi
        tries=$((tries + 1))
        sleep 0.5
    done
    return 1
}

muxrd_running() {
    "$BIN_DIR/muxrd" status >"$LOGS/muxrd-status.log" 2>&1
}

# muxrd names each backend it serves when it starts; the line goes to its
# start-up output before it detaches, or to its log.
zellij_backend_accepted() {
    muxrd_log=$(find "$HOME" -name muxrd.log | head -n 1)
    grep -q 'backend: zellij' "$LOGS/muxrd-start.log" ${muxrd_log:+"$muxrd_log"} && muxrd_running
}

# ── Build (with the caller's own environment) ────────────────────────────────

echo "building muxrd and its examples"
(cd "$ROOT" && cargo build --locked -p muxrd --bins --examples) || die "cargo build failed"
# cargo ran in the repository root, so a relative CARGO_TARGET_DIR is relative
# to it.
TARGET_DIR=$(cd "$ROOT" && mkdir -p "${CARGO_TARGET_DIR:-target}" && cd "${CARGO_TARGET_DIR:-target}" && pwd) \
    || die "cannot locate the cargo target directory"
BIN_DIR="$TARGET_DIR/debug"
EXAMPLES="$BIN_DIR/examples"
[ -x "$BIN_DIR/muxrd" ] || die "muxrd not found under $BIN_DIR"

# ── Isolation (before any zellij binary runs) ────────────────────────────────────────────────────────────────

for name in $(env | sed -n 's/^\(MUXRD_[A-Za-z0-9_]*\)=.*/\1/p'); do
    unset "$name"
done
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_RUNTIME_DIR
unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID ZELLIJ_CONFIG_DIR ZELLIJ_CONFIG_FILE
export HOME="$WORK/home"
export ZELLIJ_SOCKET_DIR="$WORK/sock"
export TERM=xterm-256color
export MUXRD_BACKEND=zellij
export MUXRD_BIND="127.0.0.1:$PORT"
export TMPDIR="$WORK/tmp"

# ── Binaries ─────────────────────────────────────────────────────────────────

if [ "$MODE" = single ]; then
    LABEL="zellij $VERSION"
    MAIN_ZELLIJ=$(zellij_binary "$VERSION") || exit 1
    ZELLIJ_BINS="$MAIN_ZELLIJ"
else
    LABEL="zellij mixed $OLD + $NEW"
    OLD_ZELLIJ=$(zellij_binary "$OLD") || exit 1
    MAIN_ZELLIJ=$(zellij_binary "$NEW") || exit 1
    ZELLIJ_BINS="$MAIN_ZELLIJ $OLD_ZELLIJ"
fi
export PATH="$(dirname "$MAIN_ZELLIJ"):$PATH"

# ── Sessions ─────────────────────────────────────────────────────────────────

if [ "$MODE" = single ]; then
    run_check "session demo starts ($VERSION)" start_session "$MAIN_ZELLIJ" demo
else
    # The older version runs first so that it, not the newer one, writes the
    # configuration in the fresh home directory.
    run_check "session old starts ($OLD)" start_session "$OLD_ZELLIJ" old
    run_check "session new starts ($NEW)" start_session "$MAIN_ZELLIJ" new
fi

# ── muxrd ────────────────────────────────────────────────────────────────────

"$BIN_DIR/muxrd" init >"$LOGS/muxrd-init.log" 2>&1 || { echo "FAIL muxrd init"; FAILED=1; }
"$BIN_DIR/muxrd" create-token -n rw >"$LOGS/token-rw.log" 2>&1
"$BIN_DIR/muxrd" create-token -n ro --read-only >"$LOGS/token-ro.log" 2>&1
RW=$(sed -n 's/^ *TOKEN: //p' "$LOGS/token-rw.log")
RO=$(sed -n 's/^ *TOKEN: //p' "$LOGS/token-ro.log")
CRT=$(find "$HOME" -name server.crt | head -n 1)
if [ -z "$RW" ] || [ -z "$RO" ] || [ -z "$CRT" ]; then
    echo "FAIL muxrd tokens and certificate"
    FAILED=1
fi

if "$BIN_DIR/muxrd" start --daemonize >"$LOGS/muxrd-start.log" 2>&1; then
    MUXRD_STARTED=1
fi
tries=0
while [ "$tries" -lt 20 ] && ! muxrd_running; do
    tries=$((tries + 1))
    sleep 0.5
done
run_check "muxrd starts with the zellij backend" zellij_backend_accepted

ADDR="https://127.0.0.1:$PORT"

action_check() {
    "$EXAMPLES/action_client" --cert "$CRT" --auth-token "$RW" --ro-auth-token "$RO" \
        --addr "$ADDR" --session "$1"
}

session_check() {
    "$EXAMPLES/session_client" --cert "$CRT" --auth-token "$RW" --ro-auth-token "$RO" \
        --addr "$ADDR" --session "$1"
}

attach_check() {
    if [ $# -gt 0 ]; then
        "$EXAMPLES/attach_client" --cert "$CRT" --auth-token "$RW" --addr "$ADDR" \
            --rows 30 --cols 100 --session "$1"
    else
        "$EXAMPLES/attach_client" --cert "$CRT" --auth-token "$RW" --addr "$ADDR" \
            --rows 30 --cols 100
    fi
}

# ── Checks ───────────────────────────────────────────────────────────────────

if [ "$MODE" = single ]; then
    run_check "action_client (pane operations, read-only gate)" action_check demo
    run_check "native client attached to demo" attach_native_client demo
    run_check "session_client (tabs, scroll, session lifecycle)" session_check demo
    stop_native_client
    run_check "attach_client (render frames)" attach_check
else
    run_check "action_client on old ($OLD)" action_check old
    run_check "attach_client on old ($OLD)" attach_check zellij:old
    run_check "action_client on new ($NEW)" action_check new
    run_check "attach_client on new ($NEW)" attach_check zellij:new

    # A session of the newer version is killed, which leaves its entry in
    # that version's session index; the older version then serves the same
    # name under a name-named socket.
    run_check "session stale starts ($NEW)" start_session "$MAIN_ZELLIJ" stale
    run_check "session stale killed ($NEW)" kill_session "$MAIN_ZELLIJ" stale
    run_check "session stale starts again ($OLD)" start_session "$OLD_ZELLIJ" stale
    run_check "action_client on stale ($OLD after $NEW)" action_check stale
fi

# ── Result ───────────────────────────────────────────────────────────────────

if [ "$FAILED" = 0 ]; then
    echo "RESULT: PASS $LABEL"
    exit 0
fi
echo "RESULT: FAIL $LABEL"
exit 1
