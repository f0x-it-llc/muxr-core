# muxr-core

The open-source backend for **Muxr** — a Rust [Cargo workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html)
that lets a mobile client attach to and control remote terminal-multiplexer sessions
over a TLS, bearer-authenticated gRPC API. `muxrd` drives two backends behind a
`MuxBackend` trait — [zellij](https://zellij.dev/) and [herdr](https://herdr.dev/) — and auto-detects
whichever are available at startup.

Two distributed binaries, plus a third workspace crate distributed separately:

| Crate | Binary | What it does |
|-------|--------|--------------|
| [`muxrd`](muxrd/) | `muxrd` | gRPC server (protobuf package `muxr.v1`) that relays over a terminal multiplexer — zellij (Unix-domain IPC) or herdr (JSON-API + binary wire sockets). TLS (self-signed, an external CA cert, or plaintext h2c behind a proxy) + per-token auth, read-only tokens, daemonize. |
| [`muxrctl`](muxrctl/) | `muxrctl` | Terminal UI to install, configure, and pair the server: cert/SAN setup, token management, QR-code device pairing (fingerprint-pinned or system-CA), live status. Links `muxrd` as a library for its pure ops. |
| [`muxr-notify`](muxr-notify/) | `muxr-notify` | A small, self-hostable push-notification relay (mints device push-handles, forwards `muxrd`'s notify requests to FCM). **Not** part of the release suite above — it ships only as its own Docker image (see [`muxr-notify/README.md`](muxr-notify/README.md)), since a self-hosted relay only delivers push to apps you build yourself (FCM tokens are project-scoped). The dev rig runs an in-container instance (`FCM_MODE=log`) for e2e testing. |

## Install

Install the latest pre-built suite (both binaries) on Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/f0x-it-llc/muxr-core/main/install.sh | bash
```

This installs `muxrd` and `muxrctl` into `~/.local/bin` (override with
`MUXR_INSTALL_DIR`). Pin a version with `… | bash -s -- --version 0.1.0`.
Pre-built targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and
`aarch64-apple-darwin` (Apple Silicon). **Intel Macs** (`x86_64-apple-darwin`)
have no prebuilt binary — build from source with `cargo build --release`.
Windows is unsupported (the server requires a Unix host). The installer
verifies the downloaded archive against the release's published
`checksums-sha256.txt` (SHA-256) and fails closed on any mismatch.

## Build

```bash
cargo build --workspace          # all three binaries (debug)
cargo build --release --workspace  # all three binaries (release)
cargo build -p muxrd             # just the server
cargo build -p muxrctl           # just the TUI
cargo build -p muxr-notify       # just the push relay (separately distributed — see below)
```

## Run

```bash
# Generate the TLS cert, then serve:
cargo run -p muxrd -- init
MUXRD_SKIP_VERSION_CHECK=1 cargo run -p muxrd -- start

# The configure/pair TUI:
cargo run -p muxrctl
```

`start` opens a control socket for `status`/`stop`; add `--daemonize` to detach.
`MUXRD_SKIP_VERSION_CHECK=1` bypasses the Zellij version-match check.

For the **zellij** backend, requires the matching `zellij` binary on `PATH` (the
server pins a Zellij version and refuses to start against a different one). For the
**herdr** backend, requires a running `herdr` instance (wire protocol 14, e.g.
v0.7.1) — a separate, unmodified, user-installed binary (AGPL-3.0). muxrd
auto-detects which backends are available and serves all of them; pass `--backend`
/ `MUXRD_BACKEND` to restrict to one.

## TLS modes & deployment

The server resolves its TLS identity by precedence **h2c > external cert > self-signed**:

| Mode | Flags / env | Use for |
|------|-------------|---------|
| **Self-signed** (default) | _(none)_ — generated for `127.0.0.1` + `localhost` + any `--san` extras | Direct / LAN connections. The mobile client pins the cert's SHA-256 fingerprint, distributed out-of-band in the pairing QR. |
| **External cert** | `--tls-cert <pem> --tls-key <pem>` (or `MUXRD_TLS_CERT` / `MUXRD_TLS_KEY`) | Serving a real, publicly-trusted cert directly — Let's Encrypt, a Cloudflare Origin CA cert, or a corporate CA. The client trusts it via the system CA store; no pinning. Both files are validated at `init`/`start`. |
| **Plaintext h2c** | `--insecure-h2c` (or `MUXRD_H2C=1`) | Sitting behind a TLS-terminating reverse proxy (Traefik / Dokploy / Cloudflare) that owns the public cert. Serves **unencrypted** HTTP/2, so it **refuses a non-loopback bind** unless you also pass `--i-know-this-is-behind-a-proxy` (env `MUXRD_H2C_ALLOW_PUBLIC`). |

External and h2c are mutually exclusive with each other. `muxrd init` validates the
chosen mode (e.g. parses the external key) so misconfigurations surface before `start`.

`muxrctl` detects the active mode over the control socket and builds the pairing QR to match:
a **fingerprint-pinned** pairing (`tm=pin`) for self-signed, or a **system-CA** pairing (`tm=ca`,
no fingerprint) for external/h2c. Press **`t`** on the Cert screen to override the advertised trust
(**Auto → CA → Pin**) — needed when a *self-signed* origin sits behind a CA-terminating proxy. The
choice persists across restarts.

## Test

```bash
cargo test               # workspace unit + integration tests
cargo fmt                # format before committing
```

## Docker dev rig

`docker/` builds a self-contained container running the server against a
pre-populated Zellij session — or, via the opt-in `herdr` profile, against a herdr
workspace — useful for on-device testing from a phone on the same network. It
also starts an in-container `muxr-notify` instance (`FCM_MODE=log`, loopback
only) so push-notification flows are exercisable end-to-end without any
Firebase setup. See [`docker/README.md`](docker/README.md) and
[`muxr-notify/README.md`](muxr-notify/README.md) (muxr-notify's own deploy
image + self-host caveats).

```bash
docker compose -f docker/compose.yaml up --build
```

**Tailnet / LAN exposure:** set `BIND_ADDR` to the host IP you want to publish
on — the cert's SAN is automatically set to that IP so clients connecting on it
get a valid TLS cert. Override `MUXRD_SAN` explicitly to cover a
different or additional address (comma-separated).

```bash
# Publish + cert-valid on a tailnet IP:
BIND_ADDR=100.x.y.z docker compose -f docker/compose.yaml up --build
```

## gRPC contract

The wire contract is `muxrd/proto/muxr.proto` (package
`muxr.v1`). The server compiles it via `build.rs`; clients generate
their own stubs from the same file. A reference Dart client lives in
[`muxrd/clients/dart_test_client/`](muxrd/clients/dart_test_client/).

## Releasing

Releases are cut by the **Release** GitHub Actions workflow
(`.github/workflows/release.yml`), triggered manually from the Actions tab
(`workflow_dispatch`):

1. **Version** — auto-computed from conventional commits via
   [git-cliff](https://git-cliff.org/) (`cliff.toml`), or supply an explicit
   `version` input. The single source of truth is `[workspace.package].version`
   in the root `Cargo.toml`; all three crates inherit it (`muxr-notify`
   included, since it reports `CARGO_PKG_VERSION` too — but it is not part of
   what gets published, see below).
2. **Bump + tag** — the workflow updates the workspace version, commits
   `chore(release): … [skip ci]`, and pushes a `vX.Y.Z` tag to `main`.
3. **Build** — `muxrd` and `muxrctl` are compiled for three targets on native
   runners (Linux `x86_64`/`aarch64`, macOS `aarch64`) and packaged **by name**
   as one `muxr-core-v<ver>-<target>.tar.gz` suite archive per target. Intel
   macOS (`x86_64-apple-darwin`) is not built — see the Install section.
   `cargo build --release --target <triple>` compiles the whole workspace
   (so `muxr-notify` is built too, as a compile-time sanity check), but only
   `muxrd`/`muxrctl` are copied into the archive — `muxr-notify` is **never**
   part of the release suite; it ships only as its own Docker image (see
   [`muxr-notify/README.md`](muxr-notify/README.md)).
4. **Publish** — a GitHub Release is created with the suite archives,
   `checksums-sha256.txt`, `install.sh`, and a git-cliff changelog. `install.sh`
   only ever installs `muxrd`/`muxrctl`.

The version job pushes the bump commit + tag using the built-in `GITHUB_TOKEN`.
This requires `main` to accept pushes from `github-actions[bot]`; if `main` is
protected by a ruleset that blocks the bot, switch the `version` job's checkout
to a GitHub App token (set `vars.RELEASE_APP_ID` + `secrets.RELEASE_APP_PRIVATE_KEY`
and pass `token:` to `actions/checkout`).

The non-semver `fonts-v1` tag (font-catalog asset release) is excluded from
version computation via the anchored `tag_pattern` in `cliff.toml`.

## License

[MIT](LICENSE).
