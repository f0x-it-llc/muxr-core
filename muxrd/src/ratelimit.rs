//! ratelimit — per-client-IP token-bucket rate limiting for the muxrd gRPC API.
//!
//! ## Why
//!
//! `Login` is a PUBLIC (unauthenticated) RPC and every request runs a token-DB
//! lookup + hash. Without a limiter an anonymous client can flood `Login` (or
//! spray invalid bearer tokens) and amplify blocking work onto the runtime. This
//! layer sits **outside** [`crate::auth::BearerAuthLayer`] (added first, so it is
//! the outermost middleware) and sheds excess requests *before* any auth/DB work
//! runs.
//!
//! ## Model
//!
//! A [token bucket](https://en.wikipedia.org/wiki/Token_bucket) per client IP:
//! each bucket holds up to `burst` tokens and refills at `rps` tokens/second.
//! Every request costs one token; a request that finds an empty bucket is
//! rejected with `RESOURCE_EXHAUSTED`. Bursts up to `burst` pass instantly;
//! sustained throughput is capped at `rps`.
//!
//! ## Peer identification
//!
//! The client IP comes from tonic's [`TcpConnectInfo`] request extension. Behind
//! a TLS-terminating reverse proxy (h2c mode) the peer is the proxy, so all
//! proxied clients share one bucket — a coarse global cap rather than true
//! per-client limiting. When the extension is absent the request falls back to a
//! single shared bucket keyed on the unspecified address.
//!
//! ## Tuning
//!
//! Defaults are generous for a real mobile client (which opens a few RPCs plus
//! one long-lived `AttachTerminal` stream) and can be overridden via env:
//! - `MUXRD_RATE_LIMIT_RPS`   — sustained requests/sec per IP (default 30)
//! - `MUXRD_RATE_LIMIT_BURST` — burst capacity per IP (default 60)
//! - `MUXRD_RATE_LIMIT_DISABLE=1` — disable the limiter entirely

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use dashmap::DashMap;
use tonic::Status;
use tonic::transport::server::TcpConnectInfo;
use tower_layer::Layer;
use tower_service::Service;

/// Beyond this many tracked IPs, an insert triggers a prune of idle (full)
/// buckets to bound memory. Real TCP peers can't spoof their source IP, so this
/// is only reached under a genuine many-host flood.
const MAX_TRACKED_IPS: usize = 65_536;

/// Sentinel key for requests whose peer IP could not be determined (e.g. h2c
/// behind a proxy that did not surface connect info).
const UNKNOWN_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

// ─── Config ─────────────────────────────────────────────────────────────────

/// Rate-limiter configuration, resolved from defaults + env overrides.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Sustained refill rate in tokens (requests) per second, per IP.
    pub rps: f64,
    /// Maximum burst — the bucket capacity, per IP.
    pub burst: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            rps: 30.0,
            burst: 60.0,
        }
    }
}

impl RateLimitConfig {
    /// Resolve config from env, falling back to defaults. Returns `None` when
    /// `MUXRD_RATE_LIMIT_DISABLE` is truthy (limiter disabled).
    pub fn from_env() -> Option<Self> {
        if env_truthy("MUXRD_RATE_LIMIT_DISABLE") {
            log::warn!("ratelimit: disabled via MUXRD_RATE_LIMIT_DISABLE");
            return None;
        }
        let d = Self::default();
        let rps = env_f64("MUXRD_RATE_LIMIT_RPS").unwrap_or(d.rps);
        let burst = env_f64("MUXRD_RATE_LIMIT_BURST").unwrap_or(d.burst);
        // Guard against nonsensical values that would disable the limit.
        let rps = if rps.is_finite() && rps > 0.0 {
            rps
        } else {
            d.rps
        };
        let burst = if burst.is_finite() && burst >= 1.0 {
            burst
        } else {
            d.burst
        };
        Some(Self { rps, burst })
    }
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0")
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

// ─── Bucket + limiter core ──────────────────────────────────────────────────

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Shared, cloneable rate-limiter state (one per server, held behind an `Arc`).
#[derive(Clone)]
pub struct RateLimiter {
    cfg: RateLimitConfig,
    buckets: Arc<DashMap<IpAddr, Bucket>>,
    /// Cheap sweep trigger: every `MAX_TRACKED_IPS` checks we consider pruning.
    checks: Arc<AtomicUsize>,
}

impl RateLimiter {
    /// Build a limiter with the given config.
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            cfg,
            buckets: Arc::new(DashMap::new()),
            checks: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Try to admit one request from `ip`. Returns `true` if a token was
    /// available (request allowed), `false` if the bucket is empty (reject).
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let admitted = {
            let mut entry = self.buckets.entry(ip).or_insert(Bucket {
                tokens: self.cfg.burst,
                last: now,
            });
            // Refill for the elapsed time, capped at burst.
            let elapsed = now.duration_since(entry.last).as_secs_f64();
            entry.tokens = (entry.tokens + elapsed * self.cfg.rps).min(self.cfg.burst);
            entry.last = now;
            if entry.tokens >= 1.0 {
                entry.tokens -= 1.0;
                true
            } else {
                false
            }
        };

        // Opportunistic memory bound: once the map is large, drop idle (full)
        // buckets. Only sweeps occasionally to keep the hot path cheap.
        if self
            .checks
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(MAX_TRACKED_IPS)
            && self.buckets.len() > MAX_TRACKED_IPS
        {
            let burst = self.cfg.burst;
            self.buckets.retain(|_, b| b.tokens < burst);
        }

