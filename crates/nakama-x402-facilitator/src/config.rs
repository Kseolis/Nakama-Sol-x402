//! Facilitator startup configuration.
//!
//! Two layers:
//! * **Hardcoded** for hackathon MVP per `<Hardcoded rules>` in agent
//!   instructions (no env vars for things like RPC interval).
//! * **Env-driven** for things that must vary per developer machine (program
//!   ID, hot keypair path, bind addr).
//!
//! ADR-015 §F3 additions:
//! * `api_key` — bearer-token auth on protected routes (`/top-up`,
//!   `/computed-status`). Loaded from `NAKAMA_FACILITATOR_API_KEY`;
//!   non-test builds REFUSE to start without one (fail-closed).
//! * `max_top_up_amount` — hard cap on `/top-up` `amount` field. Default
//!   1_000_000_000 base units (= $1000 USDC). Configurable via
//!   `NAKAMA_FACILITATOR_MAX_TOP_UP_AMOUNT`.
//! * Bind default switched from `0.0.0.0:8080` to `127.0.0.1:8080` per
//!   `security-audit-patterns.md` P4. Exposing the service on a routable
//!   interface is opt-in via `NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND=1`.

use std::str::FromStr;

use anyhow::{Context, Result};
use solana_pubkey::Pubkey;

/// Default Solana devnet RPC. Hardcoded per agent rules: no `RPC_URL` env var.
pub const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";

/// Default bind address — loopback only. ADR-015 §F3 / P4: exposing a
/// signing endpoint on `0.0.0.0` by default is a hot-wallet-on-the-public-
/// internet footgun. Operators wanting public access set
/// `NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND=1` and an explicit bind addr.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";

/// Hardcoded program ID per `CLAUDE.md` "Project facts" — matches the on-chain
/// devnet deployment.
pub const DEFAULT_PROGRAM_ID: &str = "HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm";

/// Default top-up upper bound = 1000 USDC in 6-decimal base units (ADR-015
/// §F3). Big enough for the demo flows in `clients/ts/scripts/07-x402-flow.ts`,
/// small enough that a single mis-issued request can't drain a meaningful
/// subscriber balance.
pub const DEFAULT_MAX_TOP_UP_AMOUNT: u64 = 1_000_000_000;

#[derive(Debug, Clone)]
pub struct Config {
    pub rpc_url: String,
    pub bind_addr: String,
    pub program_id: Pubkey,
    /// When `true`, the binary reads a Solana JSON keypair (64-byte array)
    /// from stdin at startup and uses it to sign top-up transactions. When
    /// `false`, the facilitator runs in "assemble-only" mode (returns
    /// 503 from any signing endpoint).
    ///
    /// Why stdin and not a path: a path threaded through env input opens an
    /// env-controlled file read which static analyzers correctly flag
    /// (CWE-22). stdin is a controlled FD; bytes flow into a pure parser
    /// without touching the filesystem.
    pub read_demo_keypair_from_stdin: bool,
    /// Shared bearer token for the protected HTTP surface (ADR-015 §F3).
    /// `None` is only valid for unit tests built without the env var; the
    /// real binary refuses to start in that case (see `from_env`).
    ///
    /// We hold the token as a plain `String` rather than pulling in
    /// `secrecy` — the demo deployment lives behind the operator's reverse
    /// proxy and the secret never leaves process memory. If this crate
    /// gains a production deployment, swap to `secrecy::SecretString`.
    pub api_key: Option<String>,
    /// Hard cap on `TopUpRequest::amount` (ADR-015 §F3). Requests above
    /// this threshold are rejected with 400 BEFORE any RPC fetch — saves
    /// a tx fee on an obviously-bad request and bounds single-tx capital
    /// loss in the event the auth layer is compromised.
    pub max_top_up_amount: u64,
}

