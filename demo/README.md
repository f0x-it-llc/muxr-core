# Muxr demo server

A locked-down, internet-exposable `muxrd` instance whose only purpose is to let
someone — a store reviewer, or anyone you hand the QR to — pair the Muxr mobile
app and drive a **real but heavily sandboxed** terminal.

This is **not** the [dev rig](../docker/README.md), which has passwordless root
SSH and is for local development only. Use *this* for anything reachable from
the internet.

The image installs the official prebuilt `muxrd` release via
[`install.sh`](../install.sh) — it does **not** build from source, so the build
context is just this directory and no Rust toolchain is needed.

## What someone can do

- Pair the app by scanning the QR (or entering the pairing URI).
- Attach to a live, bar-less zellij session with an editor / monitors / logs,
  **or** a herdr space — the demo serves **both backends**, so both session
  types appear in the app's session list.
- Read a real, read-only clone of this repository at `/opt/demo/muxr-core`:
  `git log`, `git show`, `rg`, `bat` and `nvim` all work against actual source.

## What they cannot do

| Control | Mechanism |
|---|---|
| Become root | Runs as uid `10001`; no `sudo`, no `su` (removed); every setuid/setgid bit stripped at build |
| Gain privileges via a child | `no-new-privileges:true` |
| Use Linux capabilities | `cap_drop: ALL` |
| Write system files | `read_only` rootfs |
| Edit the repo clone | Root-owned and `chmod a-w`; `nvim` opens it, `:w` fails |
| Run a dropped binary | `/tmp` is `tmpfs` mounted `noexec,nosuid,nodev` |
| Fork-bomb / exhaust the host | `pids_limit: 256`, `mem_limit`/`memswap_limit: 512m`, `cpus: 1.0`, `nproc: 256`, `nofile: 1024/2048` |
| See host processes | Container PID namespace |
| Reach an admin surface | No `sshd`, no `muxrctl` in the image; provisioning is headless |
| Pivot / exfiltrate over the network | Read-only visitors cannot run a command at all; an optional VM firewall rule (below) blocks egress for read-write tokens too |

## One token, many people

`muxrd` mints an independent session token on every `Login`, with no cap, so a
**single auth token serves many concurrent people by design**. Two tokens exist:

| Token | Posture | Default |
|---|---|---|
| `demo-public` | read-only — the token in the published QR | always minted |
| `demo-reviewer` | read-write — for store reviewers | only when `DEMO_REVIEWER_TOKEN=1` |

**The public token is read-only for a reason.** `muxrd` has no per-login session
isolation: everyone presenting the same token attaches to the *same* session and
sees the same activity. This is a shared window onto a live terminal, **not a
private sandbox** — do not describe it as one. Read-only viewers can navigate
tabs, panes and spaces, scroll, and resize their own view; they cannot type and
cannot send anything but wheel-scroll (no clicks or drags). On zellij, a tab is
sized to the *smallest* client focused on it, read-only viewers included — so
one visitor's small window does affect what everyone else on that tab sees,
though it's floored at a minimum size so it can't be shrunk to nothing. On
herdr, a read-only attach is an observer: its resize is accepted without
resizing the pane, and its scroll never reaches the terminal.

### Abuse kill switch

```bash
docker exec muxr-demo muxrd revoke-token demo-public
```

This invalidates the token **and every live session minted from it** in one
transaction. Restart afterwards to mint a fresh token, then republish the QR.

## What persists, what resets

Exactly one named volume (`demo-data` at `/var/lib/demo`) survives a restart:

- **Persists** — the TLS cert, key and SAN sidecar
  (`/var/lib/demo/zellij/muxrd/`), and the auth token DB
  (`/var/lib/demo/zellij/tokens.db`).
- **Resets** — all session state, `$HOME` (`/run/demo`), `/tmp`, and every edit
  anyone made.

Persisting the cert is what makes a PIN-mode QR *reusable*: the pairing URI
pins the certificate (`tm=pin&fp=<sha256>`), so a regenerated cert would
invalidate every QR already in circulation regardless of the token. In PROXIED
mode nothing is pinned and only the token DB matters — the volume layout is the
same, so switching modes keeps the token.

> **Changing `DEMO_HOST` regenerates the cert** (it changes the SANs) and
> therefore invalidates every published QR. Republish after any `DEMO_HOST`
> change.

