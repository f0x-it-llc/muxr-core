//! HTTP API v1: axum router, shared state, and request handlers.

use crate::config::Config;
use crate::fcm::{FcmSender, SendOutcome, token_prefix};
use crate::ratelimit::RateLimiter;
use crate::store::Store;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BODY_BYTES: usize = 1024;
const MAX_TITLE_CHARS: usize = 100;
const MAX_BODY_CHARS: usize = 512;
const MAX_TOKEN_CHARS: usize = 512;

/// Shared, cheaply-cloneable application state.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub limiter: Arc<RateLimiter>,
    pub fcm: Arc<FcmSender>,
    pub config: Arc<Config>,
}

/// Build the v1 router. Callers serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()` so peer IPs are visible.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/register", post(register))
        .route("/v1/notify", post(notify))
        .route("/v1/registrations/{handle}", delete(delete_registration))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

// ─── Client-IP extraction ────────────────────────────────────────────────────

/// The resolved client IP for rate-limiting. Infallible: falls back to `unknown`
/// when the source is absent (e.g. router driven directly in tests).
pub struct ClientIp(pub String);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        let xff = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        Ok(ClientIp(resolve_client_ip(
            state.config.trust_proxy,
            xff,
            peer,
        )))
    }
}

/// Resolve the client IP used for per-IP rate-limiting.
///
/// When `trust_proxy` is enabled we read `X-Forwarded-For`. A conforming fronting
/// proxy *appends* the address of the peer it accepted the connection from to any
/// client-supplied XFF list, so the **right-most** entry is the only one it
/// vouches for — the left-most entries are attacker-controlled and MUST be
/// ignored (trusting them lets a client mint arbitrary IP keys and bypass the
/// per-IP limits). Exactly **one** trusted hop is assumed. On an absent, blank,
/// or unparseable header — or when `trust_proxy` is off — we fall back to the
/// socket peer address, then to `"unknown"`.
fn resolve_client_ip(trust_proxy: bool, xff: Option<&str>, peer: Option<String>) -> String {
    let trusted = if trust_proxy {
        xff.and_then(|s| s.split(',').next_back())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    } else {
        None
    };
    trusted.or(peer).unwrap_or_else(|| "unknown".to_string())
}

// ─── Wire types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub platform: String,
    pub fcm_token: String,
    #[serde(default)]
    pub existing_handle: Option<String>,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub push_handle: String,
}

#[derive(Deserialize)]
pub struct NotifyRequest {
    pub push_handle: String,
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn healthz() -> StatusCode {
    StatusCode::OK
}

async fn register(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<RegisterRequest>,
) -> Response {
    if !st.limiter.allow_register(&ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    // v1 is Android-only.
    if req.platform != "android" {
        return (StatusCode::BAD_REQUEST, "unsupported platform").into_response();
    }
    if req.fcm_token.trim().is_empty() || req.fcm_token.chars().count() > MAX_TOKEN_CHARS {
        return (StatusCode::BAD_REQUEST, "invalid fcm_token").into_response();
    }

    let new_handle = mint_handle();
    match st
        .store
        .register(
            req.platform,
            req.fcm_token,
            req.existing_handle,
            new_handle,
            unix_now(),
        )
        .await
    {
        Ok(handle) => {
            info!("registered handle={}", token_prefix(&handle));
            (
                StatusCode::OK,
                Json(RegisterResponse {
                    push_handle: handle,
                }),
            )
                .into_response()
        }
        Err(e) => {
            error!("register failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn notify(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<NotifyRequest>,
) -> Response {
    // Per-IP throttle first — before any store/mutex lookup — to bound the
    // single-mutex DoS surface.
    if !st.limiter.allow_request(&ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let title_len = req.title.as_deref().map_or(0, |s| s.chars().count());
    let body_len = req.body.as_deref().map_or(0, |s| s.chars().count());
    if title_len > MAX_TITLE_CHARS || body_len > MAX_BODY_CHARS {
        return (StatusCode::BAD_REQUEST, "title/body too long").into_response();
    }

    let reg = match st.store.get(req.push_handle.clone()).await {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("notify lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if reg.pruned {
        return StatusCode::GONE.into_response();
    }

    let now = unix_now();
    let today = day_bucket(now);
    if reg.day_bucket == today && reg.sends_today >= st.config.daily_cap {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if !st.limiter.allow_notify(&reg.handle) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    match st
        .fcm
        .send(
            &reg.fcm_token,
            &req.kind,
            req.title.as_deref(),
            req.body.as_deref(),
        )
        .await
    {
        Ok(SendOutcome::Delivered) => {
            if let Err(e) = st.store.record_send(reg.handle, today, now).await {
                error!("record_send failed: {e:#}");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(SendOutcome::Unregistered) => {
            if let Err(e) = st.store.mark_pruned(reg.handle).await {
                error!("mark_pruned failed: {e:#}");
            }
            StatusCode::GONE.into_response()
        }
        Ok(SendOutcome::UpstreamError) => StatusCode::BAD_GATEWAY.into_response(),
        Err(e) => {
            error!("fcm send error: {e:#}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn delete_registration(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(handle): Path<String>,
) -> Response {
    // Per-IP throttle first — before any store/mutex lookup.
    if !st.limiter.allow_request(&ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match st.store.delete(handle).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            error!("delete failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Mint a 32-byte OS-CSPRNG capability handle, hex-encoded (64 chars).
fn mint_handle() -> String {
    use rand::RngCore;
    use std::fmt::Write;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(64);
    for b in buf {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// UTC day-index bucket (`unix_secs / 86400`) as a string key.
fn day_bucket(now: i64) -> String {
    (now / 86_400).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_handle_is_64_hex() {
        let h = mint_handle();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(mint_handle(), h, "handles must be unique");
    }

    #[test]
    fn xff_takes_rightmost_trusted_entry() {
        // Client spoofs the left-most entries; the trusted proxy appended the
        // real peer address on the right — that is the one we must use.
        let ip = resolve_client_ip(
            true,
            Some("1.1.1.1, 2.2.2.2, 9.9.9.9"),
            Some("10.0.0.1".to_string()),
        );
        assert_eq!(ip, "9.9.9.9", "spoofed left-most entries must be ignored");
    }

    #[test]
    fn xff_single_entry_is_trimmed() {
        let ip = resolve_client_ip(true, Some("  9.9.9.9  "), Some("10.0.0.1".to_string()));
        assert_eq!(ip, "9.9.9.9");
    }

    #[test]
    fn xff_ignored_when_not_trusting_proxy() {
        let ip = resolve_client_ip(false, Some("1.1.1.1"), Some("10.0.0.1".to_string()));
        assert_eq!(ip, "10.0.0.1", "socket peer wins when proxy is untrusted");
    }

    #[test]
    fn xff_absent_or_blank_falls_back_to_peer() {
        let peer = || Some("10.0.0.1".to_string());
        assert_eq!(resolve_client_ip(true, None, peer()), "10.0.0.1");
        assert_eq!(resolve_client_ip(true, Some(""), peer()), "10.0.0.1");
        assert_eq!(resolve_client_ip(true, Some("   "), peer()), "10.0.0.1");
    }

    #[test]
    fn no_peer_and_no_xff_is_unknown() {
        assert_eq!(resolve_client_ip(true, None, None), "unknown");
        assert_eq!(resolve_client_ip(false, Some("1.1.1.1"), None), "unknown");
    }
}
