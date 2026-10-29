//! auth — bearer-token auth for the muxrd gRPC API.
//!
//! ## Design
//!
//! This module provides a tower [`Layer`] (`BearerAuthLayer`) that wraps the
//! entire gRPC router at the HTTP level.  Because it intercepts raw
//! `http::Request<B>` values it can inspect the URI path (the gRPC method name
//! reconstructed from the HTTP/2 `:path` pseudo-header) to determine whether a
//! request must be authenticated.
//!
//! Why not a tonic `Interceptor`?  Tonic interceptors receive a
//! `tonic::Request<()>` whose metadata comes from HTTP headers only — **not**
//! from HTTP/2 pseudo-headers such as `:path`.  The RPC method name is
//! therefore invisible inside an interceptor.  A tower layer at the
//! `Server::builder().layer(...)` level sees the full `http::Request` and is
//! the canonical tonic approach for path-based middleware.
//!
//! ## Public RPCs (no auth required)
//!
//! - `/muxr.v1.Muxr/GetVersion`
//! - `/muxr.v1.Muxr/Login`
//!
//! All other paths require `authorization: Bearer <session_token>`.
//!
//! ## Extensions stashed on authenticated requests
//!
//! - [`SessionReadOnly`] — read-only flag of the session; mutating RPCs and
//!   `AttachTerminal` input enforce it.
//! - [`BearerAuthenticated`] — marker that auth passed (for handler assertions).
//! - [`SessionToken`] — the validated token, for periodic in-stream re-checks.
//!
//! [`TokenIdentity`] derives a stable, non-secret identity from that token for
//! maps that must be scoped per token (attach eviction) without keying on it.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tonic::Status;
use tower_layer::Layer;
use tower_service::Service;
use zellij_utils::web_authentication_tokens::{is_session_token_read_only, validate_session_token};

/// Request extension: read-only flag of the authenticated session token.
///
/// Mutating RPCs and `AttachTerminal` read this to refuse writes on read-only
/// sessions.
#[derive(Debug, Clone, Copy)]
pub struct SessionReadOnly(pub bool);

/// Request extension: marker that the BearerAuthLayer validated a token.
#[derive(Debug, Clone, Copy)]
pub struct BearerAuthenticated;

/// Request extension: the validated session token string.
///
/// Stashed so long-lived handlers (notably `AttachTerminal`'s relay) can
/// **re-validate** the bearer token periodically rather than trusting the
/// single check at stream open (review Major H).  Never logged.
#[derive(Clone)]
pub struct SessionToken(pub String);

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the token into logs.
        f.write_str("SessionToken(<redacted>)")
    }
}

/// A stable, non-secret identity of the bearer token that authenticated a
/// request — what scopes `AttachTerminal` eviction to one token
/// (`AttachReq.client_instance_id`, see `crate::relay::AttachRegistry`).
///
/// A domain-separated SHA-256 of the token string, so it is deterministic for
/// one token across requests and connections, distinct across tokens, and never
/// the raw secret itself: maps may key on it where they must not key on the
/// token. The domain tag keeps the digest from equalling any plain SHA-256 of
/// the token stored or computed elsewhere. Kept in memory only; its `Debug`
/// output is redacted all the same, so it never lands in a log line.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TokenIdentity([u8; 32]);

impl TokenIdentity {
    /// Domain-separation tag hashed ahead of the token.
    const DOMAIN: &'static [u8] = b"muxrd/attach-token-identity/v1\0";

    /// Derive the identity of `token` (the validated bearer string — the value
    /// [`SessionToken`] carries).
    pub fn of(token: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(Self::DOMAIN);
        hasher.update(token.as_bytes());
        Self(hasher.finalize().into())
    }
}

impl std::fmt::Debug for TokenIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenIdentity(<redacted>)")
    }
}

/// gRPC URI paths that require **no** bearer auth.
const PUBLIC_PATHS: &[&str] = &["/muxr.v1.Muxr/GetVersion", "/muxr.v1.Muxr/Login"];

// ─── Layer ────────────────────────────────────────────────────────────────────

/// Tower [`Layer`] that enforces bearer-token auth on non-public gRPC paths.
///
/// Install via `Server::builder().layer(BearerAuthLayer)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BearerAuthLayer;

impl<S> Layer<S> for BearerAuthLayer {
    type Service = BearerAuthService<S>;

    fn layer(&self, inner: S) -> BearerAuthService<S> {
        BearerAuthService { inner }
    }
}

// ─── Service ──────────────────────────────────────────────────────────────────

