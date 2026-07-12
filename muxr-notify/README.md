# muxr-notify

A small, self-hostable push-notification relay for Muxr. It mints opaque
**push-handles** for devices, stores their FCM registration tokens, forwards
`muxrd`'s notify requests to Firebase Cloud Messaging (HTTP v1), and enforces
per-handle + per-IP rate limits. It never sees or stores anything about the
underlying terminal session — only a device platform, an FCM token, and a
notification `kind` + short text.

**muxr-notify is a separate, independently-deployed artifact from `muxrd`/
`muxrctl`.** It is a workspace member (so `cargo build --workspace` / `cargo
test` cover it) but it is **never** included in the muxr-core release suite
tarballs or `install.sh` — it ships only as its own Docker image
([`Dockerfile`](Dockerfile)).

## Why a separate artifact — the self-host reality

**A self-hosted relay only delivers push to apps you build yourself.** FCM
registration tokens are scoped to the Firebase project/Sender ID that issued
them, and `firebase_messaging` (the Flutter plugin) supports only the
default Firebase app — there's no way for a second, independently-run relay
with its own Firebase project to push to a token that some *other* project's
app instance registered.

Concretely:

| Deployment | Delivers to |
|---|---|
| The hosted instance (`noti.muxr.app`, our Firebase project) | ✅ the store-distributed app (Play Store / App Store) |
| Your self-hosted `muxr-notify`, your own Firebase project | ❌ the store app — ✅ **only a custom build of the app** using your own `google-services.json` |

If you self-host, point `muxrd` at your instance (`MUXRD_NOTIFY_RELAY_URL` /
`notify_relay_url`) and build the app yourself with matching Firebase
credentials. There is no supported way to make a self-hosted relay deliver to
the store-distributed app — this is an upstream FCM/Firebase constraint, not
a muxr-core limitation. (An upstream-forwarding mode — self-hosted relay →
`noti.muxr.app` → FCM, so store-app users can still self-host the first hop —
is a candidate for a future version; not implemented in v1.)

## Configuration (environment)

muxr-notify is configured entirely from the environment (no config file).

| Variable | Default | Purpose |
|---|---|---|
| `NOTIFY_LISTEN` | `127.0.0.1:8080` | Plain-HTTP listen address. TLS terminates at a fronting reverse proxy — this process never speaks TLS. |
| `NOTIFY_DB` | `./notify.db` | SQLite file path for the registration store. |
| `FCM_MODE` | `send` | `send` mints a real Google OAuth2 token and POSTs to FCM HTTP v1; `log` skips auth + the HTTP call and just logs the would-be message — no Firebase artifacts needed (used by the dev rig, CI, and the handler tests). |
| `FCM_SERVICE_ACCOUNT` | _(none)_ | Path to a Firebase service-account JSON. **Required when `FCM_MODE=send`** — startup fails fast otherwise. |
| `FCM_PROJECT_ID` | _(derived)_ | Firebase project id; derived from the service-account JSON's `project_id` when unset. |
| `NOTIFY_DAILY_CAP` | `150` | Max notifications/day per push-handle. |
| `NOTIFY_TRUST_PROXY` | `0` | When `1`, the client IP used for rate-limiting is taken from `X-Forwarded-For` (left-most entry) instead of the socket peer address. Set this when running behind a reverse proxy. |

## HTTP API (v1)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/healthz` | Liveness check, no auth. |
| `POST` | `/v1/register` | `{platform, fcm_token, existing_handle?}` → `{push_handle}`. `platform` is `"android"` in v1. Re-registering with `existing_handle` rotates the stored FCM token in place. |
| `POST` | `/v1/notify` | `{push_handle, kind, title?, body?}` → forwards to FCM (or logs, in `FCM_MODE=log`). 404 if the handle is unknown, 410 if pruned (FCM reported it unregistered), 429 on rate-limit/daily-cap. |
| `DELETE` | `/v1/registrations/{handle}` | Removes a registration (device opt-out / uninstall). |

The relay holds no entitlement/billing state — like the rest of muxr-core it
is licensing-agnostic; the app decides whether a user is allowed to register.

## Deploy recipe — behind a TLS-terminating proxy

muxr-notify only ever speaks plain HTTP; put a reverse proxy in front of it
for TLS, exactly like muxrd's own h2c recipe (see the root
[`README.md`](../README.md#tls-modes--deployment)):

```
phone/muxrd ─TLS/443─▶ notify.example.com ─▶ Traefik (LE cert, terminates TLS)
                                                  │ plain HTTP on internal net
                                                  ▼
                                          muxr-notify :8080
```

```bash
docker build -f muxr-notify/Dockerfile -t muxr-notify .

docker run -d --name muxr-notify \
  -p 127.0.0.1:8080:8080 \
  -v muxr-notify-data:/var/lib/muxr-notify \
  -e FCM_MODE=send \
  -e FCM_SERVICE_ACCOUNT=/run/secrets/fcm-service-account.json \
  -e NOTIFY_TRUST_PROXY=1 \
  -v /path/to/fcm-service-account.json:/run/secrets/fcm-service-account.json:ro \
  muxr-notify
```

Then point your reverse proxy at `127.0.0.1:8080` and `muxrd` at your public
URL: `MUXRD_NOTIFY_RELAY_URL=https://notify.example.com` (or the
`notify_relay_url` config.toml field). Mount a volume at
`/var/lib/muxr-notify` (the image's default `NOTIFY_DB` path) so registered
push-handles survive container recreation.

## Local development

```bash
# From the muxr-core workspace root
cargo build -p muxr-notify
cargo test -p muxr-notify

# Run against nothing but FCM_MODE=log (no Firebase artifacts needed):
FCM_MODE=log cargo run -p muxr-notify
```

## Dev rig

The Docker dev rig (`docker/compose.yaml`) starts an **in-container** instance
of muxr-notify (`FCM_MODE=log`, loopback-only) so `muxrd`'s notifier has a
local e2e target without any Firebase setup — see
[`docker/README.md`](../docker/README.md). That in-rig instance is a testing
convenience only; it is built from the same source but is **not** the
`muxr-notify/Dockerfile` deploy image described above.