`docker compose down -v` wipes the volume too — that is a full reset, and it
also invalidates published QRs.

## Operator notes

**The read-only flag is fixed when a token is minted.** There is no
`update-token` command, so flipping `DEMO_READ_ONLY` after first boot has *no
effect* on an existing token. To change posture, revoke the token and let the
next boot mint a fresh one — then republish the QR.

**Persisted token secrets are readable by anyone with an interactive shell.**
`muxrd create-token` prints a secret once and it cannot be recovered afterwards,
so the entrypoint stores it under `/var/lib/demo` at mode `0600`, owned by uid
`10001` — which is the same uid the visitor's shell runs as. With the shipped
defaults there is no exposure: `DEMO_READ_ONLY=1` means a public visitor cannot
send input at all, and `DEMO_REVIEWER_TOKEN=0` means the read-write secret is
never written. **But if you set both `DEMO_READ_ONLY=0` and
`DEMO_REVIEWER_TOKEN=1` on a publicly reachable host, a visitor can read
`demo-reviewer.token` — and revoking `demo-public` does not invalidate it.**
Avoid that combination in public.

**If you copy this persistence pattern for your own deployment, clear muxrd's
pidfile on boot.** `muxrd`'s pidfile and control socket live in the *same*
directory as its TLS cert, so persisting the cert also persists them. Since the
entrypoint `exec`s muxrd as PID 1 and container PID namespaces restart at 1, a
stale pidfile makes muxrd refuse to start — a permanent crash loop.
`entrypoint.sh` removes `muxrd.pid` and `control.sock` on every boot for exactly
this reason.

**`opencode` is installed but cannot reach a model.** The demo ships no API key
and makes no egress exception, so its onboarding screen is deliberate
sandboxing, not a broken product.

## Two TLS modes

| | PIN (default) | PROXIED |
|---|---|---|
| Who owns TLS | `muxrd`, self-signed | a reverse proxy with a publicly-trusted certificate |
| QR trust | `tm=pin&fp=<sha256>` — the app pins **this** cert | `tm=ca` — the app trusts the proxy's public cert |
| Network path | app → container directly; port published; DNS unproxied | app → proxy → container over plaintext h2c; port **not** published |
| Select with | nothing (`DEMO_PROXIED=0`) | `DEMO_PROXIED=1` + `DEMO_HOST` |
| Compose file | `compose.yaml` | `compose.proxied.yaml` |