        admitted
    }
}

// ─── Layer ──────────────────────────────────────────────────────────────────

/// Tower [`Layer`] that installs per-IP token-bucket rate limiting.
///
/// Install **before** [`crate::auth::BearerAuthLayer`] so it is the outermost
/// middleware (tonic applies the first-added layer outermost). When the limiter
/// is disabled (`MUXRD_RATE_LIMIT_DISABLE`) the layer is a transparent
/// pass-through, so the server builder's type is unaffected either way.
#[derive(Clone)]
pub struct RateLimitLayer {
    limiter: Option<RateLimiter>,
}

impl RateLimitLayer {
    /// Build the layer from the environment: an active limiter unless disabled.
    pub fn from_env() -> Self {
        let limiter = RateLimitConfig::from_env().map(|cfg| {
            log::info!(
                "ratelimit: enabled — {} req/s sustained, {} burst, per client IP",
                cfg.rps,
                cfg.burst
            );
            RateLimiter::new(cfg)
        });
        Self { limiter }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> RateLimitService<S> {
        RateLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Tower service produced by [`RateLimitLayer`].
///
/// `limiter` is `None` when rate limiting is disabled — then every request is
/// forwarded unconditionally.
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    limiter: Option<RateLimiter>,
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for RateLimitService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    ReqBody: Send + 'static,
    ResBody: http_body::Body + Default + Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = RateLimitFuture<S::Future, ResBody>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> RateLimitFuture<S::Future, ResBody> {
        // Disabled → transparent pass-through.
        let Some(limiter) = &self.limiter else {
            return RateLimitFuture::forward(self.inner.call(req));
        };

        let ip = req
            .extensions()
            .get::<TcpConnectInfo>()
            .and_then(|info| info.remote_addr())
            .map(|addr| addr.ip())
            .unwrap_or(UNKNOWN_IP);

        if limiter.allow(ip) {
            RateLimitFuture::forward(self.inner.call(req))
        } else {
            log::warn!("ratelimit: rejecting request from {ip} (bucket empty)");
            RateLimitFuture::reject(Status::resource_exhausted(
                "rate limit exceeded — slow down and retry",
            ))
        }
    }
}

// ─── Future ─────────────────────────────────────────────────────────────────

/// Future returned by [`RateLimitService`].
pub struct RateLimitFuture<F, B> {
    inner: RateLimitFutureKind<F, B>,
}

enum RateLimitFutureKind<F, B> {
    Forward(F),
    Reject(Option<Status>, std::marker::PhantomData<B>),
}

impl<F, B> RateLimitFuture<F, B> {
    fn forward(f: F) -> Self {
        Self {
            inner: RateLimitFutureKind::Forward(f),
        }
    }
    fn reject(status: Status) -> Self {
        Self {
            inner: RateLimitFutureKind::Reject(Some(status), std::marker::PhantomData),
        }
    }
}

impl<F, E, B> Future for RateLimitFuture<F, B>
where
    F: Future<Output = Result<http::Response<B>, E>>,
    B: http_body::Body + Default,
{
    type Output = Result<http::Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we project through a pin to the inner field; we never move it.
        let inner = unsafe { &mut self.get_unchecked_mut().inner };
        match inner {
            RateLimitFutureKind::Forward(f) => {
                // SAFETY: `f` is never moved after this point.
                unsafe { Pin::new_unchecked(f) }.poll(cx)
            }
            RateLimitFutureKind::Reject(status, _) => {
                // Same gRPC-over-HTTP/2 error encoding as BearerAuthLayer: an
                // HTTP 200 carrying the grpc-status trailer.
                let status = status.take().expect("polled after completion");
                let (http_parts, _) = status.into_http::<()>().into_parts();
                let resp = http::Response::from_parts(http_parts, B::default());
                Poll::Ready(Ok(resp))
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn allows_burst_then_rejects_when_empty() {
        let limiter = RateLimiter::new(RateLimitConfig {
            rps: 0.0001, // effectively no refill within the test window
            burst: 5.0,
        });
        let peer = ip(1);
        // First `burst` requests pass.
        for i in 0..5 {
            assert!(limiter.allow(peer), "request {i} within burst should pass");
        }
        // The next one is shed.
        assert!(
            !limiter.allow(peer),
            "request beyond burst should be rejected"
        );
    }

    #[test]
    fn buckets_are_per_ip() {
        let limiter = RateLimiter::new(RateLimitConfig {
            rps: 0.0001,
            burst: 1.0,
        });
        assert!(limiter.allow(ip(1)), "ip1 first request passes");
        assert!(!limiter.allow(ip(1)), "ip1 second request shed");
        // A different IP has its own full bucket.
        assert!(limiter.allow(ip(2)), "ip2 is independent");
    }

    #[test]
    fn refills_over_time() {
        let limiter = RateLimiter::new(RateLimitConfig {
            rps: 1000.0,
            burst: 1.0,
        });
        let peer = ip(3);
        assert!(limiter.allow(peer), "first passes");
        assert!(!limiter.allow(peer), "immediately empty");
        // After ~2ms at 1000 rps, ~2 tokens have refilled → allow again.
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(limiter.allow(peer), "should refill after a short wait");
    }

    #[test]
    fn config_from_env_disable() {
        // Not asserting env mutation here (tests share the process env); just
        // check the default is sane.
        let cfg = RateLimitConfig::default();
        assert!(cfg.rps > 0.0 && cfg.burst >= 1.0);
    }
}
