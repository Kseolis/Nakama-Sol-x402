//! HTTP request handlers.
//!
//! One file per endpoint family; the router in `lib.rs` wires routes.

pub mod computed_status;
pub mod top_up;

use axum::http::StatusCode;

/// Liveness probe. Per ADR-007 demo flow — no readiness gating, no RPC
/// roundtrip; container orchestrators that need readiness probes can
/// re-implement against this same router.
pub async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}
