# Muxr dev rig (Docker)

A Debian container running the **Muxr backend** — `muxrd` (the gRPC
server) and `muxrctl` (the configure/pair TUI) — pre-loaded with a realistic
Zellij session and a full set of terminal tools so the mobile client has a real,
interesting target. You SSH into the container and drive everything with
`muxrctl`.

**Zellij is pinned to v0.44.3** (the version muxrd was compiled against;
it refuses to start on any other version).

> The rig defaults to the **zellij** backend. To exercise muxrd's **herdr**
> backend instead, use the opt-in herdr profile — see
> [Herdr backend (opt-in)](#herdr-backend-opt-in) below.

## What's inside

- **muxrd** + **muxrctl** (static musl binaries built from the Cargo
  workspace) — the TLS gRPC server (port **50051**, self-signed cert +
  bearer-token auth) and the TUI that configures and starts it.
- **OpenSSH server** (port **22**) — root login so you can attach and run `muxrctl`.
- **muxr-notify** (loopback only, not published to the host) — the push-notification
  relay, started in-container in `FCM_MODE=log` (no Firebase artifacts needed) so
  `muxrd`'s notifier has a local e2e target. Set `NOTIFY_ENABLED=0` to skip it. See
  [`muxr-notify/README.md`](../muxr-notify/README.md) — it is a **separate,
  independently-deployed artifact**, never part of the muxr-core release suite.
- **Zellij v0.44.3** running a pre-populated `backend-dev` session (see `layout.kdl`):
  an `editor` tab (nvim + shell + btop), a `shell` tab (shell + htop), and a
  `logs` tab (live log stream).
- Terminal tooling: **Neovim + NvChad**, **btop**, htop, lazygit, ripgrep, fd,
  fzf, bat, tree, jq, ncdu, tmux, git, node/npm, python3, plus toys.

## Quickstart — loopback (local testing)

```bash
# From the repo root — publishes 127.0.0.1:50051 (gRPC) + 127.0.0.1:2222 (SSH).
docker compose -f docker/compose.yaml up --build
# or via the helper:
./docker/run.sh
```

The container boots a zellij session + sshd and prints a banner. SSH in (a TTY
is required for the TUI — note the `-t`) and start the server with `muxrctl`:

```bash
ssh -t root@127.0.0.1 -p 2222      # no password
muxrctl                          # Configure → Cert → Tokens → Server → Pair
```

In `muxrctl`: generate the cert, create a token, **start** the server, then
open **Pair** to scan the QR from the app (or copy the token + cert manually).

## Herdr backend (opt-in)

The same rig can drive muxrd's **herdr** backend instead of zellij. This is gated
behind a Docker Compose `herdr` profile so the default rig is unaffected and never
downloads herdr:

```bash
# Loopback (local testing):
./docker/run.sh --herdr
# …or compose directly:
docker compose -f docker/compose.yaml --profile herdr up --build muxrd-herdr
# LAN/phone: add --host / BIND_ADDR exactly like the zellij rig.
```

This builds the `runtime-herdr` image (a pinned, **unmodified** upstream herdr
binary — `HERDR_VERSION`, default `0.7.1`), starts a headless `herdr server`,
seeds a demo workspace, and exports `MUXRD_BACKEND=herdr` so the `muxrctl`-started
daemon selects herdr automatically. Then SSH in and drive `muxrctl` exactly as for
zellij (Configure → Cert → Tokens → **Server (start)** → Pair). The container is
**`muxr-herdr-rig`**.

> **herdr is pinned to a wire-protocol-14 release (v0.7.1)** — the version muxrd's
> herdr backend targets. muxrd asserts the version on the wire handshake and fails
> clearly on a mismatch, so do not bump `HERDR_VERSION` without confirming muxrd's
> `HERDR_PROTOCOL_VERSION`.

> **AGPL-3.0:** herdr is a separate, unmodified, user-installed binary that muxrd
> drives only over its public `0600` Unix sockets. The rig downloads the official
> upstream release for **local** dev use (it is not bundled into the default image,
> modified, or redistributed). muxrd stays the TLS/bearer boundary; herdr runs
> same-user/same-host.

> **What you'll see:** herdr per-terminal attach streams the **focused** pane's
> content (not zellij's all-panes composite); switching panes/tabs in the app
> re-attaches to that pane. This is expected for the herdr backend. herdr has no
> floating layer, and pane write/resize/scroll over the ephemeral path return
> "unsupported" (input/resize/scroll flow through the live attach stream instead).

The two rigs publish the **same host ports** — run one at a time. herdr keeps its
own state under the `herdr-data` + `muxrd-herdr-data` volumes (separate from the
zellij rig's). To inspect/drive herdr directly inside the container:

```bash
docker exec muxr-herdr-rig herdr status server
docker exec muxr-herdr-rig herdr workspace list
```

## LAN / phone access

To test from a real Android phone, publish on a **LAN IP** reachable by both the
host and the phone (same network):

```bash
# Replace with the host's actual LAN IP
LAN_IP="192.168.1.50"

# Helper wrapper (publishes gRPC + SSH on that interface)
BIND_ADDR="${LAN_IP}" ./docker/run.sh --host "${LAN_IP}"
# …or compose directly
BIND_ADDR="${LAN_IP}" docker compose -f docker/compose.yaml up --build
```

`BIND_ADDR` controls which host interface the ports are published on (default
`127.0.0.1` — loopback, nothing exposed). When you run `muxrctl`, pick that
same LAN IP in **Configure** so it lands in the cert's **Subject Alternative
Name** — the phone validates the self-signed cert against the IP it connects to.

## Connecting the mobile client / Dart test client

| Field | Value |
|-------|-------|
| Host  | the host/IP you published on (e.g. `127.0.0.1` or your LAN IP) |
| Port  | `50051` |
| Token | created in `muxrctl` → Tokens (or via the CLI below) |
| TLS   | self-signed cert — pair via QR, trust the PEM, or use the app's insecure-dev mode |

## Auth token & TLS cert — CLI fallback

`muxrctl` is the intended path, but you can also use the server CLI directly
(over SSH, or via `docker exec`). The container is **`muxr-grpc-rig`**;
locally (your user in the `docker` group) no `sudo` is needed, otherwise prefix
every `docker` command with `sudo`.

```bash
# Mint a token (prints a fresh UUID on stdout):
docker exec muxr-grpc-rig muxrd create-token --name mytoken
# read-only variant:
docker exec muxr-grpc-rig muxrd create-token --name viewer --read-only

# List token names + read-only flag (does NOT print the secret):
docker exec muxr-grpc-rig muxrd list-tokens

# The self-signed TLS cert (PEM) — for clients that pin/trust it:
docker exec muxr-grpc-rig \
  cat /root/.local/share/zellij/muxrd/server.crt > /tmp/rig-server.crt
```

## Shell into the container / view the live Zellij session

The pre-populated session is **`backend-dev`** (the `SESSION` env var). You're
already in over SSH; to watch / drive the exact session the mobile client sees:

```bash
# Attach a real Zellij client (TERM must be set):
env TERM=xterm-256color zellij attach backend-dev
# Detach (leave it running):  Ctrl-o then d

# Inspect without attaching a full client (no geometry impact):
zellij --session backend-dev action list-tabs
zellij --session backend-dev action list-panes
```

> ⚠️ **Heads-up for single-pane on-device testing:** attaching your own Zellij client
> adds a second client to the session. Zellij sizes the session to the **smallest**
> attached client, so your terminal's size will resize what the phone sees; and the
> server's single-pane **fullscreen is gated to the sole-client case**, so it is
> disabled while you're attached. **Detach (`Ctrl-o d`) when done** to restore the
> phone's view.

## Running the Dart / gRPC test client

```bash
# From muxrd/clients/dart_test_client/
dart run bin/muxr_client.dart \
  --host 127.0.0.1 --port 50051 \
  --token <paste-token-here> \
  --cert /tmp/rig-server.crt
```

## Using `read_client` (Rust example)

```bash
# From muxrd/
cargo run --example read_client -- \
  --addr 127.0.0.1:50051 \
  --auth-token <token> \
  --cert /tmp/rig-server.crt
```

## run.sh flags

```
./docker/run.sh [--host <IP>] [-- EXTRA_COMPOSE_ARGS...]
```

| Flag          | Default     | Description                                  |
|---------------|-------------|----------------------------------------------|
| `--host <IP>` | `127.0.0.1` | Publish the gRPC + SSH ports on this address |

## Notes

- **TLS mode:** this rig runs the **self-signed + QR-fingerprint-pinned** path (the direct/LAN
  case). The server *also* supports serving an external CA cert (`--tls-cert`/`--tls-key`) or
  running plaintext **h2c** behind a TLS-terminating proxy (`--insecure-h2c`) for domain/proxied
  deployments — see [TLS modes & deployment](../README.md#tls-modes--deployment) in the main README.
  Those modes are not exercised by this rig.
- **Named volumes** persist the token DB, cert, and zellij config across
  restarts. Use `docker compose down -v` to reset everything.
- **SSH:** passwordless root login (the entrypoint clears root's password —
  dev-rig only). `SSH_PORT` changes the published SSH port (default `2222`).
- **Zellij version:** must remain 0.44.3. Upgrading zellij without recompiling
  muxrd will cause a version-mismatch error at startup.
- **muxr-notify:** `NOTIFY_ENABLED` (default `1`) and `NOTIFY_PORT` (default `8090`)
  control the in-container relay. It always runs `FCM_MODE=log` in the rig — no
  real FCM send happens, the would-be message is logged (`docker exec <container>
  cat /var/log/muxr-notify.log`). Its SQLite store persists under the same
  `zellij-data`/`muxrd-*-data` volume as the rest of muxrd's state.
- **Security:** this is a **dev/test rig** — the self-signed cert, the
  passwordless SSH root login, and the `BIND_ADDR` LAN exposure are intentional
  dev affordances. Do not expose this container on an untrusted network.
