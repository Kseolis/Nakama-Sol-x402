//! ADR-015 §F3 regression: bearer-token auth on protected routes + top-up
//! amount cap enforcement.
//!
//! We drive the router via `tower::ServiceExt::oneshot` so the test doesn't
//! bind a real socket or talk to Solana RPC. Protected handlers reach for
//! RPC only AFTER the auth + amount-cap gates fire, so the missing RPC
//! mock isn't a problem for these scenarios — the asserted responses are
//! 401 (no/wrong token) and 400 (amount > cap), neither of which fetches
//! anything.
//!
//! What this test does NOT cover:
//! * Full happy-path RPC flow — that's `clients/ts/scripts/07-x402-flow.ts`
//!   against devnet.
//! * Owner-check (F5 decode_owned) — see `f5_decoder_owner_check.rs`.
//!
//! Crates pulled in `dev-dependencies` of the facilitator: `tower` for
//! ServiceExt, `http-body-util` for body collection. `serde_json` is
//! already a workspace dep.

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
};
use nakama_x402_facilitator::{router, AppState, Config};
use tower::ServiceExt;

const TEST_API_KEY: &str = "test-bearer-secret-xyz";
const TEST_MAX_TOP_UP: u64 = 5_000_000_000; // 5000 USDC

async fn build_state() -> AppState {
    let cfg = Config::for_test(TEST_API_KEY, TEST_MAX_TOP_UP);
    AppState::new(cfg, None)
        .await
        .expect("AppState constructs without RPC roundtrip")
}

fn json_top_up_body(amount: u64) -> Body {
    let payload = serde_json::json!({ "amount": amount });
    Body::from(payload.to_string())
}

/// Any valid base58 32-byte pubkey works — the router parses `sub_pda` from
/// the path before any handler-level RPC happens. We use the program ID
/// constant for convenience.
const SUB_PDA_STR: &str = "HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm";

#[tokio::test]
async fn top_up_without_auth_header_returns_401() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/top-up"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_top_up_body(1_000_000))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "unauthorized");
}

#[tokio::test]
async fn top_up_with_wrong_bearer_returns_401() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/top-up"))
                .header(header::AUTHORIZATION, "Bearer not-the-real-secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_top_up_body(1_000_000))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn top_up_with_wrong_scheme_returns_401() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/top-up"))
                // Basic auth value — middleware only honours `Bearer`.
                .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_top_up_body(1_000_000))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn computed_status_without_auth_header_returns_401() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/computed-status"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn healthz_is_open_without_auth() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn top_up_amount_above_cap_returns_400() {
    // Valid auth, valid keypair (None — but cap check fires BEFORE the
    // SigningUnavailable gate per top_up.rs ordering). Amount > cap.
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/top-up"))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_top_up_body(TEST_MAX_TOP_UP + 1))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "bad_request");
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("max_top_up_amount"),
        "error message should reference the cap, got: {msg}"
    );
}

#[tokio::test]
async fn top_up_amount_zero_returns_400() {
    // Pre-existing behaviour — preserved by F3 changes.
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/subscriptions/{SUB_PDA_STR}/top-up"))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_top_up_body(0))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn responses_carry_defence_headers() {
    let app = router(build_state().await);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers();
    assert!(
        headers.contains_key(header::STRICT_TRANSPORT_SECURITY),
        "HSTS header must be set"
    );
    assert_eq!(
        headers
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("nosniff"),
    );
}