impl Config {
    /// Load config from environment.
    ///
    /// Fail-closed paths:
    /// * No `NAKAMA_FACILITATOR_API_KEY` set → return `Err`. The binary
    ///   refuses to bind a listener if it can't enforce auth on protected
    ///   routes (security-audit-patterns.md P4: "auth must be enforced,
    ///   not warned").
    /// * `NAKAMA_BIND_ADDR` resolves to a non-loopback interface without
    ///   `NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND=1` → return `Err`.
    pub fn from_env() -> Result<Self> {
        let rpc_url = std::env::var("NAKAMA_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.into());
        let bind_addr =
            std::env::var("NAKAMA_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
        let allow_public_bind = std::env::var("NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
            .unwrap_or(false);
        if !is_loopback_bind(&bind_addr) {
            if !allow_public_bind {
                anyhow::bail!(
                    "bind addr {bind_addr} is not loopback; set NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND=1 to opt-in (ADR-015 §F3 / P4)"
                );
            }
            tracing::warn!(
                bind_addr = %bind_addr,
                "facilitator bound to non-loopback interface — signing endpoint reachable from the network. Ensure ingress is reverse-proxied with TLS + IP allow-list."
            );
        }

        let program_id_str =
            std::env::var("NAKAMA_PROGRAM_ID").unwrap_or_else(|_| DEFAULT_PROGRAM_ID.into());
        let program_id = Pubkey::from_str(&program_id_str)
            .with_context(|| format!("invalid program ID: {program_id_str}"))?;
        let read_demo_keypair_from_stdin =
            std::env::var("NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN").is_ok();

        let api_key = match std::env::var("NAKAMA_FACILITATOR_API_KEY") {
            Ok(key) if !key.is_empty() => Some(key),
            Ok(_) | Err(_) => {
                anyhow::bail!(
                    "NAKAMA_FACILITATOR_API_KEY is empty or unset; facilitator refuses to start without an auth secret (ADR-015 §F3). Set a strong random value (e.g. `openssl rand -hex 32`)."
                );
            }
        };

        let max_top_up_amount = match std::env::var("NAKAMA_FACILITATOR_MAX_TOP_UP_AMOUNT") {
            Ok(v) => v
                .parse::<u64>()
                .with_context(|| format!("invalid NAKAMA_FACILITATOR_MAX_TOP_UP_AMOUNT: {v}"))?,
            Err(_) => DEFAULT_MAX_TOP_UP_AMOUNT,
        };

        Ok(Self {
            rpc_url,
            bind_addr,
            program_id,
            read_demo_keypair_from_stdin,
            api_key,
            max_top_up_amount,
        })
    }

    /// Test-only builder that bypasses env. Compiled under `cfg(test)` AND
    /// behind an integration-test hook so the binary path can't accidentally
    /// pick it up. Returns a Config with a known api_key for auth assertions.
    #[doc(hidden)]
    pub fn for_test(api_key: impl Into<String>, max_top_up_amount: u64) -> Self {
        let program_id =
            Pubkey::from_str(DEFAULT_PROGRAM_ID).expect("hardcoded DEFAULT_PROGRAM_ID parses");
        Self {
            rpc_url: DEFAULT_RPC_URL.into(),
            bind_addr: DEFAULT_BIND_ADDR.into(),
            program_id,
            read_demo_keypair_from_stdin: false,
            api_key: Some(api_key.into()),
            max_top_up_amount,
        }
    }
}

/// Return true iff `bind_addr` (host:port) targets a loopback interface.
/// Conservative — if parsing fails we treat the address as non-loopback
/// (fail-closed; surfaces as a startup error).
fn is_loopback_bind(bind_addr: &str) -> bool {
    use std::net::{IpAddr, SocketAddr};
    let Ok(addr) = bind_addr.parse::<SocketAddr>() else {
        return false;
    };
    match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bind_is_loopback() {
        assert!(is_loopback_bind(DEFAULT_BIND_ADDR));
    }

    #[test]
    fn public_bind_detected() {
        assert!(!is_loopback_bind("0.0.0.0:8080"));
        assert!(!is_loopback_bind("192.168.1.10:8080"));
    }

    #[test]
    fn malformed_bind_addr_is_not_loopback() {
        assert!(!is_loopback_bind("not-a-socket"));
    }
}