/// Tower service produced by [`BearerAuthLayer`].
#[derive(Debug, Clone)]
pub struct BearerAuthService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for BearerAuthService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    ReqBody: Send + 'static,
    ResBody: http_body::Body + Default + Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = BearerAuthFuture<S::Future, ResBody>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> BearerAuthFuture<S::Future, ResBody> {
        let path = req.uri().path().to_owned();

        // Public paths bypass auth entirely.
        if PUBLIC_PATHS.contains(&path.as_str()) {
            log::debug!("auth: public path {path} — skipping bearer check");
            return BearerAuthFuture::forward(self.inner.call(req));
        }

        // Extract and validate the bearer token.
        match check_bearer(req.headers()) {
            Ok((read_only, marker, token)) => {
                // Stash extensions so downstream handlers can read them.
                let (mut parts, body) = req.into_parts();
                parts.extensions.insert(SessionReadOnly(read_only));
                parts.extensions.insert(marker);
                parts.extensions.insert(SessionToken(token));
                let req = http::Request::from_parts(parts, body);
                log::debug!("auth: bearer OK for {path} (read_only={read_only})");
                BearerAuthFuture::forward(self.inner.call(req))
            }
            Err(status) => {
                log::info!("auth: bearer rejected for {path}: {}", status.message());
                BearerAuthFuture::reject(status)
            }
        }
    }
}

// ─── Future ───────────────────────────────────────────────────────────────────

/// Future returned by [`BearerAuthService`].
pub struct BearerAuthFuture<F, B> {
    inner: BearerAuthFutureKind<F, B>,
}

enum BearerAuthFutureKind<F, B> {
    Forward(F),
    Reject(Option<Status>, std::marker::PhantomData<B>),
}

impl<F, B> BearerAuthFuture<F, B> {
    fn forward(f: F) -> Self {
        Self {
            inner: BearerAuthFutureKind::Forward(f),
        }
    }
    fn reject(status: Status) -> Self {
        Self {
            inner: BearerAuthFutureKind::Reject(Some(status), std::marker::PhantomData),
        }
    }
}

impl<F, E, B> Future for BearerAuthFuture<F, B>
where
    F: Future<Output = Result<http::Response<B>, E>>,
    B: http_body::Body + Default,
{
    type Output = Result<http::Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we project through a pin to the inner field; we never move it.
        let inner = unsafe { &mut self.get_unchecked_mut().inner };
        match inner {
            BearerAuthFutureKind::Forward(f) => {
                // SAFETY: `f` is never moved after this point.
                unsafe { Pin::new_unchecked(f) }.poll(cx)
            }
            BearerAuthFutureKind::Reject(status, _) => {
                // Convert the Status into an HTTP 200 response with grpc-status trailer,
                // which is how gRPC signals errors over HTTP/2.
                let status = status.take().expect("polled after completion");
                let (http_parts, _) = status.into_http::<()>().into_parts();
                let resp = http::Response::from_parts(http_parts, B::default());
                Poll::Ready(Ok(resp))
            }
        }
    }
}

// ─── Token validation helper ──────────────────────────────────────────────────

/// Extract and validate `authorization: Bearer <token>` from HTTP headers.
///
/// Returns `(read_only, BearerAuthenticated, token)` on success, or a
/// `Status::unauthenticated` on any failure.  The token string is returned so
/// the layer can stash it for periodic re-validation (Major H).
fn check_bearer(headers: &http::HeaderMap) -> Result<(bool, BearerAuthenticated, String), Status> {
    let header = headers
        .get(http::header::AUTHORIZATION)
        .ok_or_else(|| Status::unauthenticated("missing authorization header"))?;

    let value = header
        .to_str()
        .map_err(|_| Status::unauthenticated("authorization header is not valid ASCII"))?;

    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| Status::unauthenticated("authorization header must use Bearer scheme"))?;

    if token.is_empty() {
        return Err(Status::unauthenticated("bearer token must not be empty"));
    }

    let valid = validate_session_token(token).map_err(|e| {
        log::warn!("auth: validate_session_token DB error: {e}");
        Status::unauthenticated("authentication error")
    })?;

    if !valid {
        return Err(Status::unauthenticated(
            "invalid or expired session token — call Login first",
        ));
    }

    // Major B — fail CLOSED: on any error determining the read-only flag we
    // default to `true` (read-only / most-restrictive).  A DB hiccup must never
    // silently grant write access to mutating RPCs.
    let read_only = is_session_token_read_only(token).unwrap_or_else(|e| {
        log::warn!("auth: is_session_token_read_only error (failing closed → read-only): {e}");
        true
    });

    Ok((read_only, BearerAuthenticated, token.to_owned()))
}
