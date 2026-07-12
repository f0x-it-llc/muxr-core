//! HTTP API integration tests. The router is driven directly via
//! `tower::ServiceExt::oneshot` (no live port); FCM runs in `log` mode so no
//! Firebase artifacts are needed. Each test gets a throwaway SQLite file.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use muxr_notify::config::{Config, FcmMode};
use muxr_notify::fcm::FcmSender;
use muxr_notify::handlers::{AppState, router};
use muxr_notify::ratelimit::RateLimiter;
use muxr_notify::store::Store;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

/// Build a router + a store handle (for direct manipulation) + tempdir guard.
fn app(daily_cap: u32) -> (Router, Store, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let config = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        db_path: db.to_str().unwrap().to_string(),
        fcm_mode: FcmMode::Log,
        service_account: None,
        project_id: None,
        daily_cap,
        trust_proxy: false,
    };
    let store = Store::open(&config.db_path).unwrap();
    let state = AppState {
        store: store.clone(),
        limiter: Arc::new(RateLimiter::new()),
        fcm: Arc::new(FcmSender::Log),
        config: Arc::new(config),
    };
    (router(state), store, dir)
}

fn post(uri: &str, json: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap()
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

async fn register(router: &Router, body: &str) -> (StatusCode, Value) {
    call(router, post("/v1/register", body)).await
}

async fn register_handle(router: &Router, token: &str) -> String {
    let (status, json) = register(
        router,
        &format!(r#"{{"platform":"android","fcm_token":"{token}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    json["push_handle"].as_str().unwrap().to_string()
}

async fn notify(router: &Router, body: &str) -> StatusCode {
    call(router, post("/v1/notify", body)).await.0
}

#[tokio::test]
async fn register_mints_64_hex_handle() {
    let (router, _s, _d) = app(150);
    let handle = register_handle(&router, "tokA").await;
    assert_eq!(handle.len(), 64);
    assert!(handle.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn reregister_with_existing_handle_keeps_it_and_updates_token() {
    let (router, store, _d) = app(150);
    let handle = register_handle(&router, "tok1").await;

    let (status, json) = register(
        &router,
        &format!(r#"{{"platform":"android","fcm_token":"tok2","existing_handle":"{handle}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["push_handle"].as_str().unwrap(), handle);

    let reg = store.get(handle).await.unwrap().unwrap();
    assert_eq!(reg.fcm_token, "tok2");
}

#[tokio::test]
async fn notify_happy_path_is_204() {
    let (router, _s, _d) = app(150);
    let handle = register_handle(&router, "tok").await;
    let status = notify(
        &router,
        &format!(
            r#"{{"push_handle":"{handle}","kind":"blocked","title":"Agent","body":"needs you"}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn notify_unknown_handle_is_404() {
    let (router, _s, _d) = app(150);
    let status = notify(&router, r#"{"push_handle":"deadbeef","kind":"blocked"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notify_pruned_handle_is_410() {
    let (router, store, _d) = app(150);
    let handle = register_handle(&router, "tok").await;
    store.mark_pruned(handle.clone()).await.unwrap();
    let status = notify(
        &router,
        &format!(r#"{{"push_handle":"{handle}","kind":"done"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn notify_burst_over_ten_per_min_is_429() {
    let (router, _s, _d) = app(150);
    let handle = register_handle(&router, "tok").await;
    let body = format!(r#"{{"push_handle":"{handle}","kind":"blocked"}}"#);
    for i in 0..10 {
        assert_eq!(
            notify(&router, &body).await,
            StatusCode::NO_CONTENT,
            "send {i}"
        );
    }
    assert_eq!(notify(&router, &body).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn notify_daily_cap_is_429() {
    let (router, _s, _d) = app(3);
    let handle = register_handle(&router, "tok").await;
    let body = format!(r#"{{"push_handle":"{handle}","kind":"blocked"}}"#);
    for _ in 0..3 {
        assert_eq!(notify(&router, &body).await, StatusCode::NO_CONTENT);
    }
    assert_eq!(notify(&router, &body).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn oversized_body_is_413() {
    let (router, _s, _d) = app(150);
    let huge = "x".repeat(2000);
    let body = format!(r#"{{"push_handle":"{huge}","kind":"blocked"}}"#);
    let status = notify(&router, &body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn bad_platform_is_400() {
    let (router, _s, _d) = app(150);
    let (status, _) = register(&router, r#"{"platform":"ios","fcm_token":"tok"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_is_idempotent_204() {
    let (router, _s, _d) = app(150);
    let handle = register_handle(&router, "tok").await;
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/registrations/{handle}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
    // After delete the handle is unknown.
    let status = notify(
        &router,
        &format!(r#"{{"push_handle":"{handle}","kind":"done"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_is_200() {
    let (router, _s, _d) = app(150);
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