They don't mix. A pinned QR fails behind any TLS-terminating proxy (the app sees
the proxy's cert, not the pinned one), and a `tm=ca` QR fails against a direct
self-signed server. This section and the firewall section below describe PIN
mode; see [Behind a reverse proxy](#behind-a-reverse-proxy)
for the other.

## Run it

Local smoke test (loopback only):

```bash
docker compose -f demo/compose.yaml up --build
```

Public VM (people dial `DEMO_HOST` — it goes into the TLS cert SAN):

```bash
DEMO_HOST=demo.muxr.app BIND_ADDR=0.0.0.0 \
  docker compose -f demo/compose.yaml up --build -d
```

Get the pairing details:

```bash
docker logs muxr-demo                          # banner + ANSI QR
docker exec muxr-demo cat /run/demo/pairing.txt
```

`pairing.txt` holds the host, fingerprint, token and the full
`muxr://pair?v=2…` URI for each minted token. Paste the `demo-public` URI (or a
QR of it) wherever people will scan it.

> If your Docker host has no container internet, build with
> `docker build --network=host` — some networks drop forwarded packets with a
> TTL below 64, which fails every download in the image build.

### Environment

| Var | Default | Meaning |
|---|---|---|
| `DEMO_HOST` | *(empty)* | Public IP/DNS people connect to — the QR's `h=`. PIN mode adds it to the cert SAN (comma-separate for several; **changing it invalidates published QRs**). **Required** in PROXIED mode. |
| `DEMO_PROXIED` | `0` | `1` = PROXIED mode: plaintext h2c to a TLS-terminating proxy, `tm=ca` QR. |
| `DEMO_PUBLIC_PORT` | `443` | PROXIED only: the proxy's public port — what the app dials and what goes in the QR. |
| `PROXY_NETWORK` | *(none)* | PROXIED only, **required**: the Docker network the reverse proxy is on; the demo joins it. |
| `BIND_ADDR` | `127.0.0.1` | PIN only: host interface the gRPC port publishes on. Set `0.0.0.0` on a public VM. |
| `GRPC_PORT` | `50051` | PIN only: published host port. |
| `DEMO_READ_ONLY` | `1` | `1` = read-only public posture. `0` = interactive (store reviewers). |
| `DEMO_REVIEWER_TOKEN` | `0` | `1` also mints the read-write `demo-reviewer` token. |
| `DEMO_SESSION` | `demo` | zellij session name. |
| `MUXR_VERSION` | `0.4.3` | muxr-core release installed. **Minimum 0.4.3** — see below. |
| `HERDR_VERSION` | `0.9.0` | herdr release; muxrd is tested against its wire protocol. |

> **`MUXR_VERSION` must be ≥ 0.4.3.** `muxrd` refuses to drive a zellij whose
> version differs from the `zellij-utils` it was linked against, and **silently
> drops the backend rather than failing** — the symptom is
> `backend: zellij not in served set` and a demo that serves herdr only. v0.4.1
> predates the zellij 0.45.1 rebaseline, so it links `zellij-utils` 0.44.3 and
> cannot drive this image's zellij. Below 0.4.3, a read-only viewer is pinned to
> a single fixed-size tab, so a public demo visitor has nothing to explore. If
> you bump the zellij pin, pair it with a muxr-core release built against the
> same zellij.

### What's inside

zellij **0.45.1** and herdr **0.9.0** (both backends served), Neovim **0.12.5**
with NvChad and pre-built treesitter parsers, `opencode` **1.18.29**, plus btop,
htop, ripgrep, fd, fzf, bat, tree, jq, ncdu, git, and a few toys.

> Neovim is installed from the official release tarball, **not apt**: NvChad
> needs ≥ 0.12 (its pinned branch pulls nvim-treesitter's rewritten `main`),
> while Debian trixie ships 0.10.4. Treesitter parsers are built into the image
> because the container has no runtime egress — a missing parser would surface
> as an error in front of a visitor.

## VM firewall (required) — PIN mode

The container hardens the workload; it does **not** firewall the VM. On the
host, allow only the gRPC port inbound and — if anyone untrusted holds a
read-write token — block the container's outbound egress so nobody can use the
box as a pivot or spam relay. (PROXIED mode has its own
firewall shape — see the next section.)

```bash
# Inbound: only the gRPC port.
sudo ufw default deny incoming
sudo ufw allow 50051/tcp
sudo ufw allow OpenSSH            # your admin access only

# Outbound egress from the demo bridge: drop everything (the container needs no
# outbound traffic once the image is built). Adjust to your network.
DEMO_NET=$(docker network inspect demo_demo -f '{{(index .IPAM.Config 0).Subnet}}')
sudo iptables -I DOCKER-USER -s "$DEMO_NET" -m conntrack --ctstate NEW -j DROP
```

## Behind a reverse proxy

Use this when the VM already fronts everything with a TLS-terminating proxy,
you don't want a bare port on the internet, or an edge/CDN proxy hides the
origin IP. PIN mode cannot do any of that — the app would see the proxy's
certificate and reject it — so the demo switches to **PROXIED** mode:

```
app ──TLS (edge proxy's public cert)──▶ edge proxy (gRPC enabled)
    ──TLS (your origin cert, HTTP/2)──▶ Traefik :443
    ──plaintext h2c, container net────▶ muxrd :50051  (NOT published)
```

`muxrd` runs with `--insecure-h2c --i-know-this-is-behind-a-proxy` and the QR
carries `tm=ca` for `DEMO_HOST:443` with no fingerprint. Nothing is pinned, so
cert durability stops mattering; the token still persists exactly as before.

**Edge proxy requirements** — if one sits in front of Traefik it must: proxy
gRPC over HTTPS (usually an explicit toggle); use end-to-end TLS to the origin,
not an HTTP-to-origin "flexible" mode (gRPC needs HTTP/2 over TLS to origin);
and speak HTTP/2 to the origin. 443 is the right public port.

**Deploy** — [`compose.proxied.yaml`](compose.proxied.yaml) is the deployment
file: no `build:`, no `ports:`, identical hardening, plus the Traefik route. Run
it with plain `docker compose` rather than swarm (`docker stack deploy` ignores
`read_only`, `cap_drop`, `pids_limit` and the rest of the hardening block), and
set `DEMO_HOST` and `PROXY_NETWORK` in its environment.

```bash
DEMO_HOST=demo.example.com PROXY_NETWORK=proxy docker compose -f demo/compose.proxied.yaml up -d
```

**Why the route is in the file, not a domain form.** A platform that generates
Traefik routes from a domain form — host, entrypoint, TLS, cert resolver, port —
has no gRPC/h2c option, and names the generated router and service with an
unpredictable slug, so `loadbalancer.server.scheme=h2c` cannot be attached to
it. Without h2c Traefik speaks HTTP/1.1 to muxrd and every gRPC call fails.
That single label is why this service declares its own route. Do **not** also
create a domain for it through such a form: that adds a second, non-h2c route
for the same host.

**Network.** The demo joins the proxy's network — `PROXY_NETWORK` names it and
the compose file declares it as external — exactly like any other service
behind the proxy. Traefik reaches muxrd with nothing to attach and nothing to
redo when the proxy container is recreated; `traefik.docker.network` pins that
network so Traefik does not pick one of its others.

**Firewall** — 22, 80 and 443 only; **no 50051**. If an edge proxy is meant to
hide the VM, restrict 80/443 to that proxy's published IP ranges, or anyone who
learns the IP walks around it.

**Egress block — optional, and only worth it when someone untrusted can type.**
With the shipped defaults nobody can: the public token is read-only, so no
visitor can run a command, and nothing in the image initiates outbound
connections on its own. The rule only buys something once a read-write token is
held by someone you would not want probing the proxy's network. When that
applies, drop every connection the demo *initiates* — internet, its neighbours
on that network, the proxy itself — while inbound from the proxy still works,
then prove it:

```bash
DEMO_IP=$(docker inspect muxr-demo -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}')
sudo iptables -I DOCKER-USER -s "$DEMO_IP" -m conntrack --ctstate NEW -j DROP
# from inside the demo, the proxy must now be unreachable
docker exec muxr-demo bash -c 'timeout 3 bash -c "</dev/tcp/<proxy-ip>/80" && echo LATERAL OPEN || echo blocked'
```

The IP can change when the container is recreated, so re-apply after a redeploy.

**Verify from outside** — this exercises the whole chain, edge proxy included:

```bash
grpcurl -import-path muxrd/proto -proto muxr.proto demo.example.com:443 muxr.v1.Muxr/GetVersion
docker exec muxr-demo cat /run/demo/pairing.txt      # tls mode: proxied, tm=ca, no fp=
```

**Trade-offs, decide them on purpose:**

- Whatever terminates TLS in front sees the terminal traffic and the bearer
  tokens in plaintext. Fine for a public read-only demo of a public repo; it is
  a different trust model from end-to-end pinning.
- Edge proxies commonly drop idle connections after a minute or two.
  `AttachTerminal` is a long-lived stream, so a viewer parked on a *silent* pane
  may be cut off; panes with continuous output (btop, htop, the git-log loop)
  should hold. **Test this on a device before publishing the QR.**
- The proxy → `muxrd` hop is plaintext h2c inside the container network. That
  is why the port must never be published and why the network is private.

## Publishing

[`demo-release.yml`](../.github/workflows/demo-release.yml) publishes
`ghcr.io/f0x-it-llc/muxr-demo` to GHCR. It is a **manual `workflow_dispatch`**,
not an on-push build — this image can be internet-reachable, so publishing is a
deliberate act. `latest` moves only from `main`; other refs publish
`sha-<commit>` only. A VM can then `docker compose pull && docker compose up -d`
instead of building.

Upgrading this way keeps the existing volume, so the published QR stays valid
across the upgrade — that is deliberate.

## Connecting a test client

Session ids on a multi-backend server are **backend-qualified** — `zellij:demo`,
`herdr:herdr` — as returned by `ListSessions`. The example clients discover this
themselves when `--session` is omitted:

```bash
docker exec muxr-demo cat /var/lib/demo/zellij/muxrd/server.crt > /tmp/demo.crt
cargo run --example read_client -- \
  --addr https://127.0.0.1:50051 --cert /tmp/demo.crt --auth-token <token>
```
