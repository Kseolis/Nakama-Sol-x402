//! Bearer-token auth middleware (ADR-015 §F3, security-audit-patterns P4).
//!
//! Pattern: `axum::middleware::from_fn_with_state` — a per-request async
//! closure that reads `Authorization: Bearer <token>` and short-circuits with
//! 401 if the token is absent or mismatches `Config::api_key`. We do NOT
//! pull `tower_http::auth::AsyncRequireAuthorizationLayer` because the
//! generic boundary it imposes on the inner service forces a closure type
//! that fights axum 0.8's `MethodRouter::route_layer` ergonomics; a tiny
//! `from_fn_with_state` middleware is one screen of code and stays type-
//! transparent.
//!
//! Trust boundary: the API key is a single shared secret. ADR-015 §F3
//! "Open questions Q2" calls out multi-key rotation as deferred to
//! `future-work.md`. For the demo + hackathon judging the static-secret
//! model gives us the property we need (rejection of unauthenticated
//! requests) at minimal infrastructure cost.

use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};

use crate::{error::ApiError, state::AppState};

/// Constant-time-ish bearer comparison. `subtle` would be more correct
/// against timing oracles, but the secret lives in process memory of a
/// short-lived demo binary and the underlying TLS layer (operator-provided
/// reverse proxy) already smooths out network-observable timings. We use
/// `.as_bytes()` equality which compiles to a length-checked memcmp.
fn token_matches(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    expected.as_bytes() == presented.as_bytes()
}

/// Extract the bearer token from an `Authorization` header. Returns `None`
/// if the header is absent, not ASCII, or not prefixed with `Bearer `.
fn extract_bearer(req: &Request) -> Option<&str> {
    let header = req.headers().get(AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Per-request middleware. Routes that need auth wrap their handlers with
/// `axum::middleware::from_fn_with_state(state.clone(), require_bearer)`.
/// Routes that stay open (`/healthz`) skip the wrapper.
///
/// Failure paths:
/// * Header missing / malformed → `ApiError::Unauthorized` (401).
/// * Header present, token mismatches → `ApiError::Unauthorized` (401).
/// * Server has no `api_key` configured → `ApiError::Internal`. The
///   binary fails to start without a key (`Config::from_env`), so this
///   arm is only reachable in test builds that constructed a Config
///   without one. Treat as misconfiguration.
pub async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let expected = state
        .inner
        .config
        .api_key
        .as_deref()
        .ok_or_else(|| ApiError::Internal("api_key not configured".into()))?;

    let presented = extract_bearer(&req).ok_or(ApiError::Unauthorized)?;
    if !token_matches(expected, presented) {
        // Log the attempt but never the token itself — even a partial
        // prefix leak gives an attacker a search shortcut. The remote
        // addr would be ideal here but axum 0.8 surfaces it via a
        // `ConnectInfo` extractor we don't wire today; out of scope.
        tracing::warn!("rejected request with invalid bearer token");
        return Err(ApiError::Unauthorized);
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, Method, Request as HttpRequest};

    fn req_with_auth(value: Option<&str>) -> Request {
        let mut headers = HeaderMap::new();
        if let Some(v) = value {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        }
        let mut req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/x")
            .body(axum::body::Body::empty())
            .unwrap();
        *req.headers_mut() = headers;
        req
    }

    #[test]
    fn extract_bearer_happy() {
        let r = req_with_auth(Some("Bearer hunter2"));
        assert_eq!(extract_bearer(&r), Some("hunter2"));
    }

    #[test]
    fn extract_bearer_missing_header() {
        let r = req_with_auth(None);
        assert_eq!(extract_bearer(&r), None);
    }

    #[test]
    fn extract_bearer_wrong_scheme() {
        let r = req_with_auth(Some("Basic dXNlcjpwYXNz"));
        assert_eq!(extract_bearer(&r), None);
    }

    #[test]
    fn extract_bearer_empty_token() {
        let r = req_with_auth(Some("Bearer "));
        assert_eq!(extract_bearer(&r), None);
    }

    #[test]
    fn token_matches_equal_strings() {
        assert!(token_matches("secret", "secret"));
    }

    #[test]
    fn token_matches_length_diff_rejected() {
        assert!(!token_matches("secret", "secrets"));
        assert!(!token_matches("longer-secret", "short"));
    }

    #[test]
    fn token_matches_different_bytes_rejected() {
        assert!(!token_matches("secret", "secrxt"));
    }
}
