//! HTTP API error model.
//!
//! `thiserror` for the crate-internal enum (per agent rules: thiserror inside
//! crates, anyhow only at binary entry). Each variant carries a stable
//! machine-readable `code` string surfaced in the JSON response, plus a
//! human-readable message. HTTP status mapping is centralized in
//! `IntoResponse`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

use nakama_client::AccountDecodeError;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    /// Subscriber keypair not loaded; signing-required endpoint.
    #[error("signing not available: facilitator started without demo keypair")]
    SigningUnavailable,

    /// On-chain account exists but its bytes don't decode to the expected layout.
    #[error("account decode error: {0}")]
    Decode(#[from] AccountDecodeError),

    /// Solana RPC call failed.
    #[error("rpc error: {0}")]
    Rpc(String),

    /// Anything else — programmer error or unexpected runtime fault.
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    code: &'a str,
}

impl ApiError {
    fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::SigningUnavailable => "signing_unavailable",
            Self::Decode(_) => "decode_error",
            Self::Rpc(_) => "rpc_error",
            Self::Internal(_) => "internal_error",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::SigningUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Decode(_) => StatusCode::BAD_GATEWAY,
            // Both rpc and internal errors are 502/500. We prefer 502 for RPC
            // because the upstream is not us.
            Self::Rpc(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code();
        let msg = self.to_string();
        // Log at error level for 5xx; warn for 4xx. Never log full
        // request bodies or signer keys.
        if status.is_server_error() {
            tracing::error!(%status, %code, error = %msg, "api error");
        } else {
            tracing::warn!(%status, %code, error = %msg, "api error");
        }
        let body = ErrorBody { error: &msg, code };
        (status, Json(body)).into_response()
    }
}

/// Convert solana RPC client errors. We deliberately stringify here rather
/// than pulling in the full `ClientError` taxonomy — the public API surface
/// is just "RPC failed, here's the message", and matching on RPC error
/// kinds is out of scope for the demo (no retry-with-backoff per agent
/// rules).
impl From<solana_rpc_client_api::client_error::Error> for ApiError {
    fn from(e: solana_rpc_client_api::client_error::Error) -> Self {
        Self::Rpc(e.to_string())
    }
}
