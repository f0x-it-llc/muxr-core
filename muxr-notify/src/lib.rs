//! muxr-notify — a small self-hostable push relay.
//!
//! It mints capability push-handles for devices, stores their FCM tokens,
//! enforces per-handle and per-IP rate limits, and forwards notify requests to
//! FCM (HTTP v1). TLS terminates at a fronting proxy; the listener is plain HTTP.
//!
//! See `docs`/the task spec for the API contract; entitlement/billing state lives
//! only in the app — muxr-notify (like the rest of muxr-core) is
//! licensing-agnostic.

pub mod config;
pub mod fcm;
pub mod handlers;
pub mod ratelimit;
pub mod store;

use anyhow::{Context, Result};
use log::info;
use std::net::SocketAddr;
use std::sync::Arc;

use config::Config;
use fcm::FcmSender;
use handlers::AppState;
use ratelimit::RateLimiter;
use store::Store;

/// Resolve config from the environment, build state, and serve until shutdown.
pub async fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = Config::from_env()?;
    let store = Store::open(&config.db_path)
        .with_context(|| format!("opening store at '{}'", config.db_path))?;
    let fcm = FcmSender::build(&config)?;
    let listen = config.listen;

    let state = AppState {
        store,
        limiter: Arc::new(RateLimiter::new()),
        fcm: Arc::new(fcm),
        config: Arc::new(config),
    };

    let app = handlers::router(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    info!("muxr-notify listening on {listen}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serving")?;
    Ok(())
}
